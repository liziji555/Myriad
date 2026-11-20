-- 「总是允许」：这个房间里，这个工具做这件事不用再问人。
CREATE TABLE permission_rules (
    id         TEXT PRIMARY KEY,
    room_id    TEXT NOT NULL REFERENCES rooms (id) ON DELETE CASCADE,
    tool       TEXT NOT NULL,
    pattern    TEXT NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;

CREATE UNIQUE INDEX permission_rules_key ON permission_rules (room_id, tool, pattern);
