-- Bot 身份与形象字段：持久化中枢主控标识、职责短标签、自建参数化造型/配色与自定义头像。
ALTER TABLE bots ADD COLUMN is_master INTEGER NOT NULL DEFAULT 0;
ALTER TABLE bots ADD COLUMN role TEXT NOT NULL DEFAULT '';
ALTER TABLE bots ADD COLUMN avatar_shape TEXT NOT NULL DEFAULT '';
ALTER TABLE bots ADD COLUMN avatar_color TEXT NOT NULL DEFAULT '';
ALTER TABLE bots ADD COLUMN avatar_image TEXT NOT NULL DEFAULT '';

-- 兼容旧库升级：将最早创建的已启用 Bot（若无则最早创建的 Bot）设为中枢主控。
UPDATE bots
SET is_master = 1,
    role = CASE WHEN role = '' THEN '主控中枢' ELSE role END,
    avatar_shape = CASE WHEN avatar_shape = '' THEN 'orb' ELSE avatar_shape END,
    avatar_color = CASE WHEN avatar_color = '' THEN 'obsidian' ELSE avatar_color END
WHERE id = (
    SELECT id FROM bots ORDER BY enabled DESC, created_at ASC, id ASC LIMIT 1
);
