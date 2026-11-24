-- harness 工位开在哪个共享目录（sa share）那台电脑上。空表示开在服务这边。
ALTER TABLE harness_desks ADD COLUMN share TEXT;
