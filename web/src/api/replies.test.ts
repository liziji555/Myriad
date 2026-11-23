import { describe, expect, it } from "vitest";
import type { Message, RoomActivity } from "./types";
import { repliesOf } from "./replies";

function message(patch: Partial<Message> & Pick<Message, "id" | "seq">): Message {
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
const idle: RoomActivity = { room_id: "r", speaking: null, speakers: [], queued: [], routing: false, handled_seq: 99 };

describe("repliesOf", () => {
  it("Bot 还在写就是回复中，写完是已回复", () => {
    const said = message({ id: "u1", seq: 1 });
    expect(repliesOf([said, message({ id: "m1", seq: 2, sender: bot("b1"), state: "streaming" })], idle).get("u1")).toBe(
      "replying",
    );
    expect(repliesOf([said, message({ id: "m1", seq: 2, sender: bot("b1") })], idle).get("u1")).toBe("replied");
  });

  it("刚发出去、房间还在挑人或排队时也是回复中", () => {
    const said = message({ id: "u1", seq: 5 });
    expect(repliesOf([said], { ...idle, routing: true, handled_seq: 4 }).get("u1")).toBe("replying");
    expect(repliesOf([said], { ...idle, queued: ["b1"], handled_seq: 4 }).get("u1")).toBe("replying");
    expect(repliesOf([said], idle).has("u1")).toBe(false);
  });

  it("几个 Bot 接同一句：有一个出错就是失败，全被停下是已停止", () => {
    const said = message({ id: "u1", seq: 1 });
    const ok = message({ id: "m1", seq: 2, sender: bot("b1") });
    const bad = message({ id: "m2", seq: 3, sender: bot("b2"), state: "failed" });
    expect(repliesOf([said, ok, bad], idle).get("u1")).toBe("failed");
    const cut = message({ id: "m3", seq: 2, sender: bot("b1"), state: "cancelled" });
    expect(repliesOf([said, cut], idle).get("u1")).toBe("stopped");
    expect(repliesOf([said, cut, message({ id: "m4", seq: 3, sender: bot("b2") })], idle).get("u1")).toBe("replied");
  });

  it("下一句人说的话之后才开始的回复不算给上一句", () => {
    const replies = repliesOf(
      [
        message({ id: "u1", seq: 1 }),
        message({ id: "m1", seq: 2, sender: bot("b1") }),
        message({ id: "u2", seq: 3 }),
        message({ id: "m2", seq: 4, sender: bot("b1"), state: "failed" }),
      ],
      idle,
    );
    expect(replies.get("u1")).toBe("replied");
    expect(replies.get("u2")).toBe("failed");
  });

  it("插话算给它插进去的那条回复", () => {
    const messages = [
      message({ id: "m0", seq: 1, sender: bot("b1") }),
      message({ id: "u1", seq: 2 }),
      message({ id: "m1", seq: 3, sender: bot("b1"), state: "streaming" }),
      message({ id: "u2", seq: 4 }),
    ];
    const steered = (id: string) => (id === "u2" ? ["b1"] : []);
    expect(repliesOf(messages, idle, steered).get("u2")).toBe("replying");
    expect(repliesOf(messages, idle).has("u2")).toBe(false);
  });

  it("不算 Bot 自己的话、任务运行和外面来的事件", () => {
    const replies = repliesOf(
      [
        message({ id: "e1", seq: 1, kind: "event" }),
        message({ id: "u1", seq: 2 }),
        message({ id: "r1", seq: 3, sender: bot("b1"), kind: "run", state: "streaming" }),
      ],
      idle,
    );
    expect(replies.size).toBe(0);
  });
});
