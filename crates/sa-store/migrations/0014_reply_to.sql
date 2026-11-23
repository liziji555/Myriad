-- 这条消息是在回哪几条（消息 id 的 JSON 数组）。
ALTER TABLE messages ADD COLUMN reply_to TEXT NOT NULL DEFAULT '[]';
