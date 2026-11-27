use crate::{Result, StoreError};
use rusqlite::{params, Connection, OptionalExtension, Row};
use sa_protocol::{
    BotId, Message, MessageId, MessageKind, MessageState, RoomId, RunId, Sender, TaskId, ToolStep,
};

const COLUMNS: &str =
    "seq, id, room_id, sender_kind, sender_bot_id, content, steps_json, state, error, rev, created_at, updated_at, mentions_json, hop, kind, task_id, run_id, origin, reply_to";

/// 写入一条新消息，返回数据库分配的 `seq`。传入的 `message.seq` 被忽略。
pub fn insert(conn: &Connection, message: &Message) -> Result<i64> {
    let (kind, bot_id) = match &message.sender {
        Sender::User => ("user", None),
        // 空 id 表示这个 Bot 已经被删了：和删 Bot 之后留下的消息一样，不指向任何 Bot。
        Sender::Bot { bot_id } => ("bot", Some(bot_id.as_str()).filter(|id| !id.is_empty())),
    };
    conn.execute(
        "INSERT INTO messages (id, room_id, sender_kind, sender_bot_id, content, steps_json, state, error, rev, created_at, updated_at, mentions_json, hop, kind, task_id, run_id, origin, reply_to)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        params![
            message.id.as_str(),
            message.room_id.as_str(),
            kind,
            bot_id,
            message.content,
            steps_to_json(&message.steps),
            message.state.as_str(),
            message.error,
            message.rev,
            message.created_at,
            message.updated_at,
            serde_json::to_string(&message.mentions).expect("Bot id 列表总能序列化成 JSON"),
            message.hop,
            message.kind.as_str(),
            message.task_id.as_ref().map(TaskId::as_str),
            message.run_id.as_ref().map(RunId::as_str),
            message
                .origin
                .as_ref()
                .map(|origin| serde_json::to_string(origin).expect("出处总能序列化成 JSON")),
            serde_json::to_string(&message.reply_to).expect("消息 id 列表总能序列化成 JSON"),
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 用 `message` 的可变部分（内容、步骤、状态、错误、版本）覆盖已有的那一行。
pub fn update(conn: &Connection, message: &Message) -> Result<()> {
    let changed = conn.execute(
        "UPDATE messages SET content = ?2, steps_json = ?3, state = ?4, error = ?5, rev = ?6, updated_at = ?7 WHERE id = ?1",
        params![
            message.id.as_str(),
            message.content,
            steps_to_json(&message.steps),
            message.state.as_str(),
            message.error,
            message.rev,
            message.updated_at
        ],
    )?;
    if changed == 0 {
        return Err(StoreError::Corrupt(format!("消息 {} 不存在", message.id)));
    }
    Ok(())
}

pub fn get(conn: &Connection, id: &MessageId) -> Result<Option<Message>> {
    let row = conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM messages WHERE id = ?1"),
            [id.as_str()],
            read_row,
        )
        .optional()?;
    row.transpose()
}

/// 房间里 `seq` 小于 `before` 的最近 `limit` 条，按 `seq` 升序返回。
pub fn list_before(
    conn: &Connection,
    room_id: &RoomId,
    before: Option<i64>,
    limit: u32,
) -> Result<Vec<Message>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM messages WHERE room_id = ?1 AND seq < ?2 ORDER BY seq DESC LIMIT ?3"
    ))?;
    let rows = stmt.query_map(
        params![room_id.as_str(), before.unwrap_or(i64::MAX), limit],
        read_row,
    )?;
    let mut messages = rows.map(|row| row?).collect::<Result<Vec<_>>>()?;
    messages.reverse();
    Ok(messages)
}

/// 房间里 `seq` 大于 `after` 的、人说的话（不算人开的任务卡），按 `seq` 升序。
pub fn list_user_after(conn: &Connection, room_id: &RoomId, after: i64) -> Result<Vec<Message>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM messages WHERE room_id = ?1 AND seq > ?2 AND sender_kind = 'user' AND kind IN ('chat', 'routine', 'event') ORDER BY seq"
    ))?;
    let rows = stmt.query_map(params![room_id.as_str(), after], read_row)?;
    rows.map(|row| row?).collect()
}

/// 房间里 `seq` 大于 `after` 的全部消息（人和 Bot 的都算），按 `seq` 升序。
pub fn list_after(conn: &Connection, room_id: &RoomId, after: i64) -> Result<Vec<Message>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM messages WHERE room_id = ?1 AND seq > ?2 ORDER BY seq"
    ))?;
    let rows = stmt.query_map(params![room_id.as_str(), after], read_row)?;
    rows.map(|row| row?).collect()
}

