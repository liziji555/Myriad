-- 待发队列：Bot 忙的时候人说的话先排着，房间闲下来按顺序一条一条发出去。
CREATE TABLE queued_inputs (
    id         TEXT PRIMARY KEY,
    room_id    TEXT NOT NULL REFERENCES rooms (id) ON DELETE CASCADE,
    content    TEXT NOT NULL,
    position   INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

CREATE INDEX queued_inputs_room ON queued_inputs (room_id, position);
