import { describe, expect, it } from "vitest";
import {
  applyDelta,
  emptyChat,
  mergePage,
  removeMessage,
  resetChat,
  setActivity,
  upsertMessage,
  watchRoom,
} from "./chat";
import type { Message, MessageDelta } from "./types";

const ROOM = "room_1";
const message = (seq: number, patch: Partial<Message> = {}): Message => ({
  id: `msg_${seq}`,
  seq,
  room_id: ROOM,
  sender: { kind: "user" },
  content: `第 ${seq} 条`,
  mentions: [],
  hop: 0,
  steps: [],
  state: "complete",
  error: null,
  rev: 0,
  created_at: seq,
  updated_at: seq,
  ...patch,
});
const draft = (seq: number, content: string, rev: number): Message =>
  message(seq, { sender: { kind: "bot", bot_id: "bot_1" }, content, rev, state: "streaming" });
const delta = (seq: number, rev: number, text: string): MessageDelta => ({
  message_id: `msg_${seq}`,
  room_id: ROOM,
  rev,
  text,
});
const watching = watchRoom(emptyChat, ROOM);
const contents = (state: typeof emptyChat) => state.rooms[ROOM].messages.map((m) => m.content);

describe("upsertMessage", () => {
  it("ignores rooms nobody opened", () => {
    expect(upsertMessage(emptyChat, message(1))).toBe(emptyChat);
  });

  it("keeps messages ordered by seq and replaces only with a newer rev", () => {
    let state = upsertMessage(watching, message(2));
    state = upsertMessage(state, message(1));
    expect(contents(state)).toEqual(["第 1 条", "第 2 条"]);

    state = upsertMessage(state, message(2, { content: "新版", rev: 3 }));
    const replayed = upsertMessage(state, message(2, { content: "旧版", rev: 1 }));
    expect(replayed).toBe(state);
    expect(contents(state)).toEqual(["第 1 条", "新版"]);
  });
});

describe("applyDelta", () => {
  it("appends in rev order and drops stale or duplicate pieces", () => {
    let state = upsertMessage(watching, draft(1, "", 0));
    state = applyDelta(state, delta(1, 1, "你"));
    state = applyDelta(state, delta(1, 2, "好"));
    state = applyDelta(state, delta(1, 2, "好"));
    state = applyDelta(state, delta(1, 1, "你"));
    expect(contents(state)).toEqual(["你好"]);
    expect(state.rooms[ROOM].messages[0].rev).toBe(2);
    expect(state.pending).toEqual([]);
  });

  it("holds pieces that arrive before their message or out of order", () => {
    let state = applyDelta(watching, delta(1, 2, "好"));
    state = applyDelta(state, delta(1, 1, "你"));
    expect(state.pending).toHaveLength(2);
    state = upsertMessage(state, draft(1, "", 0));
    expect(contents(state)).toEqual(["你好"]);
    expect(state.pending).toEqual([]);
  });

  it("waits for the persisted version when a rev was skipped", () => {
    // rev 3 是一次落库的更新（工具步骤），它的事件比 rev 4 的片段晚到。
    let state = upsertMessage(watching, draft(1, "我先读一下。", 2));
    state = applyDelta(state, delta(1, 4, "\n\n读完了"));
    expect(contents(state)).toEqual(["我先读一下。"]);
    state = upsertMessage(state, draft(1, "我先读一下。", 3));
    expect(contents(state)).toEqual(["我先读一下。\n\n读完了"]);
  });

  it("never touches a finished message", () => {
    const done = message(1, { content: "写完了", rev: 5 });
    let state = upsertMessage(watching, done);
    state = applyDelta(state, delta(1, 6, "多余"));
    expect(contents(state)).toEqual(["写完了"]);
    expect(state.pending).toEqual([]);
  });
});

describe("mergePage", () => {
  it("keeps what the event stream delivered while the page was loading", () => {
    // 拉取期间：第 3 条是新消息，第 2 条有了更新的版本。
    let state = upsertMessage(watching, message(3));
    state = upsertMessage(state, draft(2, "写到这里了", 7));
    state = mergePage(state, ROOM, { messages: [message(1), draft(2, "写到", 4)], has_more: true }, false);
    expect(contents(state)).toEqual(["第 1 条", "写到这里了", "第 3 条"]);
    expect(state.rooms[ROOM]).toMatchObject({ loaded: true, hasMore: true });
  });

  it("drops local messages the latest page no longer covers, then prepends older pages", () => {
    let state = mergePage(watching, ROOM, { messages: [message(8), message(9)], has_more: true }, false);
    state = mergePage(state, ROOM, { messages: [message(6), message(7)], has_more: false }, true);
    expect(state.rooms[ROOM].messages.map((m) => m.seq)).toEqual([6, 7, 8, 9]);
    expect(state.rooms[ROOM].hasMore).toBe(false);

    // 重新连上后只拉最新一页：更早的那些不再保证连续，丢掉，等用户再往前翻。
    state = mergePage(state, ROOM, { messages: [message(9), message(10)], has_more: true }, false);
    expect(state.rooms[ROOM].messages.map((m) => m.seq)).toEqual([9, 10]);
    expect(state.rooms[ROOM].hasMore).toBe(true);
  });

  it("applies held pieces once the page brings their message", () => {
    let state = applyDelta(watching, delta(1, 5, "了"));
    state = mergePage(state, ROOM, { messages: [draft(1, "写到这里", 4)], has_more: false }, false);
    expect(contents(state)).toEqual(["写到这里了"]);
  });
});

describe("resetChat", () => {
  it("forgets everything but keeps watching the open room", () => {
    expect(resetChat(null)).toEqual(emptyChat);
    const state = resetChat(ROOM);
    expect(state.rooms[ROOM]).toEqual({ messages: [], hasMore: false, loaded: false });
  });
});

describe("removeMessage / setActivity", () => {
  const watching = watchRoom(emptyChat, ROOM);
  const activity = { room_id: ROOM, speaking: "bot_1", queued: ["bot_2"], handled_seq: 1 };

  it("drops a withdrawn message and ignores unknown ones", () => {
    const state = upsertMessage(upsertMessage(watching, message(1)), message(2));
    const after = removeMessage(state, { id: "msg_2", room_id: ROOM });
    expect(after.rooms[ROOM].messages.map((m) => m.id)).toEqual(["msg_1"]);
    expect(removeMessage(after, { id: "msg_2", room_id: ROOM })).toBe(after);
    expect(removeMessage(after, { id: "msg_1", room_id: "room_other" })).toBe(after);
  });

  it("remembers who is speaking and keeps it when a page of messages arrives", () => {
    let state = setActivity(watching, activity);
    state = mergePage(state, ROOM, { messages: [message(1)], has_more: false }, false);
    expect(state.rooms[ROOM].activity).toEqual(activity);
    expect(state.rooms[ROOM].loaded).toBe(true);
    // 没打开的房间不记。
    expect(setActivity(watching, { ...activity, room_id: "room_other" })).toBe(watching);
  });
});