/// 删掉一条消息。返回是否真的删了。
pub fn delete(conn: &Connection, id: &MessageId) -> Result<bool> {
    Ok(conn.execute("DELETE FROM messages WHERE id = ?1", [id.as_str()])? > 0)
}

/// 所有还停在某个状态的消息。
pub fn list_in_state(conn: &Connection, state: MessageState) -> Result<Vec<Message>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM messages WHERE state = ?1 ORDER BY seq"
    ))?;
    let rows = stmt.query_map([state.as_str()], read_row)?;
    rows.map(|row| row?).collect()
}

/// `seq` 之前最近的一条可能引出 `bot_id` 这句回复（它是转到第 `hop` 手说的）的话。
///
/// - 界面里人说的话：不管点没点名都算。房间里的话每个 Bot 都看得到，点了别人的话也可能写进这句回复，
///   所以不能跳过它、算到更早的外部事件头上（那样会把网页上的话转发到外面）。
/// - 外面（Telegram）来的人说的话：没点名的谁都可能来答；点了名的，只有被点到的 Bot 会直接来答，
///   别的 Bot 要经过交接（`hop > 0`）才会接着说。否则回界面里的话会被送到那个聊天里去。
/// - 定时任务、外面来的事：只算指定交给它的。交给别的 Bot 的、谁都没交给的（只摆在时间线上的事）不算。
pub fn last_trigger_before(
    conn: &Connection,
    room_id: &RoomId,
    seq: i64,
    bot_id: &BotId,
    hop: u32,
) -> Result<Option<Message>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM messages
         WHERE room_id = ?1 AND seq < ?2 AND sender_kind = 'user'
           AND ((kind = 'chat'
                 AND (origin IS NULL OR ?4 > 0 OR mentions_json = '[]'
                      OR EXISTS (SELECT 1 FROM json_each(mentions_json) WHERE value = ?3)))
                OR (kind IN ('routine', 'event') AND json_extract(mentions_json, '$[0]') = ?3))
         ORDER BY seq DESC LIMIT 1"
    );
    conn.query_row(
        &sql,
        params![room_id.as_str(), seq, bot_id.as_str(), hop],
        read_row,
    )
    .optional()?
    .transpose()
}

fn steps_to_json(steps: &[ToolStep]) -> String {
    serde_json::to_string(steps).expect("步骤总能序列化成 JSON")
}

