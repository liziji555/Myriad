-- Bot 由谁来干活：builtin 直接调模型，其他是本机的 harness 命令行。
ALTER TABLE bots ADD COLUMN harness TEXT NOT NULL DEFAULT 'builtin';
ALTER TABLE bots ADD COLUMN harness_command TEXT NOT NULL DEFAULT '';

-- harness Bot 在每个房间里的工位：自己的独立目录、harness 那边的会话号、目录里改了哪些文件。
CREATE TABLE harness_desks (
    room_id TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    bot_id TEXT NOT NULL REFERENCES bots(id) ON DELETE CASCADE,
    workdir TEXT,
    work_kind TEXT,
    branch TEXT,
    base_ref TEXT,
    work_tree TEXT,
    git_dir TEXT,
    source_dir TEXT,
    session TEXT,
    changes TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY (room_id, bot_id)
);
