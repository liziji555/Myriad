-- 房间时间线。seq 是房间内（也是全局）的先后顺序和翻页游标。
CREATE TABLE messages (
    seq           INTEGER PRIMARY KEY AUTOINCREMENT,
    id            TEXT NOT NULL UNIQUE,
    room_id       TEXT NOT NULL REFERENCES rooms (id) ON DELETE CASCADE,
    sender_kind   TEXT NOT NULL CHECK (sender_kind IN ('user', 'bot')),
    sender_bot_id TEXT REFERENCES bots (id) ON DELETE SET NULL,
    content       TEXT NOT NULL,
    steps_json    TEXT NOT NULL DEFAULT '[]',
    state         TEXT NOT NULL CHECK (state IN ('streaming', 'complete', 'cancelled', 'interrupted', 'failed')),
    error         TEXT,
    rev           INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
) STRICT;

CREATE INDEX messages_room_seq ON messages (room_id, seq);

-- 会话索引。对话记录本身在 sessions/<id>.jsonl 里。
CREATE TABLE sessions (
    id         TEXT PRIMARY KEY,
    room_id    TEXT NOT NULL REFERENCES rooms (id) ON DELETE CASCADE,
    bot_id     TEXT NOT NULL REFERENCES bots (id) ON DELETE CASCADE,
    layer      TEXT NOT NULL CHECK (layer IN ('front', 'work', 'sub')),
    created_at INTEGER NOT NULL
) STRICT;

-- 每个房间里每个 Bot 只有一个入口会话。
CREATE UNIQUE INDEX sessions_front ON sessions (room_id, bot_id) WHERE layer = 'front';

-- 每次模型调用记一行。
CREATE TABLE usage (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    room_id       TEXT,
    bot_id        TEXT,
    session_id    TEXT,
    model         TEXT NOT NULL,
    in_tokens     INTEGER NOT NULL,
    out_tokens    INTEGER NOT NULL,
    cached_tokens INTEGER NOT NULL,
    created_at    INTEGER NOT NULL
) STRICT;

CREATE INDEX usage_created ON usage (created_at);
