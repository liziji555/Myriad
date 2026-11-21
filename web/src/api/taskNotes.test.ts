import { describe, expect, it } from "vitest";
import { taskDecision } from "./taskNotes";

describe("taskDecision", () => {
  it("开任务的卡片不是决定", () => {
    expect(taskDecision("开了任务「修 bug」：把登录页修好")).toBeNull();
  });

  it("验收、打回、取消的那句话原样给出", () => {
    for (const said of ["验收了任务「修 bug」，改动已经带回工作目录。", "打回了任务「修 bug」：还差测试", "取消了任务「修 bug」"]) {
      expect(taskDecision(said)).toBe(said);
    }
  });
});
