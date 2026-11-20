-- 用量直接记是哪个任务花的。以前靠会话找任务，Bot 被删掉时会话跟着删，任务的花费就找不到了。
ALTER TABLE usage ADD COLUMN task_id TEXT;
UPDATE usage SET task_id = (SELECT s.task_id FROM sessions s WHERE s.id = usage.session_id);
CREATE INDEX usage_task ON usage (task_id);
