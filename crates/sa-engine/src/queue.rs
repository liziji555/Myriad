//! 待发队列：Bot 忙的时候人说的话先排着，可以改字、挪位置、取消；房间闲下来由编排层一条一条发出去。

use crate::messages::{add_chat, check_said};
use crate::{now_ms, to_json, Engine, EngineError, Result};
use sa_protocol::{
    EditQueuedRequest, EventKind, Message, MessageState, QueuedId, QueuedInput, RoomId, RoomQueue,
    Sender,
};
use sa_store::{events, queue, rooms, Transaction};

/// 一个房间最多排多少句。
pub const MAX_QUEUED: usize = 20;

fn snapshot(
    tx: &Transaction<'_>,
    room_id: &RoomId,
    now: i64,
) -> sa_store::Result<(RoomQueue, i64)> {
    let queue = RoomQueue {
        room_id: room_id.clone(),
        items: queue::list(tx, room_id)?,
    };
    let seq = events::append(
        tx,
        Some(room_id),
        EventKind::QueueUpdated,
        &to_json(&queue),
        now,
    )?;
    Ok((queue, seq))
}

fn gone() -> EngineError {
    EngineError::NotFound("这句已经发出去或者取消了".into())
}

impl Engine {
    /// 排一句到队尾。
    pub fn queue_input(&self, room_id: &RoomId, content: &str) -> Result<RoomQueue> {
        check_said(content)?;
        let now = now_ms();
        let (queue, seq) = self.store.write(|tx| {
            if rooms::get(tx, room_id)?.is_none() {
                return Ok(Err(EngineError::NotFound("没有这个房间".into())));
            }
            if queue::list(tx, room_id)?.len() >= MAX_QUEUED {
                return Ok(Err(EngineError::Invalid(format!(
                    "最多排 {MAX_QUEUED} 句，先等前面的发出去"
                ))));
            }
            let item = QueuedInput {
                id: QueuedId::new(),
                room_id: room_id.clone(),
                content: content.into(),
                created_at: now,
                updated_at: now,
            };
            queue::push(tx, &item)?;
            snapshot(tx, room_id, now).map(Ok)
        })??;
        self.publish(seq);
        Ok(queue)
    }

    pub fn list_queue(&self, room_id: &RoomId) -> Result<RoomQueue> {
        self.get_room(room_id)?;
        Ok(RoomQueue {
            room_id: room_id.clone(),
            items: self.store.read(|conn| queue::list(conn, room_id))?,
        })
    }

    /// 有排着的话的房间。
    pub fn queued_rooms(&self) -> Result<Vec<RoomId>> {
        Ok(self.store.read(queue::rooms)?)
    }

    /// 改字、挪一位。已经发出去的改不了。
    pub fn edit_queued(&self, id: &QueuedId, edit: &EditQueuedRequest) -> Result<RoomQueue> {
        if let Some(content) = &edit.content {
            check_said(content)?;
        }
        let now = now_ms();
        let (queue, seq) = self.store.write(|tx| {
            let Some(item) = queue::get(tx, id)? else {
                return Ok(Err(gone()));
            };
            if let Some(content) = &edit.content {
                queue::set_content(tx, id, content, now)?;
            }
            if let Some(offset) = edit.offset.filter(|offset| *offset != 0) {
                queue::shift(tx, id, offset)?;
            }
            snapshot(tx, &item.room_id, now).map(Ok)
        })??;
        self.publish(seq);
        Ok(queue)
    }

    /// 取消一句。已经发出去的取消不了。
    pub fn cancel_queued(&self, id: &QueuedId) -> Result<RoomQueue> {
        let now = now_ms();
        let (queue, seq) = self.store.write(|tx| {
            let Some(item) = queue::get(tx, id)? else {
                return Ok(Err(gone()));
            };
            queue::delete(tx, id)?;
            snapshot(tx, &item.room_id, now).map(Ok)
        })??;
        self.publish(seq);
        Ok(queue)
    }

    /// 把排在最前面的一句发出去：出队和变成房间里的消息在同一个事务里，不会发两遍，也不会丢。
    pub fn dispatch_queued(&self, room_id: &RoomId) -> Result<Option<Message>> {
        let now = now_ms();
        let sent = self.store.write(|tx| {
            let Some(item) = queue::list(tx, room_id)?.into_iter().next() else {
                return Ok(None);
            };
            queue::delete(tx, &item.id)?;
            let Some((message, _)) = add_chat(
                tx,
                room_id,
                Sender::User,
                item.content,
                MessageState::Complete,
                None,
                0,
                now,
            )?
            else {
                return Ok(None);
            };
            let (_, seq) = snapshot(tx, room_id, now)?;
            Ok(Some((message, seq)))
        })?;
        Ok(sent.map(|(message, seq)| {
            self.publish(seq);
            message
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sa_store::Store;

    fn engine_with_room() -> (Engine, RoomId) {
        let engine = Engine::new(Store::open_in_memory().unwrap()).unwrap();
        engine.ensure_defaults().unwrap();
        let room = engine.list_rooms().unwrap().remove(0);
        (engine, room.id)
    }

    #[test]
    fn queued_words_go_out_in_order_once_and_can_be_edited_moved_and_cancelled() {
        let (engine, room_id) = engine_with_room();
        let first = engine.queue_input(&room_id, "一").unwrap().items[0]
            .id
            .clone();
        engine.queue_input(&room_id, "二").unwrap();
        let third = engine.queue_input(&room_id, "三").unwrap().items[2]
            .id
            .clone();
        assert!(engine.queue_input(&room_id, "  ").is_err());

        let edit = |content: Option<&str>, offset: Option<i32>| EditQueuedRequest {
            content: content.map(Into::into),
            offset,
        };
        engine
            .edit_queued(&third, &edit(Some("三改"), Some(-1)))
            .unwrap();
        let queue = engine.cancel_queued(&first).unwrap();
        let contents: Vec<_> = queue.items.iter().map(|q| q.content.as_str()).collect();
        assert_eq!(contents, ["三改", "二"]);
        assert!(engine.cancel_queued(&first).is_err());

        let sent = engine.dispatch_queued(&room_id).unwrap().unwrap();
        assert_eq!(sent.content, "三改");
        assert_eq!(sent.sender, Sender::User);
        assert!(engine
            .edit_queued(&third, &edit(Some("晚了"), None))
            .is_err());
        assert_eq!(
            engine.dispatch_queued(&room_id).unwrap().unwrap().content,
            "二"
        );
        assert!(engine.dispatch_queued(&room_id).unwrap().is_none());
        assert!(engine.queued_rooms().unwrap().is_empty());
        let messages = engine.list_messages(&room_id, None, 10).unwrap().messages;
        assert_eq!(messages.iter().filter(|m| m.content == "三改").count(), 1);
    }
}
