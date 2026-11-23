import { describe, expect, it } from "vitest";
import type { Message } from "./types";
import { steersOf } from "./steer";

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

const bot = { kind: "bot", bot_id: "b1" } as Message["sender"];

describe("steersOf", () => {
  it("Bot 正在回复时人说的话是插话", () => {
    const steers = steersOf([
      message({ id: "u1", seq: 1 }),
      message({ id: "m1", seq: 2, sender: bot, state: "streaming" }),
      message({ id: "u2", seq: 3 }),
    ]);
    expect([...steers]).toEqual([["u2", { botIds: ["b1"], live: true }]]);
  });

  it("回复写完之后还记得那句是插话，只是不再是进行中", () => {
    const steers = steersOf([
      message({ id: "m1", seq: 2, sender: bot, updated_at: 9000 }),
      message({ id: "u2", seq: 3 }),
    ]);
    expect(steers.get("u2")).toEqual({ botIds: ["b1"], live: false });
  });

  it("回复已经结束之后说的话是新的一轮", () => {
    const steers = steersOf([message({ id: "m1", seq: 2, sender: bot }), message({ id: "u2", seq: 3 })]);
    expect(steers.size).toBe(0);
  });

  it("点名别的 Bot 不算插话，点名正在回复的那个算", () => {
    const streaming = message({ id: "m1", seq: 2, sender: bot, state: "streaming" });
    expect(steersOf([streaming, message({ id: "u2", seq: 3, mentions: ["b2"] })]).size).toBe(0);
    expect(steersOf([streaming, message({ id: "u2", seq: 3, mentions: ["b1"] })]).size).toBe(1);
  });

  it("几个 Bot 同时在说：没点名的话都进它们的上下文，点名的只给被点到的", () => {
    const b2 = { kind: "bot", bot_id: "b2" } as Message["sender"];
    const both = [
      message({ id: "m1", seq: 2, sender: bot, state: "streaming" }),
      message({ id: "m2", seq: 3, sender: b2, state: "streaming" }),
    ];
    expect(steersOf([...both, message({ id: "u", seq: 4 })]).get("u")).toEqual({ botIds: ["b1", "b2"], live: true });
    expect(steersOf([...both, message({ id: "u", seq: 4, mentions: ["b2"] })]).get("u")).toEqual({
      botIds: ["b2"],
      live: true,
    });
  });

  it("任务的运行过程不算：它不读房间里的话", () => {
    const run = message({ id: "m1", seq: 2, sender: bot, kind: "run", state: "streaming" });
    expect(steersOf([run, message({ id: "u2", seq: 3 })]).size).toBe(0);
  });
});
