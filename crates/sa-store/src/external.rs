//! 外面的世界：连接、收到的事、配对码、要送出去的话。

use crate::{Result, StoreError};
use rusqlite::{params, Connection, OptionalExtension, Row};
use sa_protocol::{Link, LinkId, MessageId, RoomId, Source};

const LINK_COLUMNS: &str = "id, source, external, room_id, title, created_at";

/// 连上。这一头已经连着别的房间（或者这个房间）时不改，返回 `false`。
pub fn insert_link(conn: &Connection, link: &Link) -> Result<bool> {
    let added = conn.execute(
        "INSERT INTO links (id, source, external, room_id, title, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (source, external) DO NOTHING",
        params![
            link.id.as_str(),
            link.source.as_str(),
            link.external,
            link.room_id.as_str(),
            link.title,
            link.created_at,
        ],
    )?;
    Ok(added > 0)
}

pub fn delete_link(conn: &Connection, id: &LinkId) -> Result<bool> {
    Ok(conn.execute("DELETE FROM links WHERE id = ?1", [id.as_str()])? > 0)
}

pub fn get_link(conn: &Connection, id: &LinkId) -> Result<Option<Link>> {
    let sql = format!("SELECT {LINK_COLUMNS} FROM links WHERE id = ?1");
    conn.query_row(&sql, [id.as_str()], read_link)
        .optional()?
        .transpose()
}

/// 外面这一头连着哪个房间。
pub fn find_link(conn: &Connection, source: Source, external: &str) -> Result<Option<Link>> {
    let sql = format!("SELECT {LINK_COLUMNS} FROM links WHERE source = ?1 AND external = ?2");
    conn.query_row(&sql, params![source.as_str(), external], read_link)
        .optional()?
        .transpose()
}

pub fn list_links(conn: &Connection) -> Result<Vec<Link>> {
    let sql = format!("SELECT {LINK_COLUMNS} FROM links ORDER BY created_at, id");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], read_link)?;
    rows.map(|row| row?).collect()
}

/// 一个房间在某个来源上的连接。
pub fn room_links(conn: &Connection, room_id: &RoomId, source: Source) -> Result<Vec<Link>> {
    let sql = format!(
        "SELECT {LINK_COLUMNS} FROM links WHERE room_id = ?1 AND source = ?2 ORDER BY created_at, id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![room_id.as_str(), source.as_str()], read_link)?;
    rows.map(|row| row?).collect()
}

fn read_link(row: &Row<'_>) -> rusqlite::Result<Result<Link>> {
    let id: String = row.get(0)?;
    let source: String = row.get(1)?;
    let Some(source) = Source::parse(&source) else {
        return Ok(Err(StoreError::Corrupt(format!(
            "连接 {id} 的来源未知：{source}"
        ))));
    };
    Ok(Ok(Link {
        id: LinkId::from(id),
        source,
        external: row.get(2)?,
        room_id: RoomId::from(row.get::<_, String>(3)?),
        title: row.get(4)?,
        created_at: row.get(5)?,
    }))
}

/// 外面送来的一件事。
pub struct NewTrigger<'a> {
    pub source: Source,
    pub key: &'a str,
    pub kind: &'a str,
    /// 收下了没有。没收下时 `note` 是原因。
    pub stored: bool,
    pub note: &'a str,
    pub room_id: Option<&'a RoomId>,
    pub message_id: Option<&'a MessageId>,
}

/// 记下一件送来的事。同一件事已经记过时不改，返回 `false`。
pub fn record_trigger(conn: &Connection, trigger: &NewTrigger<'_>, now: i64) -> Result<bool> {
    let added = conn.execute(
        "INSERT INTO triggers (source, key, kind, outcome, note, room_id, message_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT (source, key) DO NOTHING",
        params![
            trigger.source.as_str(),
            trigger.key,
            trigger.kind,
            if trigger.stored { "stored" } else { "filtered" },
            trigger.note,
            trigger.room_id.map(RoomId::as_str),
            trigger.message_id.map(MessageId::as_str),
            now,
        ],
    )?;
    Ok(added > 0)
}

