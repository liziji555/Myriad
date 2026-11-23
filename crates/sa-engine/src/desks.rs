//! harness Bot 在房间里的工位。

use crate::{Engine, Result};
use sa_protocol::{BotId, HarnessDesk, RoomId};
use sa_store::desks;

pub use sa_store::desks::Desk;

impl Engine {
    pub fn desk(&self, room: &RoomId, bot: &BotId) -> Result<Desk> {
        Ok(self.store.read(|c| desks::get(c, room, bot))?)
    }

    pub fn put_desk(&self, room: &RoomId, bot: &BotId, desk: &Desk) -> Result<()> {
        Ok(self.store.write(|tx| desks::put(tx, room, bot, desk))?)
    }

    /// 房间里开着独立目录的工位，给界面看。
    pub fn room_desks(&self, room: &RoomId) -> Result<Vec<HarnessDesk>> {
        let all = self.store.read(|c| desks::list(c, room))?;
        Ok(all
            .into_iter()
            .filter_map(|(bot_id, desk)| {
                let workdir = desk.workdir?;
                Some(HarnessDesk {
                    bot_id,
                    path: Some(workdir.path),
                    branch: workdir.branch,
                    changes: desk.changes,
                })
            })
            .collect())
    }
}
