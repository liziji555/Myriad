CREATE TABLE bots (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    kind       TEXT NOT NULL CHECK (kind IN ('assistant', 'router', 'worker', 'group', 'watchdog')),
    tier       TEXT NOT NULL CHECK (tier IN ('cheap', 'mid', 'strong')),
    enabled    INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    created_at INTEGER NOT NULL
) STRICT;

CREATE TABLE rooms (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    host_bot_id TEXT REFERENCES bots (id) ON DELETE SET NULL,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
) STRICT;

CREATE TABLE room_members (
    room_id   TEXT NOT NULL REFERENCES rooms (id) ON DELETE CASCADE,
    bot_id    TEXT NOT NULL REFERENCES bots (id) ON DELETE CASCADE,
    joined_at INTEGER NOT NULL,
    PRIMARY KEY (room_id, bot_id)
) STRICT, WITHOUT ROWID;

-- 追加式事件日志。seq 是全局游标：SSE 断线后按它补发。
CREATE TABLE events (
    seq          INTEGER PRIMARY KEY AUTOINCREMENT,
    room_id      TEXT,
    kind         TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    created_at   INTEGER NOT NULL
) STRICT;

CREATE INDEX events_room_seq ON events (room_id, seq);
