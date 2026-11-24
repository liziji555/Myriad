-- 房间用哪个共享目录（sa share）当工作目录。空表示用服务自己的工作目录。
ALTER TABLE rooms ADD COLUMN share TEXT;
