//! 别的电脑用 `sa share` 共享过来的目录。
//!
//! 那台电脑上的 `sa share` 经隧道来这里取活（长轮询），在它自己那边执行文件工具和命令，再把结果交回来。
//! 它每次来取活都说一声手上还在做哪几件；交出去了、不在里面、又没交回的，就是丢了。
//! 人点了停止、活已经交出去了的话，下一次取活时交给它一件「停下」。
//! 电脑没连着时，要用这个目录的那一步就等着，连上之后接着做；人点了停止就不等了。

use async_trait::async_trait;
use sa_protocol::{Share, ShareCall, SharePoll, ShareReply, MAX_SHARE_NAME_CHARS};
use sa_tools::{Permission, Tool, ToolCtx, ToolError, ToolRegistry, ToolResult};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{oneshot, Notify};
use tokio_util::sync::CancellationToken;

/// 在共享目录那台电脑上执行的工具。
const SHARED_TOOLS: [&str; 7] = ["read", "list_dir", "glob", "grep", "write", "edit", "shell"];
/// 交给 `sa share` 的「停下」：`id` 是要停的那件活。
pub const STOP: &str = "_stop";
/// 只读的共享里能用的工具。
pub const READ_ONLY_TOOLS: [&str; 4] = ["read", "list_dir", "glob", "grep"];
/// 这么久没来取活，就算那台电脑断开了。
const ONLINE_GRACE: Duration = Duration::from_secs(40);
const TAKEN: &str = "这个共享已经被另一个 sa share 接手了";
/// 交出去的「停下」过了这么久那边还说在跑，就再交一次（上一次可能半路丢了）。
const STOP_RESEND: Duration = if cfg!(test) {
    Duration::from_millis(200)
} else {
    Duration::from_secs(5)
};

#[derive(Default)]
pub struct Shares {
    state: Mutex<State>,
    /// 有新活、或者共享换了人：叫醒在等的 `sa share`。
    wake: Notify,
}

#[derive(Default)]
struct State {
    next_id: u64,
    shares: HashMap<String, Entry>,
}

#[derive(Default)]
struct Entry {
    /// 空的话是还没连上来过（比如服务重启后、电脑还没重连），只是有活在等它。
    session: String,
    root: String,
    read_only: bool,
    /// 那边允许执行命令时用的命令行。
    shell: Option<String>,
    /// 该 `sa share` 客户端是否支持远端工位双向审批、实时插话与常驻会话池回收。
    interactive: bool,
    /// 已经交出去、人又点了停止的活，等着告诉那边停下。
    stops: VecDeque<u64>,
    /// 已经叫过停、那边还说在跑的活：上次叫停是什么时候。
    stopping: HashMap<u64, Instant>,
    /// 发给正在运行中的远端工位的实时控制指令（审批答复、运行中插话）。
    controls: VecDeque<ShareCall>,
    /// 正挂着等活的请求有几个。
    polling: usize,
    last_seen: Option<Instant>,
    /// 还没交出去的活，先来先做。
    queue: VecDeque<u64>,
    calls: HashMap<u64, Call>,
    /// 被顶替过的运行：它们再来取活也不让回来，免得两个 `sa share` 来回抢。
    retired: std::collections::HashSet<String>,
}

impl Entry {
    /// 叫那边停下一件交出去了的活。
    fn stop(&mut self, id: u64) {
        self.controls.retain(|c| c.id != id);
        if !self.stops.contains(&id) {
            self.stops.push_back(id);
        }
    }

    /// 叫过停的活那边不跑了就不管了；还在跑、又过了一阵的，再叫一次。
    fn resend_stops(&mut self, running: &[u64]) {
        self.stopping.retain(|id, _| running.contains(id));
        let due: Vec<u64> = self
            .stopping
            .iter()
            .filter(|(_, at)| at.elapsed() >= STOP_RESEND)
            .map(|(id, _)| *id)
            .collect();
        for id in due {
            self.stop(id);
        }
    }

    fn online(&self) -> bool {
        self.polling > 0 || self.last_seen.is_some_and(|at| at.elapsed() < ONLINE_GRACE)
    }

