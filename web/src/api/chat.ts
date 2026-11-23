import type {
  Message,
  MessageDelta,
  MessagePage,
  MessageRef,
  QueuedInput,
  RoomActivity,
  RoomId,
  RoomQueue,
} from "./types";

export interface RoomChat {
  /** 按 `seq` 升序。 */
  readonly messages: readonly Message[];
  /** 更早的消息还有没有。 */
  readonly hasMore: boolean;
  /** 最新一页拉到了没有。没拉到之前 `messages` 里只有事件流送来的消息。 */
  readonly loaded: boolean;
  /** 现在谁在说话、谁在排队。没收到过就是没有。 */
  readonly activity?: RoomActivity;
  /** 待发队列：Bot 忙的时候说的话，排在前面的先发。没拉到之前是空的。 */
  readonly queue: readonly QueuedInput[];
}

export interface ChatState {
  /** 只有打开过的房间在这里；其他房间的消息事件直接忽略，打开时再拉。 */
  readonly rooms: Readonly<Record<RoomId, RoomChat>>;
  /** 来早了的片段：对应的消息还没到，或者中间缺了版本。 */
  readonly pending: readonly MessageDelta[];
}

export const emptyChat: ChatState = { rooms: {}, pending: [] };

const emptyRoom: RoomChat = { messages: [], hasMore: false, loaded: false, queue: [] };
const MAX_PENDING = 2000;

/** 开始关注一个房间：之后它的消息事件会被记下来。 */
export function watchRoom(state: ChatState, roomId: RoomId): ChatState {
  if (state.rooms[roomId]) return state;
  return { ...state, rooms: { ...state.rooms, [roomId]: emptyRoom } };
}

/** 丢掉所有房间的消息（重新连上之后用），只保留对 `keep` 的关注。 */
export function resetChat(keep: RoomId | null): ChatState {
  return keep ? watchRoom(emptyChat, keep) : emptyChat;
}

/** 放进一条消息或它的新版本。版本比手上旧的会被忽略，所以重复和乱序都不会造成回退。 */
export function upsertMessage(state: ChatState, message: Message): ChatState {
  const room = state.rooms[message.room_id];
  if (!room) return state;
  const index = room.messages.findIndex((m) => m.id === message.id);
  if (index >= 0 && room.messages[index].rev >= message.rev) return state;
  const messages =
    index >= 0
      ? room.messages.map((m, i) => (i === index ? message : m))
      : [...room.messages, message].sort((a, b) => a.seq - b.seq);
  return drainPending({
    ...state,
    rooms: { ...state.rooms, [message.room_id]: { ...room, messages } },
  });
}

/** 一条消息被撤掉了（Bot 决定不说话）。 */
export function removeMessage(state: ChatState, gone: MessageRef): ChatState {
  const room = state.rooms[gone.room_id];
  if (!room || !room.messages.some((m) => m.id === gone.id)) return state;
  const messages = room.messages.filter((m) => m.id !== gone.id);
  return { ...state, rooms: { ...state.rooms, [gone.room_id]: { ...room, messages } } };
}

/** 房间里谁在说话、谁在排队变了。 */
export function setActivity(state: ChatState, activity: RoomActivity): ChatState {
  const room = state.rooms[activity.room_id];
  if (!room) return state;
  return { ...state, rooms: { ...state.rooms, [activity.room_id]: { ...room, activity } } };
}

/** 房间的待发队列变了。总是整个替换：接口和事件流给的都是完整的队列。 */
export function setQueue(state: ChatState, queue: RoomQueue): ChatState {
  const room = state.rooms[queue.room_id];
  if (!room) return state;
  return { ...state, rooms: { ...state.rooms, [queue.room_id]: { ...room, queue: queue.items } } };
}

/** 拉到的一页消息并进来。`older` 表示这是往前翻的一页，否则是最新的一页。 */
export function mergePage(
  state: ChatState,
  roomId: RoomId,
  page: MessagePage,
  older: boolean,
): ChatState {
  const room = state.rooms[roomId];
  if (!room) return state;
  const byId = new Map(page.messages.map((m) => [m.id, m]));
  // 手上已有的消息：版本更新的保留；最新一页里没有、但比这一页还新的（拉取期间事件送来的）也保留。
  const newest = page.messages.at(-1)?.seq ?? 0;
  for (const mine of room.messages) {
    const theirs = byId.get(mine.id);
    if (theirs ? mine.rev > theirs.rev : older || mine.seq > newest) byId.set(mine.id, mine);
  }
  const messages = [...byId.values()].sort((a, b) => a.seq - b.seq);
  const next: RoomChat = older
    ? { ...room, messages, hasMore: page.has_more }
    : { ...room, messages, hasMore: page.has_more, loaded: true };
  return drainPending({ ...state, rooms: { ...state.rooms, [roomId]: next } });
}

/** 正在写的消息新增了一段文字。只有版本正好接上才追加，否则先存着或丢掉。 */
export function applyDelta(state: ChatState, delta: MessageDelta): ChatState {
  if (!state.rooms[delta.room_id]) return state;
  const pending = [...state.pending, delta].slice(-MAX_PENDING);
  return drainPending({ ...state, pending });
}

function drainPending(state: ChatState): ChatState {
  if (state.pending.length === 0) return state;
  let rooms = state.rooms;
  let pending = [...state.pending].sort((a, b) => a.rev - b.rev);
  let progressed = true;
  while (progressed) {
    progressed = false;
    const rest: MessageDelta[] = [];
    for (const delta of pending) {
      const room = rooms[delta.room_id];
      const message = room?.messages.find((m) => m.id === delta.message_id);
      if (!room) continue;
      // 消息已经写完，或者这个片段已经包含在手上的版本里：丢掉。
      if (message && (message.state !== "streaming" || delta.rev <= message.rev)) continue;
      if (!message || delta.rev !== message.rev + 1) {
        rest.push(delta);
        continue;
      }
      const updated = { ...message, content: message.content + delta.text, rev: delta.rev };
      rooms = {
        ...rooms,
        [delta.room_id]: {
          ...room,
          messages: room.messages.map((m) => (m.id === updated.id ? updated : m)),
        },
      };
      progressed = true;
    }
    pending = rest;
  }
  return { rooms, pending };
}
