//! 同一个房间里几个 Bot 同时说话时，共用一个工作目录：读和搜随便，改文件、执行命令一个一个来。

use crate::RoomLink;
use async_trait::async_trait;
use sa_tools::{Permission, Tool, ToolCtx, ToolError, ToolRegistry, ToolResult};
use serde_json::Value;
use std::sync::Arc;

const STOPPED: &str = "已停止，没有执行。";
const CHANGED: &str = "等别的 Bot 改完的时候，这一步要动的东西变了，没有执行。请重新确认后再做。";

/// 把 `tools` 里每个会动东西的调用都包一层：执行的那一下要拿到房间的写锁。
/// 审批在执行之前，不拿锁：一个 Bot 等批准时别的 Bot 照样能改。
pub(crate) fn one_writer_at_a_time(tools: ToolRegistry, link: Arc<RoomLink>) -> ToolRegistry {
    let mut serial = ToolRegistry::new();
    for tool in tools.iter() {
        serial.register(Serial {
            inner: tool.clone(),
            link: link.clone(),
        });
    }
    serial
}

struct Serial {
    inner: Arc<dyn Tool>,
    link: Arc<RoomLink>,
}

#[async_trait]
impl Tool for Serial {
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

    async fn check_permission(&self, input: &Value, ctx: &ToolCtx) -> Permission {
        self.inner.check_permission(input, ctx).await
    }

    fn finishes_once_started(&self) -> bool {
        self.inner.finishes_once_started()
    }

    fn max_result_chars(&self) -> usize {
        self.inner.max_result_chars()
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolResult {
        if self.inner.is_read_only(&input) {
            return self.inner.call(input, ctx).await;
        }
        // 排队等别的 Bot 改完时，它可能把这次要动的路径改了（比如换成指向别处的链接）：
        // 拿到锁之后再查一遍，和排队前不一样就不做，免得批准的是一件事、做的是另一件。
        let before = self.inner.check_permission(&input, ctx).await;
        // 排队的时候点了停止：不用等别的 Bot 做完。
        let _writing = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return Err(ToolError(STOPPED.into())),
            writing = self.link.writing.lock() => writing,
        };
        if self.inner.check_permission(&input, ctx).await != before {
            return Err(ToolError(CHANGED.into()));
        }
        self.inner.call(input, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sa_tools::Fence;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// 调用时停一会儿，记下最多有几个同时在跑。
    struct Slow {
        name: &'static str,
        read_only: bool,
        now: Arc<AtomicUsize>,
        most: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for Slow {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> String {
            String::new()
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({"type": "object"})
        }
        fn title(&self, _input: &Value) -> String {
            self.name.into()
        }
        fn is_read_only(&self, _input: &Value) -> bool {
            self.read_only
        }
        async fn call(&self, _input: Value, _ctx: &ToolCtx) -> ToolResult {
            let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
            self.most.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            self.now.fetch_sub(1, Ordering::SeqCst);
            Ok(String::new())
        }
    }

    async fn most_at_once(read_only: bool) -> usize {
        let dir = tempfile::tempdir().unwrap();
        let link = Arc::new(RoomLink::new());
        let (now, most) = (Arc::default(), Arc::new(AtomicUsize::new(0)));
        // 两个 Bot 各有一份包好的工具，共用同一个房间。
        let registries: Vec<ToolRegistry> = (0..2)
            .map(|_| {
                let mut tools = ToolRegistry::new();
                tools.register(Slow {
                    name: "slow",
                    read_only,
                    now: Arc::clone(&now),
                    most: most.clone(),
                });
                one_writer_at_a_time(tools, link.clone())
            })
            .collect();
        let ctx = ToolCtx::new(Fence::new(dir.path(), &[]).unwrap());
        let calls = registries.iter().flat_map(|tools| {
            let tool = tools.get("slow").unwrap().clone();
            let ctx = &ctx;
            (0..2).map(move |_| {
                let tool = tool.clone();
                async move { tool.call(Value::Null, ctx).await }
            })
        });
        for result in futures_util::future::join_all(calls).await {
            result.unwrap();
        }
        most.load(Ordering::SeqCst)
    }

    #[tokio::test]
    async fn stopping_a_write_that_waits_for_another_bot_does_not_wait() {
        let dir = tempfile::tempdir().unwrap();
        let link = Arc::new(RoomLink::new());
        let mut tools = ToolRegistry::new();
        tools.register(Slow {
            name: "slow",
            read_only: false,
            now: Arc::default(),
            most: Arc::default(),
        });
        let tool = one_writer_at_a_time(tools, link.clone())
            .get("slow")
            .unwrap()
            .clone();
        // 别的 Bot 正拿着写锁。
        let _other = link.writing.lock().await;
        let ctx = ToolCtx::new(Fence::new(dir.path(), &[]).unwrap());
        let stopper = ctx.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            stopper.cancel();
        });
        let result = tokio::time::timeout(Duration::from_secs(5), tool.call(Value::Null, &ctx))
            .await
            .expect("停止之后还在等别的 Bot");
        assert_eq!(result.unwrap_err().0, STOPPED);
    }

    #[tokio::test]
    async fn writes_take_turns_and_reads_run_together() {
        assert_eq!(most_at_once(false).await, 1);
        assert_eq!(most_at_once(true).await, 4);
    }
}