    /// 交出去了、那边说不在做、又没交回：只读的重新排到最前面再做一次；改文件的不知道做没做，照实告诉 Bot。
    fn requeue_lost(&mut self, running: &[u64]) {
        let mut lost: Vec<u64> = self
            .calls
            .iter()
            .filter(|(id, call)| call.sent && !running.contains(id))
            .map(|(id, _)| *id)
            .collect();
        lost.sort_unstable();
        for id in lost.into_iter().rev() {
            let call = self.calls.get_mut(&id).expect("刚找到的");
            if call.writes {
                self.controls.retain(|c| c.id != id);
                let call = self.calls.remove(&id).expect("刚找到的");
                let _ = call.reply.send(Err(if call.tool == "shell" {
                    "和共享目录那台电脑的连接断了，不知道这条命令跑完没有。请先看一下结果再决定要不要重跑。"
                } else if call.tool == crate::share_desk::RUN {
                    "和共享目录那台电脑的连接断了，不知道 harness 跑完没有。请先看一下它的独立目录再决定要不要重做。"
                } else {
                    "和共享目录那台电脑的连接断了，不知道这次改动做完没有。请先看一下文件再决定要不要重做。"
                }
                .into()));
            } else {
                call.sent = false;
                self.queue.push_front(id);
            }
        }
    }
}

struct Call {
    tool: String,
    input: Value,
    writes: bool,
    sent: bool,
    reply: oneshot::Sender<Result<String, String>>,
    /// 做到一半报回来的进展交给这里（跑 harness 时）。
    progress: Option<UnboundedSender<String>>,
}

/// 共享的名字：去掉两头空白，不能为空，不能太长。
pub fn share_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("共享目录要有名字".into());
    }
    if name.chars().count() > MAX_SHARE_NAME_CHARS {
        return Err(format!(
            "共享目录的名字不能超过 {MAX_SHARE_NAME_CHARS} 个字"
        ));
    }
    Ok(name.to_string())
}

impl Shares {
    /// 连上来过的共享，按名字排。
    pub fn list(&self) -> Vec<Share> {
        let state = self.state.lock().unwrap();
        let mut shares: Vec<Share> = state
            .shares
            .iter()
            .filter(|(_, entry)| !entry.session.is_empty())
            .map(|(name, entry)| Share {
                name: name.clone(),
                root: entry.root.clone(),
                read_only: entry.read_only,
                online: entry.online(),
                shell: entry.shell.clone(),
            })
            .collect();
        shares.sort_by(|a, b| a.name.cmp(&b.name));
        shares
    }

    /// 最近一次知道的：这个共享是不是只读。没连上来过就当能改，到那边再说。
    pub(crate) fn read_only(&self, name: &str) -> bool {
        let state = self.state.lock().unwrap();
        state.shares.get(name).is_some_and(|entry| entry.read_only)
    }

    /// 那边允许执行命令时用的命令行。
    pub(crate) fn shell(&self, name: &str) -> Option<String> {
        let state = self.state.lock().unwrap();
        state.shares.get(name).and_then(|entry| entry.shell.clone())
    }

    /// 那台电脑现在连着没有。
    pub(crate) fn online(&self, name: &str) -> bool {
        let state = self.state.lock().unwrap();
        state.shares.get(name).is_some_and(Entry::online)
    }

    /// 那台电脑上的 `sa share` 是否支持双向审批、实时插话与远端常驻会话池。
    pub(crate) fn interactive(&self, name: &str) -> bool {
        let state = self.state.lock().unwrap();
        state
            .shares
            .get(name)
            .is_some_and(|entry| entry.interactive && entry.shell.is_some() && !entry.read_only)
    }

    /// 向正在运行中的远端工位调用发送一条即时控制消息（如 `_approval` / `_steer`）。
    /// 当目标调用已结束或共享端不支持交互控制时返回 `false`。
    pub(crate) fn send_control(&self, name: &str, call_id: u64, tool: &str, input: Value) -> bool {
        let queued = {
            let mut state = self.state.lock().unwrap();
            let Some(entry) = state.shares.get_mut(name) else {
                return false;
            };
            if entry.session.is_empty() || !entry.interactive || !entry.calls.contains_key(&call_id)
            {
                return false;
            }
            entry.controls.push_back(ShareCall {
                id: call_id,
                tool: tool.to_string(),
                input,
            });
            true
        };
        if queued {
            self.wake.notify_waiters();
        }
        queued
    }

