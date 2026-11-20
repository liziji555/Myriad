import { describe, expect, it } from "vitest";
import { buildGraph, freshNodes, FRESH_MS, layoutGraph, SPACING } from "./graph";
import type { Run, Task } from "./types";

const task = (id: string, state: Task["state"], patch: Partial<Task> = {}): Task => ({
  id,
  room_id: "room_1",
  title: `任务 ${id}`,
  scope: "",
  acceptance: "",
  owner_bot_id: "bot_1",
  state,
  note: "",
  created_by: { kind: "user" },
  workdir: null,
  branch: null,
  artifacts: [],
  rev: 1,
  created_at: 1,
  updated_at: 1000,
  ...patch,
});
const run = (id: string, taskId: string, attempt: number, state: Run["state"], started = 500): Run => ({
  id,
  task_id: taskId,
  room_id: "room_1",
  bot_id: "bot_1",
  message_id: `msg_${id}`,
  state,
  attempt,
  started_at: started,
  ended_at: state === "running" ? null : started + 10,
  report: null,
  error: state === "failed" ? "模型报错" : null,
});
const index = (runs: Run[]) => Object.fromEntries(runs.map((r) => [r.id, r]));

describe("buildGraph", () => {
  it("draws one lane per task, newest first: its runs in order, then where it stands", () => {
    const runs = index([run("b", "t1", 2, "succeeded"), run("a", "t1", 1, "failed"), run("c", "t2", 1, "running")]);
    const lanes = buildGraph([task("t1", "review"), task("t2", "working"), task("t3", "open")], runs);
    expect(lanes.map((lane) => lane.task.id)).toEqual(["t3", "t2", "t1"]);
    expect(lanes[2]?.nodes.map((n) => [n.label, n.tone])).toEqual([
      ["第 1 次", "danger"],
      ["第 2 次", "ok"],
      ["等你验收", "attention"],
    ]);
    expect(lanes[2]?.nodes[0]?.detail).toBe("第 1 次运行：没做成（模型报错）");
    // 正在做的那次运行已经画出来了，不再多画一个「正在做」。
    expect(lanes[1]?.nodes.map((n) => n.id)).toEqual(["run:c"]);
    expect(lanes[0]?.nodes.map((n) => n.id)).toEqual(["state:t3:open"]);
  });

  it("still shows a working task whose run has not been seen, and can focus on one task", () => {
    const lanes = buildGraph([task("t1", "working"), task("t2", "done")], {}, "t1");
    expect(lanes).toHaveLength(1);
    expect(lanes[0]?.nodes.map((n) => [n.label, n.tone])).toEqual([["正在做", "live"]]);
  });
});

describe("layoutGraph", () => {
  const lanes = buildGraph(
    [task("t1", "review"), task("t2", "open")],
    index([run("a", "t1", 1, "failed"), run("b", "t1", 2, "succeeded")]),
  );

  it("puts the task name in the first column and nodes to its right", () => {
    const { column, row } = SPACING.normal;
    const layout = layoutGraph(lanes, "normal");
    expect(layout.labels.map((label) => label.task.id)).toEqual(["t2", "t1"]);
    expect(layout.height).toBe(2 * row);
    expect(layout.width).toBe(4 * column);
    const second = layout.nodes.filter((n) => n.taskId === "t1");
    expect(second.map((n) => n.x)).toEqual([column, 2 * column, 3 * column]);
    expect(new Set(second.map((n) => n.y)).size).toBe(1);
    expect(second[0]?.y).toBeGreaterThan(row);
    // 三个节点之间两条线，线正好接上后一个节点的左边。
    expect(layout.edges.map((edge) => edge.x2)).toEqual([2 * column, 3 * column]);
  });

  it("packs tighter in overview", () => {
    const normal = layoutGraph(lanes, "normal");
    const overview = layoutGraph(lanes, "overview");
    expect(overview.width).toBeLessThan(normal.width);
    expect(overview.height).toBe(2 * SPACING.overview.row);
    expect(layoutGraph([], "normal")).toMatchObject({ height: 0, nodes: [], edges: [] });
  });
});

describe("freshNodes", () => {
  it("only lights up nodes that are new and just happened", () => {
    const now = 100_000;
    const old = run("a", "t1", 1, "failed", now - FRESH_MS - 1);
    const live = run("b", "t1", 2, "running", now - 1000);
    const lanes = buildGraph([task("t1", "working", { updated_at: now - 1000 })], index([old, live]));
    // 第一次看到：旧的那次是翻出来的历史，不飞。
    expect(freshNodes(new Set(), lanes, now).map((n) => n.id)).toEqual(["run:b"]);
    // 已经见过的不再飞。
    expect(freshNodes(new Set(["run:b"]), lanes, now)).toEqual([]);
  });
});