pub fn has_trigger(conn: &Connection, source: Source, key: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM triggers WHERE source = ?1 AND key = ?2",
            params![source.as_str(), key],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// 删掉 `before` 之前收到的记录，返回删了几条。
pub fn prune_triggers(conn: &Connection, before: i64) -> Result<usize> {
    Ok(conn.execute("DELETE FROM triggers WHERE created_at < ?1", [before])?)
}

/// 给房间放一个配对码（只存哈希）。房间原来的那个作废。
pub fn put_pairing(
    conn: &Connection,
    code_hash: &str,
    room_id: &RoomId,
    expires_at: i64,
) -> Result<()> {
    conn.execute(
        "DELETE FROM pairings WHERE room_id = ?1",
        [room_id.as_str()],
    )?;
    conn.execute(
        "INSERT INTO pairings (code_hash, room_id, expires_at) VALUES (?1, ?2, ?3)",
        params![code_hash, room_id.as_str(), expires_at],
    )?;
    Ok(())
}

/// 用掉一个配对码：没过期就返回它对应的房间，并且删掉它。
pub fn take_pairing(conn: &Connection, code_hash: &str, now: i64) -> Result<Option<RoomId>> {
    conn.execute("DELETE FROM pairings WHERE expires_at <= ?1", [now])?;
    let room: Option<String> = conn
        .query_row(
            "DELETE FROM pairings WHERE code_hash = ?1 RETURNING room_id",
            [code_hash],
            |row| row.get(0),
        )
        .optional()?;
    Ok(room.map(RoomId::from))
}

/// 一条等着送出去的话。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outbound {
    pub id: i64,
    pub source: Source,
    pub target: String,
    pub body: String,
    pub attempts: u32,
    pub created_at: i64,
}

/// 送完了的、放弃了的记录留多久。
const OUTBOUND_KEEP_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// 记下一条要送出去的话。同一个 `key` 已经记过时不改，返回 `false`。
pub fn enqueue(
    conn: &Connection,
    source: Source,
    target: &str,
    body: &str,
    key: &str,
    now: i64,
) -> Result<bool> {
    let added = conn.execute(
        "INSERT INTO outbound (source, target, body, key, created_at) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (key) DO NOTHING",
        params![source.as_str(), target, body, key, now],
    )?;
    Ok(added > 0)
}

