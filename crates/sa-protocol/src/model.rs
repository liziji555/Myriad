use crate::{BotId, Millis, RoomId};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Bot 的职责类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum BotKind {
    Assistant,
    Router,
    Worker,
    Group,
    Watchdog,
}

/// 模型档位。Bot 只写档位，具体模型在 `config.toml` 里映射。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ModelTier {
    Cheap,
    Mid,
    Strong,
}

macro_rules! str_enum {
    ($ty:ident { $($variant:ident => $s:literal),+ $(,)? }) => {
        impl $ty {
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $s),+ }
            }

            pub fn parse(s: &str) -> Option<Self> {
                match s { $($s => Some(Self::$variant),)+ _ => None }
            }
        }
    };
}

str_enum!(BotKind {
    Assistant => "assistant",
    Router => "router",
    Worker => "worker",
    Group => "group",
    Watchdog => "watchdog",
});

str_enum!(ModelTier {
    Cheap => "cheap",
    Mid => "mid",
    Strong => "strong",
});

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Bot {
    pub id: BotId,
    pub name: String,
    pub kind: BotKind,
    pub tier: ModelTier,
    pub enabled: bool,
    #[ts(type = "number")]
    pub created_at: Millis,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Room {
    pub id: RoomId,
    pub name: String,
    /// 房间的入口 Bot：没有 @ 任何人时由它接话。
    pub host_bot_id: Option<BotId>,
    pub member_bot_ids: Vec<BotId>,
    #[ts(type = "number")]
    pub created_at: Millis,
    #[ts(type = "number")]
    pub updated_at: Millis,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn str_enums_round_trip_and_match_serde() {
        for kind in [
            BotKind::Assistant,
            BotKind::Router,
            BotKind::Worker,
            BotKind::Group,
            BotKind::Watchdog,
        ] {
            assert_eq!(BotKind::parse(kind.as_str()), Some(kind));
            assert_eq!(
                serde_json::to_string(&kind).unwrap(),
                format!("\"{}\"", kind.as_str())
            );
        }
        for tier in [ModelTier::Cheap, ModelTier::Mid, ModelTier::Strong] {
            assert_eq!(ModelTier::parse(tier.as_str()), Some(tier));
            assert_eq!(
                serde_json::to_string(&tier).unwrap(),
                format!("\"{}\"", tier.as_str())
            );
        }
        assert_eq!(BotKind::parse("nope"), None);
    }
}