fn read_row(row: &Row<'_>) -> rusqlite::Result<Result<Message>> {
    let seq: i64 = row.get(0)?;
    let id: String = row.get(1)?;
    let sender_kind: String = row.get(3)?;
    let sender_bot_id: Option<String> = row.get(4)?;
    let steps_json: String = row.get(6)?;
    let state: String = row.get(7)?;
    let mentions_json: String = row.get(12)?;
    let kind: String = row.get(14)?;
    let origin_json: Option<String> = row.get(17)?;
    let reply_to_json: String = row.get(18)?;
    let message = (|| {
        let sender = match (sender_kind.as_str(), sender_bot_id) {
            ("user", _) => Sender::User,
            // Bot 被删掉后 sender_bot_id 变空，消息仍然保留。
            ("bot", bot_id) => Sender::Bot {
                bot_id: BotId::from(bot_id.unwrap_or_default()),
            },
            (other, _) => {
                return Err(StoreError::Corrupt(format!(
                    "消息 {id} 的发送者类型未知：{other}"
                )))
            }
        };
        let steps = serde_json::from_str(&steps_json)
            .map_err(|e| StoreError::Corrupt(format!("消息 {id} 的步骤不是合法 JSON：{e}")))?;
        let state = MessageState::parse(&state)
            .ok_or_else(|| StoreError::Corrupt(format!("消息 {id} 的状态未知：{state}")))?;
        let mentions = serde_json::from_str(&mentions_json)
            .map_err(|e| StoreError::Corrupt(format!("消息 {id} 的点名不是合法 JSON：{e}")))?;
        let kind = MessageKind::parse(&kind)
            .ok_or_else(|| StoreError::Corrupt(format!("消息 {id} 的种类未知：{kind}")))?;
        let origin = origin_json
            .map(|json| serde_json::from_str(&json))
            .transpose()
            .map_err(|e| StoreError::Corrupt(format!("消息 {id} 的出处不是合法 JSON：{e}")))?;
        let reply_to = serde_json::from_str(&reply_to_json).map_err(|e| {
            StoreError::Corrupt(format!("消息 {id} 回的是哪几条不是合法 JSON：{e}"))
        })?;
        Ok((sender, steps, state, mentions, kind, origin, reply_to))
    })();
    let (sender, steps, state, mentions, kind, origin, reply_to) = match message {
        Ok(parts) => parts,
        Err(e) => return Ok(Err(e)),
    };
    Ok(Ok(Message {
        id: MessageId::from(id),
        seq,
        room_id: RoomId::from(row.get::<_, String>(2)?),
        sender,
        content: row.get(5)?,
        mentions,
        hop: row.get(13)?,
        reply_to,
        kind,
        task_id: row.get::<_, Option<String>>(15)?.map(TaskId::from),
        run_id: row.get::<_, Option<String>>(16)?.map(RunId::from),
        origin,
        steps,
        state,
        error: row.get(8)?,
        rev: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bots, rooms, Store};
    use sa_protocol::{Bot, BotKind, ModelTier, Room, StepState};

    fn seed(store: &Store) -> (RoomId, BotId) {
        let bot = Bot {
            id: BotId::new(),
            name: "助手".into(),
            kind: BotKind::Assistant,
            tier: ModelTier::Mid,
            persona: String::new(),
            enabled: true,
            created_at: 1,
            harness: Default::default(),
            harness_command: String::new(),
            is_master: true,
            role: String::new(),
            avatar_shape: String::new(),
            avatar_color: String::new(),
            avatar_image: String::new(),
        };
        let room = Room {
            discussion: Default::default(),
            rounds: sa_protocol::DEFAULT_ROUNDS,
            share: None,
            permission: Default::default(),
            id: RoomId::new(),
            name: "房间".into(),
            host_bot_id: Some(bot.id.clone()),
            member_bot_ids: vec![bot.id.clone()],
            created_at: 1,
            updated_at: 1,
        };
        store
            .write(|tx| {
                bots::insert(tx, &bot)?;
                rooms::insert(tx, &room)
            })
            .unwrap();
        (room.id, bot.id)
    }

    fn message(room_id: &RoomId, sender: Sender, content: &str) -> Message {
        Message {
            id: MessageId::new(),
            seq: 0,
            room_id: room_id.clone(),
            sender,
            content: content.into(),
            mentions: vec![],
            hop: 0,
            reply_to: vec![],
            kind: MessageKind::Chat,
            task_id: None,
            run_id: None,
            origin: None,
            steps: vec![],
            state: MessageState::Complete,
            error: None,
            rev: 0,
            created_at: 5,
            updated_at: 5,
        }
    }

    #[test]
    fn insert_update_and_page_backwards() {
        let store = Store::open_in_memory().unwrap();
        let (room, bot) = seed(&store);
        let mut seqs = vec![];
        for i in 0..5 {
            let m = message(&room, Sender::User, &format!("第 {i} 条"));
            seqs.push(store.write(|tx| insert(tx, &m)).unwrap());
        }
        assert!(seqs.windows(2).all(|w| w[0] < w[1]));

        let latest = store.read(|c| list_before(c, &room, None, 2)).unwrap();
        assert_eq!(
            latest.iter().map(|m| m.seq).collect::<Vec<_>>(),
            seqs[3..].to_vec()
        );
        let older = store
            .read(|c| list_before(c, &room, Some(seqs[3]), 10))
            .unwrap();
        assert_eq!(
            older.iter().map(|m| m.seq).collect::<Vec<_>>(),
            seqs[..3].to_vec()
        );

        let mut reply = message(&room, Sender::Bot { bot_id: bot }, "");
        reply.state = MessageState::Streaming;
        store.write(|tx| insert(tx, &reply)).unwrap();
        reply.content = "好的".into();
        reply.state = MessageState::Complete;
        reply.rev = 3;
        reply.steps = vec![ToolStep {
            id: "call_1".into(),
            tool: "read".into(),
            title: "读取 a.txt".into(),
            state: StepState::Ok,
            approval: None,
            output: None,
        }];
        store.write(|tx| update(tx, &reply)).unwrap();
        let stored = store.read(|c| get(c, &reply.id)).unwrap().unwrap();
        assert_eq!(stored.content, "好的");
        assert_eq!(stored.steps, reply.steps);
        assert_eq!(stored.rev, 3);
        assert_eq!(stored.sender, reply.sender);

        assert_eq!(
            store
                .read(|c| list_user_after(c, &room, seqs[2]))
                .unwrap()
                .len(),
            2
        );
        assert!(store
            .read(|c| list_in_state(c, MessageState::Streaming))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn update_of_missing_message_is_an_error() {
        let store = Store::open_in_memory().unwrap();
        let (room, _) = seed(&store);
        let ghost = message(&room, Sender::User, "x");
        assert!(store.write(|tx| update(tx, &ghost)).is_err());
    }
}
