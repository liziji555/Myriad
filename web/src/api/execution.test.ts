import { describe, expect, it } from "vitest";
import type { RoomChat } from "./chat";
import { ago, elapsed, executionOf } from "./execution";
import { presenceOf } from "./presence";
import type { Bot, Message, Task, ToolStep } from "./types";

const ROOM = "room_1";
const bot = (n: number, patch: Partial<Bot> = {}): Bot =>
  ({ id: `bot_${n}`, name: `Bot ${n}`, tier: "mid", persona: "", enabled: true, created_at: n, ...patch }) as Bot;
const message = (seq: number, patch: Partial<Message> = {}): Message => ({
  id: `msg_${seq}`,
  seq,
  room_id: ROOM,
  sender: { kind: "bot", bot_id: "bot_1" },
  content: "",
  mentions: [],
  hop: 0,
  kind: "chat",
  steps: [],
  state: "streaming",
  error: null,
  rev: 0,
  created_at: seq * 1000,
  updated_at: seq * 1000,
  ...patch,
});
const step = (state: ToolStep["state"], title: string): ToolStep => ({ id: title, tool: "shell", title, state });
const task = (n: number, patch: Partial<Task> = {}): Task => ({
  id: `task_${n}`,
  room_id: ROOM,
  title: `任务 ${n}`,
  scope: "",
  acceptance: "",
  owner_bot_id: "bot_1",
  state: "working",
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
const chat = (messages: Message[]): RoomChat => ({
  messages,
  hasMore: false,
  loaded: true,
  activity: { room_id: ROOM, speaking: null, queued: [], routing: false, handled_seq: 0 },
});
const cards = (members: Bot[], room: RoomChat, tasks: Task[] = []) =>
  executionOf(presenceOf(members, room, tasks), members, room, tasks);

describe("executionOf", () => {
  it("在写的回复：从什么时候开始、做了几步、几步没成", () => {
    const room = chat([
      message(1, {
        steps: [step("ok", "读 a"), step("error", "执行 exit 3"), step("denied", "写 b"), step("running", "执行 ls")],
      }),
    ]);
    const [card] = cards([bot(1, { tier: "strong" })], room);
    expect(card).toMatchObject({
      state: "tool",
      text: "执行 ls",
      messageId: "msg_1",
      tier: "strong",
      since: 1000,
      done: 3,
      failed: 2,
    });
    expect(card.steps).toHaveLength(4);
    expect(card.lastAt).toBeUndefined();
  });

  it("空闲的 Bot 没有步骤，记着上一次说完话的时间", () => {
    const room = chat([message(1, { state: "complete", updated_at: 5000 }), message(2, { sender: { kind: "user" } })]);
    const [card] = cards([bot(1)], room);
    expect(card).toMatchObject({ state: "idle", steps: [], done: 0, since: undefined, lastAt: 5000 });
  });

  it("任务里的运行带上任务名，等批准的排在前面", () => {
    const room = chat([
      message(1, { sender: { kind: "bot", bot_id: "bot_2" } }),
      message(2, { kind: "run", task_id: "task_1", steps: [step("waiting", "执行 make")] }),
    ]);
    const rows = cards([bot(2), bot(1)], room, [task(1)]);
    expect(rows.map((row) => [row.botId, row.state, row.taskTitle])).toEqual([
      ["bot_1", "approval", "任务 1"],
      ["bot_2", "writing", undefined],
    ]);
    expect(rows[0].messageId).toBe("msg_2");
  });

  it("停用的 Bot 也有一张卡", () => {
    const [card] = cards([bot(1, { enabled: false })], chat([]));
    expect(card).toMatchObject({ state: "off", steps: [] });
  });
});

describe("elapsed / ago", () => {
  it("用了多久", () => {
    expect(elapsed(8_400)).toBe("8 秒");
    expect(elapsed(185_000)).toBe("3 分 05 秒");
    expect(elapsed(3_720_000)).toBe("1 小时 02 分");
    expect(elapsed(-5)).toBe("0 秒");
  });
  it("多久以前", () => {
    expect(ago(30_000)).toBe("刚刚");
    expect(ago(5 * 60_000)).toBe("5 分钟前");
    expect(ago(2 * 3_600_000)).toBe("2 小时前");
    expect(ago(3 * 86_400_000)).toBe("3 天前");
  });
});
