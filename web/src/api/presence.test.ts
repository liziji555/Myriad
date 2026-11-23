import { describe, expect, it } from "vitest";
import type { RoomChat } from "./chat";
import { speakersOf, presenceOf, presenceSummary } from "./presence";
import type { Bot, Message, Task, ToolStep } from "./types";

const ROOM = "room_1";
const bot = (n: number, patch: Partial<Bot> = {}): Bot =>
  ({ id: `bot_${n}`, name: `Bot ${n}`, persona: "", enabled: true, created_at: n, ...patch }) as Bot;
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
  created_at: seq,
  updated_at: seq,
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
const chat = (messages: Message[], queued: string[] = []): RoomChat => ({
  messages,
  hasMore: false,
  loaded: true,
  queue: [],
  activity: { room_id: ROOM, speaking: null, speakers: [], queued, routing: false, handled_seq: 0 },
});

describe("presenceOf", () => {
  it("没有事的 Bot 是空闲，停用的写明停用", () => {
    const rows = presenceOf([bot(1), bot(2, { enabled: false })], chat([]), []);
    expect(rows.map((row) => [row.name, row.state, row.text])).toEqual([
      ["Bot 1", "idle", "空闲"],
      ["Bot 2", "off", "已停用"],
    ]);
    expect(presenceSummary(rows)).toBe("都空着");
  });

  it("正在写的回复：没有步骤是在写，有在跑的步骤说那一步，有等批准的说等批准", () => {
    const writing = presenceOf([bot(1)], chat([message(1)]), []);
    expect(writing[0]).toMatchObject({ state: "writing", text: "正在写回复", messageId: "msg_1" });

    const tool = presenceOf(
      [bot(1)],
      chat([message(1, { steps: [step("ok", "读取 a.md"), step("running", "读取 b.md")] })]),
      [],
    );
    expect(tool[0]).toMatchObject({ state: "tool", text: "读取 b.md" });

    const approval = presenceOf(
      [bot(1)],
      chat([message(1, { steps: [step("running", "读取 b.md"), step("waiting", "运行 git status")] })]),
      [],
    );
    expect(approval[0]).toMatchObject({ state: "approval", text: "等你批准 运行 git status", messageId: "msg_1" });
    expect(presenceSummary(approval)).toBe("1 件事等你");
  });

  it("写完的、别人的、人发的消息不算", () => {
    const rows = presenceOf(
      [bot(1)],
      chat([
        message(1, { state: "complete" }),
        message(2, { sender: { kind: "bot", bot_id: "bot_2" } }),
        message(3, { sender: { kind: "user" }, state: "complete" }),
      ]),
      [],
    );
    expect(rows[0].state).toBe("idle");
  });

  it("任务的运行过程带上任务名，并指到那条消息和那个任务", () => {
    const rows = presenceOf(
      [bot(1)],
      chat([message(5, { kind: "run", task_id: "task_1", steps: [step("running", "运行 cargo test")] })]),
      [task(1)],
    );
    expect(rows[0]).toMatchObject({
      state: "tool",
      text: "任务「任务 1」：运行 cargo test",
      messageId: "msg_5",
      taskId: "task_1",
    });
  });

  it("任务等人验收、回答、卡住时说出来；在做但手上没有那条消息时也知道在做", () => {
    const review = presenceOf([bot(1)], chat([]), [task(1, { state: "review" })]);
    expect(review[0]).toMatchObject({ state: "asking", text: "任务「任务 1」等你验收", taskId: "task_1" });
    const working = presenceOf([bot(1)], undefined, [task(1)]);
    expect(working[0]).toMatchObject({ state: "working", text: "正在做任务「任务 1」" });
    const others = presenceOf([bot(1)], chat([]), [task(1, { state: "done" }), task(2, { owner_bot_id: "bot_2" })]);
    expect(others[0].state).toBe("idle");
  });

  it("等批准比等验收靠前；同时在对话和任务里时挑最需要人看的", () => {
    const rows = presenceOf(
      [bot(1)],
      chat([
        message(1),
        message(2, { kind: "run", task_id: "task_2", steps: [step("waiting", "运行 rm -rf x")] }),
      ]),
      [task(1, { state: "review" }), task(2)],
    );
    expect(rows[0]).toMatchObject({ state: "approval", messageId: "msg_2", taskId: "task_2" });
  });

  it("需要人处理的排在前面，其余保持进房间的先后；排队的写明在排队", () => {
    const rows = presenceOf(
      [bot(1), bot(2), bot(3), bot(4)],
      chat([message(1, { sender: { kind: "bot", bot_id: "bot_3" }, steps: [step("waiting", "写入 a.md")] })], ["bot_2"]),
      [task(1, { owner_bot_id: "bot_4", state: "blocked" })],
    );
    expect(rows.map((row) => [row.botId, row.state])).toEqual([
      ["bot_3", "approval"],
      ["bot_4", "asking"],
      ["bot_2", "queued"],
      ["bot_1", "idle"],
    ]);
    expect(presenceSummary(rows)).toBe("2 件事等你");
    expect(presenceSummary(rows.slice(2))).toBe("1 个 Bot 在忙");
  });
});

describe("speakersOf", () => {
  const base = { room_id: ROOM, queued: [], routing: false, handled_seq: 0 };
  it("看 speakers；旧版服务只有 speaking 时退回它", () => {
    expect(speakersOf({ ...base, speaking: "bot_1", speakers: ["bot_1", "bot_2"] })).toEqual(["bot_1", "bot_2"]);
    expect(speakersOf({ ...base, speaking: "bot_1" } as never)).toEqual(["bot_1"]);
    expect(speakersOf(undefined)).toEqual([]);
  });
});