    /// `sa share` 来取活：有就马上给，没有就最多等 `wait`，还没有就给 `None`。
    pub async fn poll(
        &self,
        hello: SharePoll,
        wait: Duration,
    ) -> Result<Option<ShareCall>, String> {
        let name = share_name(&hello.name)?;
        if hello.session.is_empty() {
            return Err("缺少 session".into());
        }
        // 新连上来的第一次取活：没活就马上回，让它知道已经连上了。
        let fresh;
        {
            let mut state = self.state.lock().unwrap();
            let entry = state.shares.entry(name.clone()).or_default();
            if entry.retired.contains(&hello.session) {
                return Err(TAKEN.into());
            }
            fresh = entry.session != hello.session;
            if fresh {
                let old = std::mem::replace(&mut entry.session, hello.session.clone());
                if !old.is_empty() {
                    entry.retired.insert(old);
                }
                entry.polling = 0;
                entry.stops.clear();
                entry.stopping.clear();
                entry.controls.clear();
                if !entry.root.is_empty() && entry.root != hello.root {
                    // 同一个名字换成了另一个目录：之前要做的（尤其批准过的改动）是冲着原来那个目录的。
                    entry.queue.clear();
                    for (_, call) in entry.calls.drain() {
                        let _ = call.reply.send(Err(if call.sent && call.writes {
                            format!("共享目录「{name}」换成了另一个目录。这一步已经交给原来的目录，可能已经改了那边的文件。")
                        } else {
                            format!("共享目录「{name}」换成了另一个目录，这一步没有执行。")
                        }));
                    }
                }
            }
            entry.root = hello.root;
            entry.read_only = hello.read_only;
            entry.shell = hello.shell.filter(|_| !hello.read_only);
            entry.interactive = hello.interactive && !hello.read_only && entry.shell.is_some();
            entry.last_seen = Some(Instant::now());
            entry.requeue_lost(&hello.running);
            entry.resend_stops(&hello.running);
            // 共享改成只读、或者不让执行命令了：还在排队的那些不再交出去。
            let refused: Vec<(u64, &str)> = entry
                .calls
                .iter()
                .filter(|(_, call)| !call.sent)
                .filter_map(|(id, call)| {
                    if call.writes && entry.read_only {
                        Some((*id, "这个共享目录是只读的，不能改。"))
                    } else if crate::share_desk::needs_commands(&call.tool) && entry.shell.is_none()
                    {
                        Some((
                            *id,
                            "共享目录那台电脑不让执行命令（sa share 没加 --commands）。",
                        ))
                    } else {
                        None
                    }
                })
                .collect();
            for (id, reason) in refused {
                entry.queue.retain(|queued| *queued != id);
                if let Some(call) = entry.calls.remove(&id) {
                    let _ = call.reply.send(Err(reason.into()));
                }
            }
            entry.polling += 1;
        }
        // 叫醒旧的那个（如果有）：让它知道被顶替了。
        self.wake.notify_waiters();
        let _polling = Polling {
            shares: self,
            name: &name,
            session: &hello.session,
        };
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = self.state.lock().unwrap();
                let Some(entry) = state.shares.get_mut(&name) else {
                    return Err(TAKEN.into());
                };
                if entry.session != hello.session {
                    return Err(TAKEN.into());
                }
                if let Some(id) = entry.stops.pop_front() {
                    entry.stopping.insert(id, Instant::now());
                    return Ok(Some(ShareCall {
                        id,
                        tool: STOP.into(),
                        input: Value::Null,
                    }));
                }
                let mut deferred = VecDeque::new();
                while let Some(control) = entry.controls.pop_front() {
                    match entry.calls.get(&control.id) {
                        Some(call) if call.sent => {
                            while let Some(prev) = deferred.pop_back() {
                                entry.controls.push_front(prev);
                            }
                            return Ok(Some(control));
                        }
                        Some(_) => deferred.push_back(control),
                        None => {}
                    }
                }
                entry.controls = deferred;
                while let Some(id) = entry.queue.pop_front() {
                    if let Some(call) = entry.calls.get_mut(&id) {
                        call.sent = true;
                        return Ok(Some(ShareCall {
                            id,
                            tool: call.tool.clone(),
                            input: call.input.clone(),
                        }));
                    }
                }
            }
            if fresh || tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Ok(None);
            }
        }
    }

    /// `sa share` 交回结果。
    pub fn reply(&self, reply: ShareReply) -> Result<(), String> {
        let name = share_name(&reply.name)?;
        let mut state = self.state.lock().unwrap();
        // 服务重启过就不认识这个共享了：交回的结果没人等，收下就是。
        let Some(entry) = state.shares.get_mut(&name) else {
            return Ok(());
        };
        if entry.session != reply.session {
            return Err(TAKEN.into());
        }
        entry.last_seen = Some(Instant::now());
        if reply.progress {
            if let Some(progress) = entry.calls.get(&reply.id).and_then(|c| c.progress.as_ref()) {
                let _ = progress.send(reply.text);
            }
            return Ok(());
        }
        if let Some(call) = entry.calls.remove(&reply.id) {
            entry.controls.retain(|c| c.id != reply.id);
            let result = if reply.ok {
                Ok(reply.text)
            } else {
                Err(reply.text)
            };
            let _ = call.reply.send(result);
        }
        Ok(())
    }

    /// 在共享目录里调用一个工具：交给那台电脑去做，等它交回。电脑没连着就一直等，直到 `cancel`。
    async fn call(
        &self,
        name: &str,
        tool: &str,
        input: Value,
        writes: bool,
        cancel: &CancellationToken,
    ) -> ToolResult {
        self.start_call(name, tool, input, writes, None)
            .wait(cancel)
            .await
    }

    /// 交给那台电脑一件会改东西的事（harness 的独立目录），等它交回。做到一半的进展交给 `progress`。
    pub(crate) async fn ask(
        &self,
        name: &str,
        tool: &str,
        input: Value,
        cancel: &CancellationToken,
        progress: Option<UnboundedSender<String>>,
    ) -> Result<String, String> {
        self.start_ask(name, tool, input, progress)
            .wait(cancel)
            .await
    }

    /// 启动一次远端工位调用并立刻返回带 `call_id` 的句柄，供运行期间双向发送 `_approval` / `_steer` 控制消息。
    pub(crate) fn start_ask<'a>(
        &'a self,
        name: &'a str,
        tool: &str,
        input: Value,
        progress: Option<UnboundedSender<String>>,
    ) -> AskHandle<'a> {
        AskHandle(self.start_call(name, tool, input, true, progress))
    }

    fn start_call<'a>(
        &'a self,
        name: &'a str,
        tool: &str,
        input: Value,
        writes: bool,
        progress: Option<UnboundedSender<String>>,
    ) -> CallHandle<'a> {
        let (reply, answer) = oneshot::channel();
        let id = {
            let mut state = self.state.lock().unwrap();
            if state.next_id == 0 {
                state.next_id = first_id();
            }
            state.next_id += 1;
            let id = state.next_id;
            let entry = state.shares.entry(name.to_string()).or_default();
            entry.calls.insert(
                id,
                Call {
                    tool: tool.to_string(),
                    input,
                    writes,
                    sent: false,
                    reply,
                    progress,
                },
            );
            entry.queue.push_back(id);
            id
        };
        self.wake.notify_waiters();
        CallHandle {
            id,
            writes,
            answer,
            pending: Pending {
                shares: self,
                name,
                id,
            },
        }
    }
}

