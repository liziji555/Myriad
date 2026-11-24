//! 别的电脑用 `sa share` 共享过来的目录。
//!
//! 那台电脑上的 `sa share` 经隧道来这里取活（长轮询），在它自己那边执行文件工具，再把结果交回来。
//! 它一次只做一件：来取下一件，就说明手上那件已经交回了，没交回的就是丢了。
//! 电脑没连着时，要用这个目录的那一步就等着，连上之后接着做；人点了停止就不等了。

use async_trait::async_trait;
use sa_protocol::{Share, ShareCall, SharePoll, ShareReply, MAX_SHARE_NAME_CHARS};
use sa_tools::{Permission, Tool, ToolCtx, ToolError, ToolRegistry, ToolResult};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, Notify};
use tokio_util::sync::CancellationToken;

/// 在共享目录里执行的工具。命令（shell）还不行。
const SHARED_TOOLS: [&str; 6] = ["read", "list_dir", "glob", "grep", "write", "edit"];
/// 只读的共享里能用的工具。
pub const READ_ONLY_TOOLS: [&str; 4] = ["read", "list_dir", "glob", "grep"];
/// 这么久没来取活，就算那台电脑断开了。
const ONLINE_GRACE: Duration = Duration::from_secs(40);
const TAKEN: &str = "这个共享已经被另一个 sa share 接手了";

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
    /// 正挂着等活的请求有几个。
    polling: usize,
    last_seen: Option<Instant>,
    /// 还没交出去的活，先来先做。
    queue: VecDeque<u64>,
    calls: HashMap<u64, Call>,
}

impl Entry {
    fn online(&self) -> bool {
        self.polling > 0 || self.last_seen.is_some_and(|at| at.elapsed() < ONLINE_GRACE)
    }

    /// 交出去了却没交回：只读的重新排到最前面再做一次；改文件的不知道做没做，照实告诉 Bot。
    fn requeue_lost(&mut self) {
        let mut lost: Vec<u64> = self
            .calls
            .iter()
            .filter(|(_, call)| call.sent)
            .map(|(id, _)| *id)
            .collect();
        lost.sort_unstable();
        for id in lost.into_iter().rev() {
            let call = self.calls.get_mut(&id).expect("刚找到的");
            if call.writes {
                let call = self.calls.remove(&id).expect("刚找到的");
                let _ = call.reply.send(Err(
                    "和共享目录那台电脑的连接断了，不知道这次改动做完没有。请先看一下文件再决定要不要重做。"
                        .into(),
                ));
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
            })
            .collect();
        shares.sort_by(|a, b| a.name.cmp(&b.name));
        shares
    }

    /// 最近一次知道的：这个共享是不是只读。没连上来过就当能改，到那边再说。
    fn read_only(&self, name: &str) -> bool {
        let state = self.state.lock().unwrap();
        state.shares.get(name).is_some_and(|entry| entry.read_only)
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
            fresh = entry.session != hello.session;
            if fresh {
                entry.session = hello.session.clone();
                entry.polling = 0;
            }
            entry.root = hello.root;
            entry.read_only = hello.read_only;
            entry.last_seen = Some(Instant::now());
            entry.requeue_lost();
            if entry.read_only {
                // 共享改成只读了：还在排队的改动不再交出去。
                let writes: Vec<u64> = entry
                    .calls
                    .iter()
                    .filter(|(_, call)| call.writes)
                    .map(|(id, _)| *id)
                    .collect();
                for id in writes {
                    entry.queue.retain(|queued| *queued != id);
                    if let Some(call) = entry.calls.remove(&id) {
                        let _ = call
                            .reply
                            .send(Err("这个共享目录是只读的，不能改。".into()));
                    }
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
        let Some(entry) = state.shares.get_mut(&name) else {
            return Err(TAKEN.into());
        };
        if entry.session != reply.session {
            return Err(TAKEN.into());
        }
        entry.last_seen = Some(Instant::now());
        if let Some(call) = entry.calls.remove(&reply.id) {
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
        let (reply, answer) = oneshot::channel();
        let id = {
            let mut state = self.state.lock().unwrap();
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
                },
            );
            entry.queue.push_back(id);
            id
        };
        self.wake.notify_waiters();
        tokio::select! {
            answer = answer => match answer {
                Ok(result) => result.map_err(ToolError),
                Err(_) => Err(ToolError::new("共享目录那边没有交回结果。")),
            },
            _ = cancel.cancelled() => {
                let mut state = self.state.lock().unwrap();
                let entry = state.shares.get_mut(name);
                let sent = entry.and_then(|entry| {
                    entry.queue.retain(|queued| *queued != id);
                    entry.calls.remove(&id)
                }).is_some_and(|call| call.sent);
                Err(ToolError::new(if sent && writes {
                    "已停止。这一步已经交给共享目录那台电脑了，可能已经改了，请看一下文件。"
                } else {
                    "已停止，没有执行。"
                }))
            }
        }
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

/// 用共享目录 `name` 当工作目录时的工具：文件工具换成交给那台电脑去做的，命令先不给，别的照旧。
pub(crate) fn tools(shares: &Arc<Shares>, name: &str, base: &ToolRegistry) -> ToolRegistry {
    let read_only = shares.read_only(name);
    let mut tools = ToolRegistry::new();
    for tool in base.iter() {
        let tool_name = tool.name();
        if tool_name == "shell" {
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
        });
    }
    tools
}

/// 交给共享目录那台电脑去做的文件工具。说明、参数和标题照搬本机的同名工具。
struct Remote {
    inner: Arc<dyn Tool>,
    shares: Arc<Shares>,
    name: String,
}

#[async_trait]
impl Tool for Remote {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> String {
        self.inner.description()
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
        if self.is_read_only(input) {
            Permission::Allow
        } else {
            Permission::Ask(format!(
                "会改动共享目录「{}」里的文件（在那台电脑上）",
                self.name
            ))
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
        };
        assert!(shares.reply(stale).is_err());
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
        let _ = shares.poll(hello("s1", false), NOW).await;
        assert_eq!(shares.poll(hello("s1", false), NOW).await.unwrap(), None);
    }
}
