-- M6a：外面的世界（GitHub 事件、Telegram）。

-- 从外面来的消息是从哪里来的（JSON）。界面里直接发的是 NULL。
ALTER TABLE messages ADD COLUMN origin TEXT;

-- 房间和外面一个地方的连接。
CREATE TABLE links (
    id         TEXT PRIMARY KEY,
    source     TEXT NOT NULL CHECK (source IN ('github', 'telegram')),
    -- 外面那一头：GitHub 是小写的 owner/name，Telegram 是聊天 id。一头只连一个房间。
    external   TEXT NOT NULL,
    room_id    TEXT NOT NULL REFERENCES rooms (id) ON DELETE CASCADE,
    title      TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (source, external)
) STRICT;

CREATE INDEX links_room ON links (room_id);

-- 外面送来的每一件事都先记在这里。同一件事（同一个来源、同一个幂等键）只收一次。
CREATE TABLE triggers (
    id         INTEGER PRIMARY KEY,
    source     TEXT NOT NULL,
    -- 来源给的幂等键：GitHub 的 delivery id，Telegram 的 update id。
    key        TEXT NOT NULL,
    kind       TEXT NOT NULL,
    outcome    TEXT NOT NULL CHECK (outcome IN ('stored', 'filtered')),
    -- 没收下的原因。
    note       TEXT NOT NULL DEFAULT '',
    room_id    TEXT,
    message_id TEXT,
    created_at INTEGER NOT NULL,
    UNIQUE (source, key)
) STRICT;

CREATE INDEX triggers_time ON triggers (created_at);

-- Telegram 配对码。只存哈希；一个房间同一时刻只有一个，用过就删。
CREATE TABLE pairings (
    code_hash  TEXT PRIMARY KEY,
    room_id    TEXT NOT NULL UNIQUE REFERENCES rooms (id) ON DELETE CASCADE,
    expires_at INTEGER NOT NULL
) STRICT;

-- 要送到外面去的话。先记下再送；同一个 key 只记一次，所以不会送两遍。
CREATE TABLE outbound (
    id         INTEGER PRIMARY KEY,
    source     TEXT NOT NULL,
    -- 送到哪里：Telegram 的聊天 id。
    target     TEXT NOT NULL,
    body       TEXT NOT NULL,
    key        TEXT NOT NULL UNIQUE,
    state      TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'sent', 'failed')),
    attempts   INTEGER NOT NULL DEFAULT 0,
    -- 下一次什么时候再试。
    retry_at   INTEGER NOT NULL DEFAULT 0,
    error      TEXT,
    created_at INTEGER NOT NULL,
    sent_at    INTEGER
) STRICT;

CREATE INDEX outbound_pending ON outbound (retry_at) WHERE state = 'pending';
