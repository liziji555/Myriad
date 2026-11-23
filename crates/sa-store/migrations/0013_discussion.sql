-- 房间里没点名的话怎么接：normal 挑一个、parallel 各答一份、rounds 轮流说 rounds 圈、blind 各答一份互相看不到。
ALTER TABLE rooms ADD COLUMN discussion TEXT NOT NULL DEFAULT 'normal';
ALTER TABLE rooms ADD COLUMN rounds INTEGER NOT NULL DEFAULT 2;
