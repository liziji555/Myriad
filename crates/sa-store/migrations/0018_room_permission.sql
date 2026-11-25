-- 房间里的 Bot 要改文件、执行命令时问不问人：ask 每次都问，full 不问直接做。
ALTER TABLE rooms ADD COLUMN permission TEXT NOT NULL DEFAULT 'ask';
