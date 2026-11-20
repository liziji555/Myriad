import { describe, expect, it } from "vitest";
import type { Bot, Event, Room } from "./types";
import { applyEvent, emptyWorkspace } from "./workspace";

const bot: Bot = {
  id: "bot_1",
  name: "助手",
  kind: "assistant",
  persona: "",
  tier: "mid",
  enabled: true,
  created_at: 1,
};
const room = (id: string, name: string): Room => ({
  id,
  name,
  host_bot_id: bot.id,
  member_bot_ids: [bot.id],
  created_at: 1,
  updated_at: 1,
});
const event = (seq: number, kind: Event["kind"], payload: unknown): Event => ({
  seq,
  room_id: null,
  kind,
  payload,
  created_at: 1,
});

describe("applyEvent", () => {
  it("puts a new room first", () => {
    const first = applyEvent(emptyWorkspace, event(1, "room.created", room("room_1", "开发")));
    const second = applyEvent(first, event(2, "room.created", room("room_2", "助理")));
    expect(second.rooms.map((r) => r.id)).toEqual(["room_2", "room_1"]);
  });

  it("is idempotent, so a replayed event does not duplicate", () => {
    const created = event(1, "room.created", room("room_1", "开发"));
    const once = applyEvent(emptyWorkspace, created);
    expect(applyEvent(once, created)).toEqual(once);

    const botCreated = event(2, "bot.created", bot);
    const withBot = applyEvent(once, botCreated);
    expect(applyEvent(withBot, botCreated).bots).toEqual([bot]);
  });

  it("updates rooms and bots in place and removes deleted bots", () => {
    let state = applyEvent(emptyWorkspace, event(1, "room.created", room("room_1", "开发")));
    state = applyEvent(state, event(2, "room.created", room("room_2", "助理")));
    state = applyEvent(state, event(3, "bot.created", bot));
    const coder: Bot = { ...bot, id: "bot_2", name: "Coder" };
    state = applyEvent(state, event(4, "bot.created", coder));

    const renamed = { ...room("room_1", "新名字"), member_bot_ids: [bot.id, coder.id] };
    state = applyEvent(state, event(5, "room.updated", renamed));
    // 改名不改变房间在列表里的位置。
    expect(state.rooms.map((r) => r.name)).toEqual(["助理", "新名字"]);
    expect(state.rooms[1].member_bot_ids).toEqual([bot.id, coder.id]);

    state = applyEvent(state, event(6, "bot.updated", { ...bot, enabled: false }));
    expect(state.bots.map((b) => [b.name, b.enabled])).toEqual([
      ["助手", false],
      ["Coder", true],
    ]);
    state = applyEvent(state, event(7, "bot.deleted", { id: coder.id }));
    expect(state.bots.map((b) => b.id)).toEqual([bot.id]);
  });

  it("does not mutate the previous state", () => {
    const before = applyEvent(emptyWorkspace, event(1, "room.created", room("room_1", "开发")));
    const snapshot = structuredClone(before);
    applyEvent(before, event(2, "room.created", room("room_2", "助理")));
    expect(before).toEqual(snapshot);
  });
});
