import { describe, expect, it } from "vitest";
import type { Bot, Message } from "./types";
import { flowOf } from "./flow";

function message(
  patch: Partial<Message> & Pick<Message, "id" | "seq">,
): Message {
  return {
    room_id: "r",
    sender: { kind: "user" },
    content: "话",
    mentions: [],
    hop: 0,
    kind: "chat",
    steps: [],
    state: "complete",
    error: null,
    rev: 1,
    created_at: patch.seq * 1000,
    updated_at: patch.seq * 1000,
    ...patch,
  } as Message;
}

const bot = (id: string) => ({ kind: "bot", bot_id: id }) as Message["sender"];
const bots = ["main", "a", "b"].map(
  (id) => ({ id, name: id.toUpperCase() }) as Bot,
);
const step = (id: string, tool: string, state = "ok") =>
  ({ id, tool, title: tool, state }) as Message["steps"][number];

describe("flowOf", () => {
  it("你、主 Agent、成员各一条竖线，主 Agent 排在你旁边", () => {
    const flow = flowOf([], bots, "main", ["a", "main", "b"]);
    expect(flow.lanes.map((lane) => [lane.key, lane.role])).toEqual([
      ["user", "user"],
      ["bot:main", "main"],
      ["bot:a", "bot"],
      ["bot:b", "bot"],
    ]);
    expect(flow.nodes).toEqual([]);
  });

  it("主 Agent 派给两个 Bot，各自做完交回，再答复你", () => {
    const flow = flowOf(
      [
        message({ id: "u1", seq: 1, content: "查一下" }),
        message({
          id: "m1",
          seq: 2,
          sender: bot("main"),
          steps: [step("s1", "ask_bot"), step("s2", "ask_bot")],
          content: "分给 A 和 B",
        }),
        message({
          id: "h1",
          seq: 3,
          sender: bot("main"),
          mentions: ["a"],
          hop: 1,
        }),
        message({
          id: "h2",
          seq: 4,
          sender: bot("main"),
          mentions: ["b"],
          hop: 1,
        }),
        message({
          id: "a1",
          seq: 5,
          sender: bot("a"),
          hop: 1,
          steps: [step("r", "read_file")],
        }),
        message({
          id: "b1",
          seq: 6,
          sender: bot("b"),
          hop: 1,
          state: "streaming",
        }),
      ],
      bots,
      "main",
      ["main", "a", "b"],
    );
    const kinds = flow.edges.map((edge) => [
      edge.from,
      edge.to,
      edge.kind,
      edge.live,
    ]);
    expect(kinds).toEqual([
      ["u1", "m1:start", "ask", false],
      ["m1:end", "m1:back:user", "return", false],
      ["h1", "a1:start", "dispatch", false],
      ["a1:end", "a1:back:bot:main", "return", false],
      ["h2", "b1:start", "dispatch", true],
    ]);
    const row = (id: string) => flow.nodes.find((node) => node.id === id)!.row;
    // 交接画在主 Agent 那一步的位置；接手的节点总比交接低。
    expect(
      flow.nodes
        .filter((node) => node.lane === "bot:main")
        .map((node) => node.kind),
    ).toEqual(["start", "handoff", "handoff", "done", "inbox"]);
    expect(row("a1:start")).toBeGreaterThan(row("h1"));
    expect(row("b1:start")).toBeGreaterThan(row("h2"));
    expect(row("a1:back:bot:main")).toBeGreaterThan(row("a1:end"));
    expect(flow.nodes.find((node) => node.id === "b1:end")?.tone).toBe("live");
  });

  it("成员之间交接算 peer，你再说一句另起一轮", () => {
    const flow = flowOf(
      [
        message({ id: "u1", seq: 1, mentions: ["a"] }),
        message({
          id: "a1",
          seq: 2,
          sender: bot("a"),
          steps: [step("s", "ask_bot")],
        }),
        message({ id: "h", seq: 3, sender: bot("a"), mentions: ["b"], hop: 1 }),
        message({ id: "b1", seq: 4, sender: bot("b"), hop: 1 }),
        message({ id: "u2", seq: 5 }),
      ],
      bots,
      "main",
      ["main", "a", "b"],
    );
    expect(flow.edges.find((edge) => edge.to === "b1:start")?.kind).toBe(
      "peer",
    );
    expect(flow.edges.find((edge) => edge.from === "b1:end")?.to).toBe(
      "b1:back:bot:a",
    );
    const u2 = flow.nodes.find((node) => node.id === "u2")!;
    expect(flow.rounds).toEqual([0, u2.row]);
    expect(u2.row).toBe(
      Math.max(
        ...flow.nodes
          .filter((node) => node.id !== "u2")
          .map((node) => node.row),
      ) + 1,
    );
  });

  it("开的任务挂在那一步上，运行和交付接在任务下面，交付交回开任务的人", () => {
    const flow = flowOf(
      [
        message({ id: "u1", seq: 1 }),
        message({
          id: "m1",
          seq: 2,
          sender: bot("main"),
          steps: [step("t", "create_task")],
        }),
        message({
          id: "card",
          seq: 3,
          sender: bot("main"),
          kind: "task",
          task_id: "T",
          content: "开了任务「改 bug」",
        }),
        message({
          id: "run",
          seq: 4,
          sender: bot("a"),
          kind: "run",
          task_id: "T",
          state: "streaming",
        }),
        message({
          id: "dl",
          seq: 5,
          sender: bot("a"),
          kind: "delivery",
          task_id: "T",
          content: "改好了",
        }),
      ],
      bots,
      "main",
      ["main", "a"],
    );
    expect(flow.nodes.find((node) => node.id === "card")?.lane).toBe(
      "bot:main",
    );
    expect(flow.edges.map((edge) => [edge.from, edge.to, edge.kind])).toEqual(
      expect.arrayContaining([
        ["card", "run:start", "dispatch"],
        ["dl", "dl:inbox", "return"],
      ]),
    );
    expect(flow.nodes.find((node) => node.id === "dl:inbox")?.lane).toBe(
      "bot:main",
    );
  });
  it("成员找主 Agent 接手算交接，不算交回结果", () => {
    const flow = flowOf(
      [
        message({ id: "u1", seq: 1, content: "@A 查一下", mentions: ["a"] }),
        message({
          id: "a1",
          seq: 2,
          sender: bot("a"),
          steps: [step("s1", "ask_bot")],
        }),
        message({
          id: "h1",
          seq: 3,
          sender: bot("a"),
          mentions: ["main"],
          hop: 1,
        }),
        message({ id: "m1", seq: 4, sender: bot("main"), hop: 1 }),
      ],
      bots,
      "main",
      ["main", "a"],
    );
    const into = flow.edges.find((edge) => edge.to === "m1:start");
    expect(into?.kind).toBe("peer");
    expect(
      flow.edges
        .filter((edge) => edge.kind === "return")
        .map((edge) => edge.to),
    ).toEqual(["a1:back:user", "m1:back:bot:a"]);
  });

  it("定时任务叫起来的回复不连到你上一句话", () => {
    const flow = flowOf(
      [
        message({ id: "u1", seq: 1, content: "早" }),
        message({ id: "m1", seq: 2, sender: bot("main") }),
        message({ id: "r1", seq: 3, kind: "routine", content: "日报" }),
        message({ id: "m2", seq: 4, sender: bot("main") }),
      ],
      bots,
      "main",
      ["main"],
    );
    expect(flow.edges.some((edge) => edge.to === "m2:start")).toBe(false);
    expect(flow.edges.some((edge) => edge.from === "m2:end")).toBe(false);
    expect(flow.nodes.find((node) => node.id === "m2:start")?.label).toBe(
      "接到定时任务",
    );
  });

  it("人打回任务不算新开任务卡，重做和交付还接在 Bot 开的卡上", () => {
    const flow = flowOf(
      [
        message({ id: "u1", seq: 1, content: "做个功能" }),
        message({ id: "m1", seq: 2, sender: bot("main") }),
        message({
          id: "card",
          seq: 3,
          sender: bot("main"),
          kind: "task",
          task_id: "t1",
          content: "开了任务「功能」：做",
        }),
        message({
          id: "no",
          seq: 4,
          kind: "task",
          task_id: "t1",
          content: "打回了任务「功能」：不行",
        }),
        message({
          id: "run2",
          seq: 5,
          sender: bot("a"),
          kind: "run",
          task_id: "t1",
        }),
        message({
          id: "dl",
          seq: 6,
          sender: bot("a"),
          kind: "delivery",
          task_id: "t1",
        }),
      ],
      bots,
      "main",
      ["main", "a"],
    );
    expect(flow.nodes.some((node) => node.id === "no")).toBe(false);
    expect(flow.edges.find((edge) => edge.to === "run2:start")?.from).toBe(
      "card",
    );
    expect(flow.nodes.find((node) => node.id === "dl:inbox")?.lane).toBe(
      "bot:main",
    );
  });
});
