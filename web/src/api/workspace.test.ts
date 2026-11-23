import { describe, expect, it } from "vitest";
import type { Bot, Event, Link, Notice, Room, Routine } from "./types";
import { applyEvent, emptyWorkspace, noticesFor } from "./workspace";

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
  discussion: "normal",
  rounds: 2,
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

describe("notices", () => {
  const notice = (id: string, created_at: number, room_id: string | null = null): Notice => ({
    id,
    kind: "budget",
    level: "warning",
    room_id,
    task_id: null,
    text: "今天的花费到上限了",
    created_at,
    dismissed_at: null,
  });

  it("keeps the newest first and does not duplicate a replayed notice", () => {
    let state = applyEvent(emptyWorkspace, event(1, "notice.created", notice("ntc_1", 10)));
    state = applyEvent(state, event(2, "notice.created", notice("ntc_2", 20)));
    state = applyEvent(state, event(1, "notice.created", notice("ntc_1", 10)));
    expect(state.notices.map((n) => n.id)).toEqual(["ntc_2", "ntc_1"]);
  });

  it("drops a notice once it is dismissed, even if it was never seen", () => {
    const state = applyEvent(emptyWorkspace, event(1, "notice.created", notice("ntc_1", 10)));
    const dismissed = { ...notice("ntc_1", 10), dismissed_at: 30 };
    expect(applyEvent(state, event(2, "notice.updated", dismissed)).notices).toEqual([]);
    expect(applyEvent(emptyWorkspace, event(2, "notice.updated", dismissed)).notices).toEqual([]);
  });

  it("shows a room its own notices and the ones that belong to no room", () => {
    const all = [notice("ntc_1", 10), notice("ntc_2", 20, "room_1"), notice("ntc_3", 30, "room_2")];
    expect(noticesFor(all, "room_1").map((n) => n.id)).toEqual(["ntc_1", "ntc_2"]);
    expect(noticesFor(all, null).map((n) => n.id)).toEqual(["ntc_1"]);
  });

  it("keeps one link per repo or chat and drops deleted ones", () => {
    const link = (id: string, source: Link["source"], external: string, room_id: string): Link => ({
      id,
      source,
      external,
      room_id,
      title: external,
      created_at: 1,
    });
    const repo = link("lnk_1", "github", "me/app", "room_1");
    const chat = link("lnk_2", "telegram", "42", "room_1");
    let state = [repo, chat].reduce(
      (current, item, index) => applyEvent(current, event(index + 1, "link.created", item)),
      emptyWorkspace,
    );
    expect(state.links.map((l) => l.id)).toEqual(["lnk_1", "lnk_2"]);
    // 重放不重复。
    expect(applyEvent(state, event(1, "link.created", repo)).links).toEqual(state.links);

    // 同一个聊天改连到别的房间：原来那条换掉。
    const moved = link("lnk_3", "telegram", "42", "room_2");
    state = applyEvent(state, event(3, "link.created", moved));
    expect(state.links.map((l) => l.id)).toEqual(["lnk_1", "lnk_3"]);

    state = applyEvent(state, event(4, "link.deleted", { id: "lnk_1", room_id: "room_1" }));
    expect(state.links).toEqual([moved]);
  });

  it("keeps routines in the order they were made and drops them with their Bot", () => {
    const routine = (id: string, title: string): Routine => ({
      id,
      room_id: "room_1",
      bot_id: bot.id,
      title,
      prompt: "做事",
      schedule: "0 9 * * *",
      enabled: true,
      next_at: 100,
      last_at: null,
      last_result: null,
      last_note: "",
      failures: 0,
      created_at: 1,
      updated_at: 1,
    });
    let state = applyEvent(emptyWorkspace, event(1, "routine.created", routine("rtn_1", "早报")));
    state = applyEvent(state, event(2, "routine.created", routine("rtn_2", "晚报")));
    state = applyEvent(state, event(3, "routine.updated", routine("rtn_1", "晨报")));
    expect(state.routines.map((r) => r.title)).toEqual(["晨报", "晚报"]);

    const gone = applyEvent(state, event(4, "routine.deleted", { id: "rtn_1", room_id: "room_1" }));
    expect(gone.routines.map((r) => r.id)).toEqual(["rtn_2"]);
    expect(applyEvent(state, event(5, "bot.deleted", { id: bot.id })).routines).toEqual([]);
  });
});
