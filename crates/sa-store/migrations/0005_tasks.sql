-- M4：任务、运行、交付物。

CREATE TABLE tasks (
    id                TEXT PRIMARY KEY,
    room_id           TEXT NOT NULL REFERENCES rooms (id) ON DELETE CASCADE,
    title             TEXT NOT NULL,
    scope             TEXT NOT NULL,
    acceptance        TEXT NOT NULL DEFAULT '',
    owner_bot_id      TEXT REFERENCES bots (id) ON DELETE SET NULL,
    state             TEXT NOT NULL CHECK (state IN ('draft', 'open', 'working', 'waiting', 'review', 'blocked', 'done', 'dropped', 'cancelled')),
    -- 对当前状态的一句说明。
    note              TEXT NOT NULL DEFAULT '',
    -- 人给的、工作会话还没读到的话。
    pending_input     TEXT NOT NULL DEFAULT '',
    -- 谁开的：空表示人，否则是 Bot。Bot 被删了也留着这个 id。
    created_by_bot_id TEXT,
    -- 独立工作目录：Bot 在哪个目录里干活、是 git worktree 还是复制出来的、分支、从哪个提交开始。
    workdir           TEXT,
    work_kind         TEXT CHECK (work_kind IN ('worktree', 'copy')),
    branch            TEXT,
    base_ref          TEXT,
    -- 任务目录的根（workdir 可能是它里面的一层）、它的 git 目录、它是从哪个工作目录开出来的。
    work_tree         TEXT,
    git_dir           TEXT,
    source_dir        TEXT,
    rev               INTEGER NOT NULL DEFAULT 0,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL
) STRICT;

CREATE INDEX tasks_room ON tasks (room_id, created_at);
CREATE INDEX tasks_state ON tasks (state);

CREATE TABLE runs (
    id          TEXT PRIMARY KEY,
    task_id     TEXT NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
    room_id     TEXT NOT NULL,
    bot_id      TEXT NOT NULL,
    message_id  TEXT NOT NULL,
    state       TEXT NOT NULL CHECK (state IN ('running', 'succeeded', 'failed', 'cancelled', 'interrupted')),
    attempt     INTEGER NOT NULL,
    started_at  INTEGER NOT NULL,
    ended_at    INTEGER,
    report_json TEXT,
    error       TEXT
) STRICT;

CREATE INDEX runs_task ON runs (task_id, started_at);
-- 一个任务同时最多一次运行在进行。
CREATE UNIQUE INDEX runs_one_running ON runs (task_id) WHERE state = 'running';

CREATE TABLE artifacts (
    id         TEXT PRIMARY KEY,
    task_id    TEXT NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
    run_id     TEXT NOT NULL,
    kind       TEXT NOT NULL CHECK (kind IN ('pr', 'branch', 'file', 'report')),
    reference  TEXT NOT NULL,
    summary    TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL
) STRICT;

CREATE INDEX artifacts_task ON artifacts (task_id, created_at);

-- 每个任务一个工作会话。
ALTER TABLE sessions ADD COLUMN task_id TEXT;
CREATE UNIQUE INDEX sessions_work ON sessions (task_id) WHERE layer = 'work';

-- 时间线上的任务卡、运行过程、交付卡也是消息。
ALTER TABLE messages ADD COLUMN kind TEXT NOT NULL DEFAULT 'chat';
ALTER TABLE messages ADD COLUMN task_id TEXT;
ALTER TABLE messages ADD COLUMN run_id TEXT;