pub(crate) struct AskHandle<'a>(CallHandle<'a>);

impl AskHandle<'_> {
    pub(crate) fn id(&self) -> u64 {
        self.0.id
    }

    pub(crate) async fn wait(self, cancel: &CancellationToken) -> Result<String, String> {
        self.0.wait(cancel).await.map_err(|error| error.0)
    }
}

struct CallHandle<'a> {
    id: u64,
    writes: bool,
    answer: oneshot::Receiver<Result<String, String>>,
    pending: Pending<'a>,
}

impl CallHandle<'_> {
    async fn wait(self, cancel: &CancellationToken) -> ToolResult {
        let CallHandle {
            writes,
            answer,
            pending,
            ..
        } = self;
        tokio::pin!(answer);
        let answer = tokio::select! {
            answer = &mut answer => answer,
            _ = cancel.cancelled() => {
                if pending.withdraw() {
                    return Err(ToolError::new("已停止，没有执行。"));
                }
                pending.stop();
                if !writes {
                    return Err(ToolError::new("已停止。"));
                }
                // 改动已经交出去了：等一会儿那边的结果，照实报。
                match tokio::time::timeout(STOPPED_WRITE_WAIT, &mut answer).await {
                    Ok(answer) => answer,
                    Err(_) => {
                        return Ok(
                            "已停止。这一步已经交给共享目录那台电脑了，可能已经改了，请看一下文件。"
                                .into(),
                        )
                    }
                }
            }
        };
        match answer {
            Ok(result) => result.map_err(ToolError),
            Err(_) => Err(ToolError::new("共享目录那边没有交回结果。")),
        }
    }
}

/// 服务重启后从这里开始编号：那边可能还在跑重启前交出去的命令，编号不能撞上。
fn first_id() -> u64 {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64);
    ms << 10
}

/// 停止后还交出去了的改动，等它多久的结果。
const STOPPED_WRITE_WAIT: Duration = Duration::from_secs(10);

/// 一次调用还在等着。丢下时把它从队列里撤掉。
struct Pending<'a> {
    shares: &'a Shares,
    name: &'a str,
    id: u64,
}

impl Pending<'_> {
    /// 还没交给那台电脑的话撤回，回 `true`；已经交出去（或已经有结果）的回 `false`。
    fn withdraw(&self) -> bool {
        let mut state = self.shares.state.lock().unwrap();
        let Some(entry) = state.shares.get_mut(self.name) else {
            return false;
        };
        if !entry.calls.get(&self.id).is_some_and(|call| !call.sent) {
            return false;
        }
        entry.calls.remove(&self.id);
        entry.queue.retain(|queued| *queued != self.id);
        true
    }

    /// 已经交出去了：叫那边停下（命令会连同它启动的进程一起结束）。
    fn stop(&self) {
        let mut state = self.shares.state.lock().unwrap();
        if let Some(entry) = state.shares.get_mut(self.name) {
            if entry.calls.get(&self.id).is_some_and(|call| call.sent) {
                entry.stop(self.id);
            }
        }
        drop(state);
        self.shares.wake.notify_waiters();
    }
}

