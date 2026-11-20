use serde::{Deserialize, Serialize};
use std::fmt;
use ts_rs::TS;
use uuid::Uuid;

macro_rules! id_type {
    ($name:ident, $prefix:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
        #[ts(export, type = "string")]
        pub struct $name(String);

        impl $name {
            pub const PREFIX: &'static str = $prefix;

            /// 生成按时间有序的新 ID，形如 `room_0192f0c8…`。
            pub fn new() -> Self {
                Self(format!("{}_{}", $prefix, Uuid::now_v7().simple()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_type!(RoomId, "room");
id_type!(BotId, "bot");
id_type!(MessageId, "msg");
id_type!(SessionId, "ses");
id_type!(ApprovalId, "apr");
id_type!(RuleId, "rule");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_prefixed_and_time_ordered() {
        let a = RoomId::new();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = RoomId::new();
        assert!(a.as_str().starts_with("room_"));
        assert!(a.as_str() < b.as_str());
    }

    #[test]
    fn ids_serialize_as_plain_strings() {
        let id = BotId::from("bot_x".to_string());
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"bot_x\"");
    }
}
