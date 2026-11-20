-- 定时任务：到点让一个 Bot 在房间里做一件事。
CREATE TABLE routines (
    id          TEXT PRIMARY KEY,
    room_id     TEXT NOT NULL REFERENCES rooms (id) ON DELETE CASCADE,
    bot_id      TEXT NOT NULL REFERENCES bots (id) ON DELETE CASCADE,
    title       TEXT NOT NULL,
    prompt      TEXT NOT NULL,
    schedule    TEXT NOT NULL,
    enabled     INTEGER NOT NULL DEFAULT 1,
    next_at     INTEGER,
    last_at     INTEGER,
    last_result TEXT CHECK (last_result IN ('spoke', 'silent', 'failed', 'skipped')),
    last_note   TEXT NOT NULL DEFAULT '',
    failures    INTEGER NOT NULL DEFAULT 0,
    -- 定时器只有一个主人：谁抢到租约谁来做这一次，租约过期了别人才能接手。
    lease_owner TEXT,
    lease_until INTEGER,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
) STRICT;

CREATE INDEX routines_due ON routines (next_at) WHERE enabled = 1;