impl Drop for Pending<'_> {
    /// 停止时调用方可能直接丢下这次调用：没交出去的撤回，交出去了还没交回的叫那边停下。
    /// 两件事在同一把锁里做：中间不能被取活的拿走。
    fn drop(&mut self) {
        let mut state = self.shares.state.lock().unwrap();
        if let Some(entry) = state.shares.get_mut(self.name) {
            entry.queue.retain(|queued| *queued != self.id);
            entry.controls.retain(|c| c.id != self.id);
            if entry.calls.remove(&self.id).is_some_and(|call| call.sent) {
                entry.stop(self.id);
            }
        }
        drop(state);
        self.shares.wake.notify_waiters();
    }
}

/// 一次取活结束（交出了活、等到头、或者连接断了）时记一下：它刚刚还在。
struct Polling<'a> {
    shares: &'a Shares,
    name: &'a str,
    session: &'a str,
}

impl Drop for Polling<'_> {
    fn drop(&mut self) {
        let mut state = self.shares.state.lock().unwrap();
        if let Some(entry) = state.shares.get_mut(self.name) {
            if entry.session == self.session {
                entry.polling = entry.polling.saturating_sub(1);
                entry.last_seen = Some(Instant::now());
            }
        }
    }
}

/// 用共享目录 `name` 当工作目录时的工具：文件工具和命令换成交给那台电脑去做的（那边不让执行命令就不给），别的照旧。
pub(crate) fn tools(shares: &Arc<Shares>, name: &str, base: &ToolRegistry) -> ToolRegistry {
    let read_only = shares.read_only(name);
    let shell = shares.shell(name);
    let mut tools = ToolRegistry::new();
    for tool in base.iter() {
        let tool_name = tool.name();
        if tool_name == "shell" && shell.is_none() {
            continue;
        }
        if !SHARED_TOOLS.contains(&tool_name) {
            tools.register_shared(tool.clone());
            continue;
        }
        if read_only && !READ_ONLY_TOOLS.contains(&tool_name) {
            continue;
        }
        tools.register(Remote {
            inner: tool.clone(),
            shares: shares.clone(),
            name: name.to_string(),
            shell: shell.clone(),
        });
    }
    tools
}

/// 交给共享目录那台电脑去做的工具。参数和标题照搬本机的同名工具。
struct Remote {
    inner: Arc<dyn Tool>,
    shares: Arc<Shares>,
    name: String,
    /// 那台电脑上的命令行：命令的说明按它写。
    shell: Option<String>,
}

impl Remote {
    fn runs_commands(&self) -> bool {
        self.inner.name() == "shell"
    }
}

#[async_trait]
impl Tool for Remote {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> String {
        match &self.shell {
            Some(shell) if self.runs_commands() => sa_tools::shell_description(shell),
            _ => self.inner.description(),
        }
    }

    fn input_schema(&self) -> Value {
        self.inner.input_schema()
    }

    fn title(&self, input: &Value) -> String {
        self.inner.title(input)
    }

    fn is_read_only(&self, input: &Value) -> bool {
        self.inner.is_read_only(input)
    }

    fn is_concurrency_safe(&self, input: &Value) -> bool {
        self.inner.is_concurrency_safe(input)
    }

    async fn check_permission(&self, input: &Value, _ctx: &ToolCtx) -> Permission {
        if self.runs_commands() {
            Permission::Ask(format!(
                "要在共享目录「{}」那台电脑上执行命令：{}",
                self.name,
                input["command"].as_str().unwrap_or_default()
            ))
        } else if self.is_read_only(input) {
            Permission::Allow
        } else {
            let path = input["path"].as_str().unwrap_or_default();
            if sa_tools::in_git_dir(std::path::Path::new(path)) {
                Permission::Ask(format!(
                    "要改的是共享目录「{}」里的 git 内部文件（里面的配置能让 git 执行命令）：{}",
                    self.name, path
                ))
            } else {
                Permission::Ask(format!(
                    "会改动共享目录「{}」里的文件（在那台电脑上）",
                    self.name
                ))
            }
        }
    }

    fn finishes_once_started(&self) -> bool {
        self.inner.finishes_once_started()
    }

