import { describe, expect, it } from "vitest";
import type { Bot, Event, Room } from "./types";
import { applyEvent, emptyWorkspace } from "./workspace";

const bot: Bot = {
  id: "bot_1",
  name: "助手",
  kind: "assistant",
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

  it("does not mutate the previous state", () => {
    const before = applyEvent(emptyWorkspace, event(1, "room.created", room("room_1", "开发")));
    const snapshot = structuredClone(before);
    applyEvent(before, event(2, "room.created", room("room_2", "助理")));
    expect(before).toEqual(snapshot);
  });
});
