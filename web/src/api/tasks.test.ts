import { describe, expect, it } from "vitest";
import {
  emptyBoard,
  groupTasks,
  isLatestDelivery,
  mergeTasks,
  resetBoard,
  runsOf,
  upsertRun,
  upsertTask,
  watchTasks,
} from "./tasks";
import type { Message, Run, Task } from "./types";

const ROOM = "room_1";
const task = (n: number, patch: Partial<Task> = {}): Task => ({
  id: `task_${n}`,
  room_id: ROOM,
  title: `任务 ${n}`,
  scope: "做点什么",
  acceptance: "",
  owner_bot_id: "bot_1",
  state: "open",
  note: "",
  created_by: { kind: "user" },
  workdir: null,
  branch: null,
  artifacts: [],
  rev: 0,
  created_at: n,
  updated_at: n,
  ...patch,
});
const run = (n: number, patch: Partial<Run> = {}): Run => ({
  id: `run_${n}`,
  task_id: "task_1",
  room_id: ROOM,
  bot_id: "bot_1",
  message_id: `msg_${n}`,
  state: "running",
  attempt: n,
  started_at: n,
  ended_at: null,
  report: null,
  error: null,
  ...patch,
});
const watching = watchTasks(emptyBoard, ROOM);

describe("task board", () => {
  it("ignores rooms nobody opened", () => {
    expect(upsertTask(emptyBoard, task(1))).toBe(emptyBoard);
    expect(mergeTasks(emptyBoard, ROOM, [task(1)])).toBe(emptyBoard);
  });

  it("keeps tasks in creation order and only accepts newer revisions", () => {
    let board = upsertTask(watching, task(2));
    board = upsertTask(board, task(1));
    expect(board.rooms[ROOM].map((t) => t.id)).toEqual(["task_1", "task_2"]);

    board = upsertTask(board, task(1, { state: "review", rev: 3 }));
    expect(upsertTask(board, task(1, { state: "working", rev: 2 }))).toBe(board);
    expect(board.rooms[ROOM][0].state).toBe("review");
  });

  it("merging a fetched list keeps newer versions that arrived meanwhile", () => {
    let board = upsertTask(watching, task(1, { state: "review", rev: 5 }));
    board = upsertTask(board, task(3));
    board = mergeTasks(board, ROOM, [task(1, { rev: 2 }), task(2)]);
    expect(board.rooms[ROOM].map((t) => [t.id, t.state])).toEqual([
      ["task_1", "review"],
      ["task_2", "open"],
      ["task_3", "open"],
    ]);
  });

  it("a late running update never reopens a finished run", () => {
    let board = upsertRun(watching, run(1));
    board = upsertRun(board, run(1, { state: "succeeded", ended_at: 9 }));
    expect(upsertRun(board, run(1))).toBe(board);
    board = upsertRun(board, run(2));
    board = upsertRun(board, run(7, { task_id: "task_other" }));
    expect(runsOf(board, "task_1").map((r) => [r.attempt, r.state])).toEqual([
      [1, "succeeded"],
      [2, "running"],
    ]);
  });

  it("reset keeps watching the open room only", () => {
    expect(resetBoard(ROOM).rooms).toEqual({ [ROOM]: [] });
    expect(resetBoard(null)).toBe(emptyBoard);
  });
});

describe("groupTasks", () => {
  it("puts what needs the person first, finished last, newest first inside a group", () => {
    const groups = groupTasks([
      task(1, { state: "done" }),
      task(2, { state: "working" }),
      task(3, { state: "review" }),
      task(4, { state: "review" }),
      task(5, { state: "blocked" }),
    ]);
    expect(groups.map((g) => [g.state, g.tasks.map((t) => t.id)])).toEqual([
      ["review", ["task_4", "task_3"]],
      ["blocked", ["task_5"]],
      ["working", ["task_2"]],
      ["done", ["task_1"]],
    ]);
  });
});

describe("isLatestDelivery", () => {
  it("is true only for the newest delivery card of the same task", () => {
    const card = (seq: number, taskId: string): Message => ({
      id: `msg_${seq}`,
      seq,
      room_id: ROOM,
      sender: { kind: "bot", bot_id: "bot_1" },
      content: "交了",
      mentions: [],
      hop: 0,
      kind: "delivery",
      task_id: taskId,
      steps: [],
      state: "complete",
      error: null,
      rev: 0,
      created_at: seq,
      updated_at: seq,
    });
    const messages = [card(1, "task_1"), card(2, "task_2"), card(3, "task_1")];
    expect(isLatestDelivery(messages, messages[0])).toBe(false);
    expect(isLatestDelivery(messages, messages[1])).toBe(true);
    expect(isLatestDelivery(messages, messages[2])).toBe(true);
  });
});
