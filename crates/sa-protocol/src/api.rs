use crate::{ApprovalDecision, BotId, Message, ModelTier};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Health {
    pub status: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CreateRoomRequest {
    pub name: String,
}

/// 所有失败响应的统一形状。`code` 给程序判断，`message` 给人看。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct PostMessageRequest {
    pub content: String,
}

/// 一页消息，按 `seq` 升序。`has_more` 表示更早的还有。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct MessagePage {
    pub messages: Vec<Message>,
    pub has_more: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct StopResponse {
    /// 是否真的有一条正在写的回复被停下。
    pub stopped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DecideApprovalRequest {
    pub decision: ApprovalDecision,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CreateBotRequest {
    pub name: String,
    /// 不给就是 `mid`。
    #[serde(default)]
    #[ts(optional)]
    pub tier: Option<ModelTier>,
    #[serde(default)]
    #[ts(optional)]
    pub persona: Option<String>,
}

/// 只改给出的字段。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct UpdateBotRequest {
    #[serde(default)]
    #[ts(optional)]
    pub name: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    pub tier: Option<ModelTier>,
    #[serde(default)]
    #[ts(optional)]
    pub persona: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    pub enabled: Option<bool>,
}

/// 只改给出的字段。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct UpdateRoomRequest {
    #[serde(default)]
    #[ts(optional)]
    pub name: Option<String>,
    /// 新的入口 Bot，必须已经是房间成员。
    #[serde(default)]
    #[ts(optional)]
    pub host_bot_id: Option<BotId>,
}
