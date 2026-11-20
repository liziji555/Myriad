-- M5：花费和提醒。

-- 这次调用按当时的价格算下来花了多少，单位是百万分之一个货币单位。老的记录没有价格，记 0。
ALTER TABLE usage ADD COLUMN cost_micros INTEGER NOT NULL DEFAULT 0;
CREATE INDEX usage_session ON usage (session_id);

-- 人上一次让任务继续时它已经花了多少。每个任务的花费上限从这里往上算。
ALTER TABLE tasks ADD COLUMN spend_floor INTEGER NOT NULL DEFAULT 0;

-- 给人看的提醒（花费快到上限、任务卡住了……）。key 相同的只记一次。
CREATE TABLE notices (
    id           TEXT PRIMARY KEY,
    key          TEXT NOT NULL UNIQUE,
    kind         TEXT NOT NULL CHECK (kind IN ('budget', 'stuck_run', 'stuck_task', 'loop', 'spend', 'backlog', 'failures', 'storage')),
    level        TEXT NOT NULL CHECK (level IN ('info', 'warning')),
    room_id      TEXT REFERENCES rooms (id) ON DELETE CASCADE,
    task_id      TEXT REFERENCES tasks (id) ON DELETE CASCADE,
    text         TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    dismissed_at INTEGER
) STRICT;

CREATE INDEX notices_open ON notices (created_at) WHERE dismissed_at IS NULL;
