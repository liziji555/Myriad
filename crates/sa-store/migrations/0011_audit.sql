-- M7a：审计日志。谁在什么时候做了一件要留底的事：审批、执行命令、推送、改 Bot、验收任务……
-- 只追加。房间、任务、Bot 删掉之后记录还在，所以这里不用外键。
CREATE TABLE audit (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    at         INTEGER NOT NULL,
    actor      TEXT NOT NULL CHECK (actor IN ('user', 'bot', 'system', 'outside')),
    actor_name TEXT NOT NULL DEFAULT '',
    action     TEXT NOT NULL,
    room_id    TEXT,
    task_id    TEXT,
    summary    TEXT NOT NULL,
    detail     TEXT
) STRICT;

CREATE INDEX audit_room ON audit (room_id, id);
CREATE INDEX audit_task ON audit (task_id, id);
CREATE INDEX audit_at ON audit (at);
