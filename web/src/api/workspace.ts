import type { Bot, BotRef, Event, Notice, Room, RoomId, Routine, RoutineRef } from "./types";

export interface Workspace {
  readonly rooms: readonly Room[];
  readonly bots: readonly Bot[];
  /** 还没收起来的提醒，新的在前。 */
  readonly notices: readonly Notice[];
  /** 所有房间的定时任务，按建的先后。 */
  readonly routines: readonly Routine[];
}

export const emptyWorkspace: Workspace = { rooms: [], bots: [], notices: [], routines: [] };

/** 在这个房间里要显示的提醒：关于这个房间的，和不专属于哪个房间的。 */
export function noticesFor(notices: readonly Notice[], roomId: RoomId | null): Notice[] {
  return notices.filter((notice) => notice.room_id === null || notice.room_id === roomId);
}

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
      // 它负责的定时任务跟着一起没了。
      return {
        ...state,
        bots: state.bots.filter((b) => b.id !== gone.id),
        routines: state.routines.filter((r) => r.bot_id !== gone.id),
      };
    }
    case "routine.created":
    case "routine.updated": {
      const routine = event.payload as Routine;
      const known = state.routines.some((r) => r.id === routine.id);
      return {
        ...state,
        routines: known
          ? state.routines.map((r) => (r.id === routine.id ? routine : r))
          : [...state.routines, routine],
      };
    }
    case "routine.deleted": {
      const gone = event.payload as RoutineRef;
      return { ...state, routines: state.routines.filter((r) => r.id !== gone.id) };
    }
    case "notice.created":
    case "notice.updated": {
      const notice = event.payload as Notice;
      const others = state.notices.filter((n) => n.id !== notice.id);
      if (notice.dismissed_at !== null) return { ...state, notices: others };
      const notices = [notice, ...others].sort(
        (a, b) => b.created_at - a.created_at || b.id.localeCompare(a.id),
      );
      return { ...state, notices };
    }
    default:
      // 消息类事件归 chat.ts 管。
      return state;
  }
}
