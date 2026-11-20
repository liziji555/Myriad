use crate::{ChatRequest, EventStream, LlmError, Provider};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// 第一次失败之后最多再试几次。
    pub max_retries: u32,
    /// 第一次重试前等多久，之后每次翻倍。
    pub base_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_secs(1),
        }
    }
}

/// 发起调用；还没有任何输出就遇到临时性失败（限流、5xx、网络、超时）时退避重试。
///
/// 流开始之后的失败不在这里处理：已经发出去的内容收不回来，由调用方决定怎么办。
pub async fn open_with_retry(
    provider: &dyn Provider,
    request: &ChatRequest,
    cancel: &CancellationToken,
    policy: RetryPolicy,
) -> Result<EventStream, LlmError> {
    let mut attempt = 0;
    loop {
        if cancel.is_cancelled() {
            return Err(LlmError::Cancelled);
        }
        match provider.stream(request.clone(), cancel.clone()).await {
            Ok(stream) => return Ok(stream),
            Err(error) if error.is_retryable() && attempt < policy.max_retries => {
                let delay = policy.base_delay * 2u32.pow(attempt);
                attempt += 1;
                tracing::warn!(%error, attempt, "模型调用失败，{delay:?} 后重试");
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = cancel.cancelled() => return Err(LlmError::Cancelled),
                }
            }
            Err(error) => return Err(error),
        }
    }
}
