//! 「总是允许」：人批准时选了以后不再问，就记在这里。

use crate::{now_ms, Engine, EngineError, Result};
use sa_protocol::{PermissionRule, RoomId, RuleId};
use sa_store::{rooms, rules};

impl Engine {
    /// 记下：这个房间里，`tool` 做 `pattern` 这件事以后不用再问。
    pub fn allow_always(
        &self,
        room_id: &RoomId,
        tool: &str,
        pattern: &str,
    ) -> Result<PermissionRule> {
        let rule = PermissionRule {
            id: RuleId::new(),
            room_id: room_id.clone(),
            tool: tool.to_string(),
            pattern: pattern.to_string(),
            created_at: now_ms(),
        };
        self.store
            .write(|tx| {
                if rooms::get(tx, room_id)?.is_none() {
                    return Ok(None);
                }
                rules::insert(tx, &rule).map(Some)
            })?
            .ok_or_else(|| EngineError::NotFound("没有这个房间".into()))
    }

    pub fn is_always_allowed(&self, room_id: &RoomId, tool: &str, pattern: &str) -> Result<bool> {
        Ok(self
            .store
            .read(|c| rules::find(c, room_id, tool, pattern))?
            .is_some())
    }

    pub fn list_rules(&self, room_id: &RoomId) -> Result<Vec<PermissionRule>> {
        self.store
            .read(|c| {
                if rooms::get(c, room_id)?.is_none() {
                    return Ok(None);
                }
                rules::list(c, room_id).map(Some)
            })?
            .ok_or_else(|| EngineError::NotFound("没有这个房间".into()))
    }

    /// 撤销一条。返回是否真的有这条。
    pub fn delete_rule(&self, id: &RuleId) -> Result<bool> {
        Ok(self.store.write(|tx| rules::delete(tx, id))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sa_store::Store;

    #[test]
    fn always_allow_is_remembered_per_room_until_revoked() {
        let engine = Engine::new(Store::open_in_memory().unwrap()).unwrap();
        engine.ensure_defaults().unwrap();
        let room = engine.list_rooms().unwrap().remove(0).id;
        let other = engine.create_room("另一个").unwrap().id;

        assert!(!engine
            .is_always_allowed(&room, "shell", "cargo test")
            .unwrap());
        let rule = engine.allow_always(&room, "shell", "cargo test").unwrap();
        assert_eq!(
            engine.allow_always(&room, "shell", "cargo test").unwrap(),
            rule
        );
        assert!(engine
            .is_always_allowed(&room, "shell", "cargo test")
            .unwrap());
        assert!(!engine
            .is_always_allowed(&room, "shell", "cargo test -q")
            .unwrap());
        assert!(!engine
            .is_always_allowed(&other, "shell", "cargo test")
            .unwrap());
        assert_eq!(engine.list_rules(&room).unwrap(), [rule.clone()]);

        assert!(engine.delete_rule(&rule.id).unwrap());
        assert!(!engine.delete_rule(&rule.id).unwrap());
        assert!(!engine
            .is_always_allowed(&room, "shell", "cargo test")
            .unwrap());

        let missing = RoomId::new();
        assert!(matches!(
            engine.allow_always(&missing, "shell", "ls"),
            Err(EngineError::NotFound(_))
        ));
        assert!(matches!(
            engine.list_rules(&missing),
            Err(EngineError::NotFound(_))
        ));
    }
}
