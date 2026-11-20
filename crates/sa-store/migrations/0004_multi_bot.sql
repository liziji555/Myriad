-- Bot 的人设和分工，接在系统提示词后面。
ALTER TABLE bots ADD COLUMN persona TEXT NOT NULL DEFAULT '';

-- 消息点名了哪些 Bot（Bot id 的 JSON 数组），以及 Bot 之间转了几手。
ALTER TABLE messages ADD COLUMN mentions_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE messages ADD COLUMN hop INTEGER NOT NULL DEFAULT 0;
