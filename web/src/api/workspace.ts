import type { Bot, BotRef, Event, Room } from "./types";

export interface Workspace {
  readonly rooms: readonly Room[];
  readonly bots: readonly Bot[];
}

export const emptyWorkspace: Workspace = { rooms: [], bots: [] };

/** 把一条事件应用到本地状态。同一条事件应用多次，结果不变。 */
export function applyEvent(state: Workspace, event: Event): Workspace {
  switch (event.kind) {
    case "room.created": {
      const room = event.payload as Room;
      return { ...state, rooms: [room, ...state.rooms.filter((r) => r.id !== room.id)] };
    }
    case "room.updated": {
      const room = event.payload as Room;
      // 位置不变：改名、换成员不该让房间在列表里跳来跳去。
      const known = state.rooms.some((r) => r.id === room.id);
      return {
        ...state,
        rooms: known ? state.rooms.map((r) => (r.id === room.id ? room : r)) : [room, ...state.rooms],
      };
    }
    case "bot.created":
    case "bot.updated": {
      const bot = event.payload as Bot;
      const known = state.bots.some((b) => b.id === bot.id);
      return {
        ...state,
        bots: known ? state.bots.map((b) => (b.id === bot.id ? bot : b)) : [...state.bots, bot],
      };
    }
    case "bot.deleted": {
      const gone = event.payload as BotRef;
      return { ...state, bots: state.bots.filter((b) => b.id !== gone.id) };
    }
    default:
      // 消息类事件归 chat.ts 管。
      return state;
  }
}