    fn max_result_chars(&self) -> usize {
        self.inner.max_result_chars()
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolResult {
        let writes = !self.is_read_only(&input);
        self.shares
            .call(&self.name, self.inner.name(), input, writes, &ctx.cancel)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hello(session: &str, read_only: bool) -> SharePoll {
        SharePoll {
            name: "项目".into(),
            session: session.into(),
            root: "D:\\项目".into(),
            read_only,
            shell: None,
            running: Vec::new(),
            interactive: false,
        }
    }

    const NOW: Duration = Duration::from_millis(50);

    async fn next(shares: &Shares, session: &str) -> ShareCall {
        // 新连上来的第一次取活没活就马上回空，之后才等活。
        if let Some(call) = shares.poll(hello(session, false), NOW).await.unwrap() {
            return call;
        }
        shares
            .poll(hello(session, false), Duration::from_secs(5))
            .await
            .unwrap()
            .expect("应该有活")
    }

    #[tokio::test]
    async fn a_call_waits_for_the_computer_and_gets_its_answer() {
        let shares = Arc::new(Shares::default());
        let cancel = CancellationToken::new();
        let calling = {
            let (shares, cancel) = (shares.clone(), cancel.clone());
            tokio::spawn(async move {
                shares
                    .call("项目", "read", json!({"path": "a"}), false, &cancel)
                    .await
            })
        };
        tokio::time::sleep(NOW).await;
        assert!(shares.list().is_empty(), "还没连上来过的不列出来");
        let call = next(&shares, "s1").await;
        assert_eq!(
            (call.tool.as_str(), &call.input),
            ("read", &json!({"path": "a"}))
        );
        assert!(shares.list()[0].online);
        let reply = ShareReply {
            name: "项目".into(),
            session: "s1".into(),
            id: call.id,
            ok: true,
            text: "内容".into(),
            progress: false,
        };
        shares.reply(reply).unwrap();
        assert_eq!(calling.await.unwrap().unwrap(), "内容");
    }

    #[tokio::test]
    async fn lost_reads_are_sent_again_and_lost_writes_are_reported() {
        let shares = Arc::new(Shares::default());
        let cancel = CancellationToken::new();
        let spawn = |tool: &'static str, writes: bool| {
            let (shares, cancel) = (shares.clone(), cancel.clone());
            tokio::spawn(async move { shares.call("项目", tool, json!({}), writes, &cancel).await })
        };
        let write = spawn("write", true);
        tokio::time::sleep(NOW).await;
        let read = spawn("read", false);
        tokio::time::sleep(NOW).await;
        assert_eq!(next(&shares, "s1").await.tool, "write");
        // 换了一次运行（电脑重启了 sa share）：交出去没交回的写不知道做没做。
        let again = next(&shares, "s2").await;
        let error = write.await.unwrap().unwrap_err();
        assert!(error.0.contains("不知道这次改动做完没有"), "{error:?}");
        assert_eq!(again.tool, "read");
        // 读丢了就再交一次。
        let again = next(&shares, "s3").await;
        assert_eq!(again.tool, "read");
        // 被顶替的那次运行交回来的不收。
        let stale = ShareReply {
            name: "项目".into(),
            session: "s2".into(),
            id: again.id,
            ok: true,
            text: "旧的".into(),
            progress: false,
        };
        assert!(shares.reply(stale).is_err());
        // 被顶替的那次运行再来取活也回不来。
        assert!(shares.poll(hello("s2", false), NOW).await.is_err());
        cancel.cancel();
        assert!(read.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn read_only_shares_refuse_queued_writes_and_hide_write_tools() {
        let shares = Arc::new(Shares::default());
        let cancel = CancellationToken::new();
        let write = {
            let (shares, cancel) = (shares.clone(), cancel.clone());
            tokio::spawn(
                async move { shares.call("项目", "write", json!({}), true, &cancel).await },
            )
        };
        tokio::time::sleep(NOW).await;
        assert_eq!(shares.poll(hello("s1", true), NOW).await.unwrap(), None);
        assert!(write.await.unwrap().unwrap_err().0.contains("只读"));
        let tools = tools(&shares, "项目", &ToolRegistry::standard());
        let names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
        for name in READ_ONLY_TOOLS {
            assert!(names.contains(&name), "{names:?}");
        }
        for name in ["write", "edit", "shell"] {
            assert!(!names.contains(&name), "{names:?}");
        }
    }

    #[tokio::test]
    async fn stopping_a_sent_call_tells_the_computer_to_stop_it() {
        let shares = Arc::new(Shares::default());
        let cancel = CancellationToken::new();
        let calling = {
            let (shares, cancel) = (shares.clone(), cancel.clone());
            tokio::spawn(async move {
                shares
                    .call(
                        "项目",
                        "shell",
                        json!({"command": "sleep 9"}),
                        true,
                        &cancel,
                    )
                    .await
            })
        };
        tokio::time::sleep(NOW).await;
        let mut busy = hello("s1", false);
        busy.shell = Some("bash".into());
        let call = shares
            .poll(busy.clone(), NOW)
            .await
            .unwrap()
            .expect("应该有活");
        assert_eq!(call.tool, "shell");
        // 那边说还在做：不当它丢了，不重交。
        busy.running = vec![call.id];
        assert_eq!(shares.poll(busy.clone(), NOW).await.unwrap(), None);
        // 调用方直接丢下了这次调用（点了停止）：下一次取活拿到的是「停下」。
        calling.abort();
        let _ = calling.await;
        let stop = shares
            .poll(busy.clone(), NOW)
            .await
            .unwrap()
            .expect("应该叫停");
        assert_eq!((stop.tool.as_str(), stop.id), (STOP, call.id));
        // 「停下」半路丢了，那边还说在跑：过一阵再叫一次，不会每次都叫。
        assert_eq!(shares.poll(busy.clone(), NOW).await.unwrap(), None);
        tokio::time::sleep(STOP_RESEND).await;
        let again = shares
            .poll(busy.clone(), NOW)
            .await
            .unwrap()
            .expect("再叫停");
        assert_eq!((again.tool.as_str(), again.id), (STOP, call.id));
        // 那边不跑了：不再叫。
        busy.running.clear();
        tokio::time::sleep(STOP_RESEND).await;
        assert_eq!(shares.poll(busy, NOW).await.unwrap(), None);
    }

    #[test]
    fn call_ids_do_not_restart_from_one_after_a_restart() {
        assert!(first_id() > 1 << 40);
    }

    #[tokio::test]
    async fn shells_are_only_offered_when_the_computer_allows_them() {
        let shares = Arc::new(Shares::default());
        let cancel = CancellationToken::new();
        let shell = {
            let (shares, cancel) = (shares.clone(), cancel.clone());
            tokio::spawn(
                async move { shares.call("项目", "shell", json!({}), true, &cancel).await },
            )
        };
        tokio::time::sleep(NOW).await;
        // 没开 --commands：排着的命令不交出去，也不给执行命令的工具。
        assert_eq!(shares.poll(hello("s1", false), NOW).await.unwrap(), None);
        assert!(shell.await.unwrap().unwrap_err().0.contains("--commands"));
        let names = |shares: &Arc<Shares>| -> Vec<String> {
            tools(shares, "项目", &ToolRegistry::standard())
                .iter()
                .map(|tool| tool.name().to_string())
                .collect()
        };
        assert!(!names(&shares).contains(&"shell".to_string()));
        let mut commands = hello("s2", false);
        commands.shell = Some("bash".into());
        assert_eq!(shares.poll(commands.clone(), NOW).await.unwrap(), None);
        assert!(names(&shares).contains(&"shell".to_string()));
        assert_eq!(shares.list()[0].shell.as_deref(), Some("bash"));
        // 只读的就算说了能执行命令也不给。
        let mut read_only = hello("s3", true);
        read_only.shell = Some("bash".into());
        assert_eq!(shares.poll(read_only, NOW).await.unwrap(), None);
        assert!(!names(&shares).contains(&"shell".to_string()));
    }

    #[tokio::test]
    async fn stopping_a_waiting_call_takes_it_out_of_the_queue() {
        let shares = Arc::new(Shares::default());
        let cancel = CancellationToken::new();
        let read = {
            let (shares, cancel) = (shares.clone(), cancel.clone());
            tokio::spawn(
                async move { shares.call("项目", "read", json!({}), false, &cancel).await },
            )
        };
        tokio::time::sleep(NOW).await;
        cancel.cancel();
        assert_eq!(read.await.unwrap().unwrap_err().0, "已停止，没有执行。");
        // 调用方直接丢下调用（停止时读文件就是这样）也一样撤回。
        let dropped = {
            let shares = shares.clone();
            tokio::spawn(async move {
                shares
                    .call("项目", "read", json!({}), false, &CancellationToken::new())
                    .await
            })
        };
        tokio::time::sleep(NOW).await;
        dropped.abort();
        let _ = dropped.await;
        let _ = shares.poll(hello("s1", false), NOW).await;
        assert_eq!(shares.poll(hello("s1", false), NOW).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_stopped_write_reports_what_the_computer_did() {
        let shares = Arc::new(Shares::default());
        let cancel = CancellationToken::new();
        let write = {
            let (shares, cancel) = (shares.clone(), cancel.clone());
            tokio::spawn(
                async move { shares.call("项目", "write", json!({}), true, &cancel).await },
            )
        };
        tokio::time::sleep(NOW).await;
        let call = next(&shares, "s1").await;
        cancel.cancel();
        tokio::time::sleep(NOW).await;
        let reply = ShareReply {
            name: "项目".into(),
            session: "s1".into(),
            id: call.id,
            ok: true,
            text: "写好了".into(),
            progress: false,
        };
        shares.reply(reply).unwrap();
        assert_eq!(write.await.unwrap().unwrap(), "写好了");
    }

    #[tokio::test]
    async fn pending_calls_are_dropped_when_the_name_moves_to_another_folder() {
        let shares = Arc::new(Shares::default());
        let _ = shares.poll(hello("s1", false), NOW).await;
        let write = {
            let shares = shares.clone();
            tokio::spawn(async move {
                shares
                    .call("项目", "write", json!({}), true, &CancellationToken::new())
                    .await
            })
        };
        tokio::time::sleep(NOW).await;
        let mut moved = hello("s2", false);
        moved.root = "E:\\别的".into();
        assert_eq!(shares.poll(moved, NOW).await.unwrap(), None);
        assert!(write
            .await
            .unwrap()
            .unwrap_err()
            .0
            .contains("换成了另一个目录"));
        // 服务重启后不认识的共享交回来的结果收下就是。
        let late = ShareReply {
            name: "别的名字".into(),
            session: "s9".into(),
            id: 1,
            ok: true,
            text: String::new(),
            progress: false,
        };
        assert!(shares.reply(late).is_ok());
    }

    #[tokio::test]
    async fn interactive_controls_wait_for_sent_prioritize_over_queue_and_clean_up() {
        let shares = Arc::new(Shares::default());
        // 旧客户端（未声明 interactive）或未开 --commands：不支持交互控制。
        assert_eq!(shares.poll(hello("s1", false), NOW).await.unwrap(), None);
        assert!(!shares.interactive("项目"));

        let mut interactive = hello("s2", false);
        interactive.shell = Some("bash".into());
        interactive.interactive = true;
        assert_eq!(shares.poll(interactive.clone(), NOW).await.unwrap(), None);
        assert!(shares.interactive("项目"));

        // 入队一次远端工位调用，但在它被 poll 拿走之前就写入一条 _steer 控制消息：
        // 必须先派发主调用本身，再在下一次 poll 派发控制消息，避免竞态扑空。
        let cancel = CancellationToken::new();
        let handle = shares.start_ask("项目", crate::share_desk::RUN, json!({}), None);
        let run_id = handle.id();
        assert!(shares.send_control(
            "项目",
            run_id,
            crate::share_desk::STEER,
            json!({"steer_id": "st1", "text": "先看测试"}),
        ));
        let first = shares
            .poll(interactive.clone(), NOW)
            .await
            .unwrap()
            .expect("主调用必须先于控制消息派发");
        assert_eq!(
            (first.id, first.tool.as_str()),
            (run_id, crate::share_desk::RUN)
        );

        // 此时再排一个普通调用，控制消息（_steer）必须优先于普通队列派发。
        let cancel2 = CancellationToken::new();
        let second_task = {
            let shares = shares.clone();
            let cancel2 = cancel2.clone();
            tokio::spawn(async move {
                shares
                    .call("项目", "read", json!({"path": "a"}), false, &cancel2)
                    .await
            })
        };
        tokio::time::sleep(NOW).await;
        interactive.running = vec![run_id];
        let ctrl = shares
            .poll(interactive.clone(), NOW)
            .await
            .unwrap()
            .expect("控制消息应优先于普通队列");
        assert_eq!(
            (ctrl.id, ctrl.tool.as_str()),
            (run_id, crate::share_desk::STEER)
        );

        // 再塞一条控制消息后直接结束主调用：残留控制消息应随调用结束被自动清除。
        assert!(shares.send_control(
            "项目",
            run_id,
            crate::share_desk::APPROVAL,
            json!({"req_id": "r1", "allowed": true}),
        ));
        shares
            .reply(ShareReply {
                name: "项目".into(),
                session: "s2".into(),
                id: run_id,
                ok: true,
                text: "done".into(),
                progress: false,
            })
            .unwrap();
        assert_eq!(handle.wait(&cancel).await.unwrap(), "done");
        assert!(!shares.send_control(
            "项目",
            run_id,
            crate::share_desk::APPROVAL,
            json!({"req_id": "r2", "allowed": true}),
        ));

        interactive.running.clear();
        let queued = shares
            .poll(interactive, NOW)
            .await
            .unwrap()
            .expect("应轮到普通 read 调用而非已清理的旧控制消息");
        assert_eq!(queued.tool, "read");
        cancel2.cancel();
        let _ = second_task.await;
    }
}
