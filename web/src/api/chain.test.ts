import { describe, expect, it } from "vitest";
import { chainParts } from "./chain";

describe("chainParts", () => {
  it("同一个任务挨着的几条连成一串", () => {
    const parts = chainParts([
      { id: "a" },
      { id: "b", task_id: "t1" },
      { id: "c", task_id: "t1" },
      { id: "d", task_id: "t1" },
      { id: "e" },
    ]);
    expect([...parts]).toEqual([
      ["b", "start"],
      ["c", "mid"],
      ["d", "end"],
    ]);
  });

  it("只有一条的不算一串", () => {
    expect(chainParts([{ id: "a", task_id: "t1" }, { id: "b" }]).size).toBe(0);
  });

  it("中间隔了别的话就断开", () => {
    const parts = chainParts([
      { id: "a", task_id: "t1" },
      { id: "b" },
      { id: "c", task_id: "t1" },
    ]);
    expect(parts.size).toBe(0);
  });

  it("两个任务挨着时各连各的", () => {
    const parts = chainParts([
      { id: "a", task_id: "t1" },
      { id: "b", task_id: "t1" },
      { id: "c", task_id: "t2" },
      { id: "d", task_id: "t2" },
    ]);
    expect([...parts.values()]).toEqual(["start", "end", "start", "end"]);
  });

  it("没有任务的消息不进串", () => {
    expect(chainParts([{ id: "a", task_id: null }, { id: "b", task_id: null }]).size).toBe(0);
  });
});
