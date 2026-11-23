import { describe, expect, it } from "vitest";
import type { Message } from "./types";
import { handoffsOf } from "./handoffs";

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

describe("handoffsOf", () => {
  it("交接那句话连到对方之后的第一条回复", () => {
    const links = handoffsOf([
      message({ id: "u1", seq: 1, mentions: ["a"] }),
      message({ id: "a1", seq: 2, sender: bot("a") }),
      message({ id: "ask", seq: 3, sender: bot("a"), mentions: ["b"], hop: 1 }),
      message({ id: "x", seq: 4, sender: bot("c") }),
      message({ id: "b1", seq: 5, sender: bot("b"), state: "streaming", hop: 1 }),
      message({ id: "b2", seq: 6, sender: bot("b"), hop: 1 }),
    ]);
    expect(links).toEqual([
      { id: "ask>b1", from: "ask", to: "b1", fromBot: "a", toBot: "b", live: true, at: 3000 },
    ]);
  });

  it("人点名的不算交接，对方还没开口的先不画", () => {
    expect(
      handoffsOf([
        message({ id: "u1", seq: 1, mentions: ["a", "b"] }),
        message({ id: "a1", seq: 2, sender: bot("a") }),
        message({ id: "ask", seq: 3, sender: bot("a"), mentions: ["c"] }),
      ]),
    ).toEqual([]);
  });

  it("写完了就不在流动，任务的运行过程不算对方的回复", () => {
    const links = handoffsOf([
      message({ id: "ask", seq: 1, sender: bot("a"), mentions: ["b", "a"], hop: 1 }),
      message({ id: "run", seq: 2, sender: bot("b"), kind: "run", state: "streaming", hop: 1 }),
      message({ id: "b1", seq: 3, sender: bot("b"), hop: 1 }),
    ]);
    expect(links.map(({ to, live }) => [to, live])).toEqual([["b1", false]]);
  });

  it("对方没接这次交接，之后别人点名它的回复不连到这句", () => {
    expect(
      handoffsOf([
        message({ id: "ask", seq: 1, sender: bot("a"), mentions: ["b"], hop: 1 }),
        message({ id: "u2", seq: 2, mentions: ["b"] }),
        message({ id: "b1", seq: 3, sender: bot("b") }),
        message({ id: "ask2", seq: 4, sender: bot("c"), mentions: ["b"], hop: 1 }),
        message({ id: "b2", seq: 5, sender: bot("b"), hop: 1 }),
      ]).map(({ id }) => id),
    ).toEqual(["ask2>b2"]);
  });
});
