//! harness Bot 在房间里的工位：独立目录、harness 自己的会话号、改了哪些文件。

use crate::tasks::Workdir;
use crate::Result;
use rusqlite::{params, Connection, OptionalExtension, Row};
use sa_protocol::{BotId, DeskChange, RoomId};

/// 一个工位。还没开过目录时 `workdir` 是 `None`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Desk {
    pub workdir: Option<Workdir>,
    pub session: Option<String>,
    pub changes: Vec<DeskChange>,
    /// 开在哪个共享目录那台电脑上。`None` 是开在服务这边。
    pub share: Option<String>,
}

const COLUMNS: &str =
    "bot_id, workdir, work_kind, branch, base_ref, work_tree, git_dir, source_dir, session, changes, share";

fn from_row(row: &Row<'_>) -> rusqlite::Result<(BotId, Desk)> {
    let text = |i| row.get::<_, Option<String>>(i);
    let workdir = match (text(1)?, text(2)?, text(4)?, text(5)?, text(6)?, text(7)?) {
        (Some(path), Some(kind), Some(base_ref), Some(root), Some(git_dir), Some(source)) => {
            Some(Workdir {
                path,
                kind,
                branch: text(3)?,
                base_ref,
                root,
                git_dir,
                source,
            })
        }
        _ => None,
    };
    let changes: String = row.get(9)?;
    Ok((
        BotId::from(row.get::<_, String>(0)?),
        Desk {
            workdir,
            session: text(8)?,
            changes: serde_json::from_str(&changes).unwrap_or_default(),
            share: text(10)?,
        },
    ))
}

pub fn get(conn: &Connection, room: &RoomId, bot: &BotId) -> Result<Desk> {
    let sql = format!("SELECT {COLUMNS} FROM harness_desks WHERE room_id = ?1 AND bot_id = ?2");
    Ok(conn
        .query_row(&sql, params![room.as_str(), bot.as_str()], from_row)
        .optional()?
        .map(|(_, desk)| desk)
        .unwrap_or_default())
}

pub fn list(conn: &Connection, room: &RoomId) -> Result<Vec<(BotId, Desk)>> {
    let sql = format!("SELECT {COLUMNS} FROM harness_desks WHERE room_id = ?1 ORDER BY bot_id");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([room.as_str()], from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn put(conn: &Connection, room: &RoomId, bot: &BotId, desk: &Desk) -> Result<()> {
    let w = desk.workdir.as_ref();
    conn.execute(
        "INSERT INTO harness_desks (room_id, bot_id, workdir, work_kind, branch, base_ref, work_tree, git_dir, source_dir, session, changes, share)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT (room_id, bot_id) DO UPDATE SET workdir = ?3, work_kind = ?4, branch = ?5, base_ref = ?6,
             work_tree = ?7, git_dir = ?8, source_dir = ?9, session = ?10, changes = ?11, share = ?12",
        params![
            room.as_str(),
            bot.as_str(),
            w.map(|w| w.path.as_str()),
            w.map(|w| w.kind.as_str()),
            w.and_then(|w| w.branch.as_deref()),
            w.map(|w| w.base_ref.as_str()),
            w.map(|w| w.root.as_str()),
            w.map(|w| w.git_dir.as_str()),
            w.map(|w| w.source.as_str()),
            desk.session,
            serde_json::to_string(&desk.changes).expect("只有字符串"),
            desk.share,
        ],
    )?;
    Ok(())
}