/// 现在该送的，按记下的先后。同一个去处前面还有一条在等重试时，后面的不抢先。
pub fn pending(conn: &Connection, source: Source, now: i64, limit: u32) -> Result<Vec<Outbound>> {
    let mut stmt = conn.prepare(
        "SELECT id, target, body, attempts, created_at FROM outbound AS o
         WHERE state = 'pending' AND source = ?1 AND retry_at <= ?2
           AND NOT EXISTS (
               SELECT 1 FROM outbound AS earlier
               WHERE earlier.state = 'pending' AND earlier.source = o.source
                 AND earlier.target = o.target AND earlier.id < o.id AND earlier.retry_at > ?2)
         ORDER BY id LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![source.as_str(), now, limit], |row| {
        Ok(Outbound {
            id: row.get(0)?,
            source,
            target: row.get(1)?,
            body: row.get(2)?,
            attempts: row.get(3)?,
            created_at: row.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// 这个去处不再送了（连接断开了）：还没送出去的都作废。返回作废了几条。
pub fn cancel_pending(conn: &Connection, source: Source, target: &str, why: &str) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE outbound SET state = 'failed', error = ?3
         WHERE state = 'pending' AND source = ?1 AND target = ?2",
        params![source.as_str(), target, why],
    )?)
}

pub fn mark_sent(conn: &Connection, id: i64, now: i64) -> Result<()> {
    conn.execute(
        "UPDATE outbound SET state = 'sent', sent_at = ?2, error = NULL, attempts = attempts + 1
         WHERE id = ?1",
        params![id, now],
    )?;
    conn.execute(
        "DELETE FROM outbound WHERE state != 'pending' AND created_at < ?1",
        [now - OUTBOUND_KEEP_MS],
    )?;
    Ok(())
}

/// 这一次没送成。`retry_at` 给了就到时候再试，没给就不再试了。
pub fn mark_failed(conn: &Connection, id: i64, error: &str, retry_at: Option<i64>) -> Result<()> {
    conn.execute(
        "UPDATE outbound SET attempts = attempts + 1, error = ?2,
                state = CASE WHEN ?3 IS NULL THEN 'failed' ELSE 'pending' END,
                retry_at = COALESCE(?3, retry_at)
         WHERE id = ?1",
        params![id, error, retry_at],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{rooms, Store};
    use sa_protocol::Room;

    fn room(store: &Store) -> RoomId {
        let room = Room {
            id: RoomId::new(),
            name: "房间".into(),
            host_bot_id: None,
            member_bot_ids: vec![],
            created_at: 1,
            updated_at: 1,
        };
        store.write(|tx| rooms::insert(tx, &room)).unwrap();
        room.id
    }

    fn link(room_id: &RoomId, source: Source, external: &str) -> Link {
        Link {
            id: LinkId::new(),
            source,
            external: external.into(),
            room_id: room_id.clone(),
            title: external.into(),
            created_at: 1,
        }
    }

    #[test]
    fn one_outside_end_connects_to_one_room_and_goes_with_the_room() {
        let store = Store::open_in_memory().unwrap();
        let (a, b) = (room(&store), room(&store));
        let first = link(&a, Source::Github, "me/app");
        assert!(store.write(|tx| insert_link(tx, &first)).unwrap());
        // 同一个仓库不能再连到别的房间；同名的 Telegram 聊天是另一回事。
        let again = link(&b, Source::Github, "me/app");
        assert!(!store.write(|tx| insert_link(tx, &again)).unwrap());
        let chat = link(&b, Source::Telegram, "me/app");
        assert!(store.write(|tx| insert_link(tx, &chat)).unwrap());

        let found = store
            .read(|c| find_link(c, Source::Github, "me/app"))
            .unwrap();
        assert_eq!(found, Some(first.clone()));
        assert_eq!(store.read(list_links).unwrap().len(), 2);
        assert_eq!(
            store.read(|c| room_links(c, &b, Source::Telegram)).unwrap(),
            vec![chat]
        );

        store
            .write(|tx| Ok(tx.execute("DELETE FROM rooms WHERE id = ?1", [b.as_str()])?))
            .unwrap();
        assert_eq!(store.read(list_links).unwrap(), vec![first.clone()]);
        assert!(store.write(|tx| delete_link(tx, &first.id)).unwrap());
        assert!(!store.write(|tx| delete_link(tx, &first.id)).unwrap());
    }

    #[test]
    fn the_same_delivery_is_recorded_once() {
        let store = Store::open_in_memory().unwrap();
        let trigger = NewTrigger {
            source: Source::Github,
            key: "delivery-1",
            kind: "issues",
            stored: false,
            note: "仓库没有连到房间",
            room_id: None,
            message_id: None,
        };
        assert!(store.write(|tx| record_trigger(tx, &trigger, 10)).unwrap());
        assert!(!store.write(|tx| record_trigger(tx, &trigger, 11)).unwrap());
        assert!(store
            .read(|c| has_trigger(c, Source::Github, "delivery-1"))
            .unwrap());
        // 幂等键是每个来源各管各的。
        assert!(!store
            .read(|c| has_trigger(c, Source::Telegram, "delivery-1"))
            .unwrap());
        assert_eq!(store.write(|tx| prune_triggers(tx, 11)).unwrap(), 1);
    }

    #[test]
    fn a_pairing_code_works_once_and_only_before_it_expires() {
        let store = Store::open_in_memory().unwrap();
        let room_id = room(&store);
        store
            .write(|tx| put_pairing(tx, "old", &room_id, 100))
            .unwrap();
        // 新的一个顶掉旧的。
        store
            .write(|tx| put_pairing(tx, "new", &room_id, 100))
            .unwrap();
        assert_eq!(store.write(|tx| take_pairing(tx, "old", 50)).unwrap(), None);
        assert_eq!(
            store.write(|tx| take_pairing(tx, "new", 50)).unwrap(),
            Some(room_id.clone())
        );
        assert_eq!(store.write(|tx| take_pairing(tx, "new", 50)).unwrap(), None);

        store
            .write(|tx| put_pairing(tx, "late", &room_id, 100))
            .unwrap();
        assert_eq!(
            store.write(|tx| take_pairing(tx, "late", 100)).unwrap(),
            None
        );
    }

    #[test]
    fn outbound_is_queued_once_retried_and_given_up() {
        let store = Store::open_in_memory().unwrap();
        let add = |key: &str| {
            store
                .write(|tx| enqueue(tx, Source::Telegram, "42", "你好", key, 1))
                .unwrap()
        };
        assert!(add("msg_1"));
        assert!(!add("msg_1"));
        assert!(add("msg_2"));

        let due = store
            .read(|c| pending(c, Source::Telegram, 10, 10))
            .unwrap();
        assert_eq!(due.len(), 2);
        store.write(|tx| mark_sent(tx, due[0].id, 11)).unwrap();
        // 没送成：到点之前不再拿出来。
        store
            .write(|tx| mark_failed(tx, due[1].id, "超时", Some(100)))
            .unwrap();
        assert!(store
            .read(|c| pending(c, Source::Telegram, 50, 10))
            .unwrap()
            .is_empty());
        let retry = store
            .read(|c| pending(c, Source::Telegram, 100, 10))
            .unwrap();
        assert_eq!(retry.len(), 1);
        assert_eq!(retry[0].attempts, 1);
        // 不再试了。
        store
            .write(|tx| mark_failed(tx, retry[0].id, "被拉黑了", None))
            .unwrap();
        assert!(store
            .read(|c| pending(c, Source::Telegram, 1_000, 10))
            .unwrap()
            .is_empty());
    }
}
