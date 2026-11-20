-- M4b：对外面做过的事（推送分支、开 PR）。同一件事只做一次：做之前先查，做完记下结果。

CREATE TABLE effects (
    -- 这件事的名字，例如 `open_pr:<任务 id>`。
    key        TEXT PRIMARY KEY,
    task_id    TEXT NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
    kind       TEXT NOT NULL,
    -- 做完得到的结果（JSON）。
    result     TEXT NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;

CREATE INDEX effects_task ON effects (task_id, created_at);
