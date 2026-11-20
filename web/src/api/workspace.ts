import type { Bot, Event, Room } from "./types";

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
    case "bot.created": {
      const bot = event.payload as Bot;
      return { ...state, bots: [...state.bots.filter((b) => b.id !== bot.id), bot] };
    }
  }
}
