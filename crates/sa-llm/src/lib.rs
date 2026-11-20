//! 模型接口。
//!
//! 上层只认识 [`Provider`]：给它一个 [`ChatRequest`]，拿回一串 [`StreamEvent`]。
//! 具体接哪家由适配器决定：[`OpenAiCompat`]（DeepSeek 及其他 OpenAI 兼容网关）和
//! 测试、演示用的 [`MockProvider`]。

mod mock;
mod openai;
mod retry;
mod types;

pub use mock::{MockProvider, MockReply};
pub use openai::OpenAiCompat;
pub use retry::{open_with_retry, RetryPolicy};
pub use types::*;

use async_trait::async_trait;
use futures_util::stream::BoxStream;
use tokio_util::sync::CancellationToken;

pub type EventStream = BoxStream<'static, Result<StreamEvent, LlmError>>;

#[async_trait]
pub trait Provider: Send + Sync {
    /// 发起一次流式调用。
    ///
    /// 返回 `Err` 表示还没有任何输出就失败了（可以按 [`LlmError::is_retryable`] 重试）；
    /// 流里的 `Err` 表示中途失败，之前的输出已经发出去了。
    /// 正常结束的流以 [`StreamEvent::Done`] 收尾；`cancel` 触发后流以 [`LlmError::Cancelled`] 结束。
    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, LlmError>;

    fn capabilities(&self, model: &str) -> ModelCaps;
}
