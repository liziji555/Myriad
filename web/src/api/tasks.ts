import type { Message, RoomId, Run, RunId, Task, TaskId, TaskState } from "./types";

export interface TaskBoard {
  /** 只有打开过的房间在这里，按创建时间升序；其他房间的任务事件直接忽略，打开时再拉。 */
  readonly rooms: Readonly<Record<RoomId, readonly Task[]>>;
  /** 见过的运行：事件流送来的，和展开任务详情时拉到的。 */
  readonly runs: Readonly<Record<RunId, Run>>;
}

export const emptyBoard: TaskBoard = { rooms: {}, runs: {} };

/** 开始关注一个房间的任务：之后它的任务事件会被记下来。 */
export function watchTasks(state: TaskBoard, roomId: RoomId): TaskBoard {
  if (state.rooms[roomId]) return state;
  return { ...state, rooms: { ...state.rooms, [roomId]: [] } };
}

/** 丢掉所有任务（重新连上之后用），只保留对 `keep` 的关注。 */
export function resetBoard(keep: RoomId | null): TaskBoard {
  return keep ? watchTasks(emptyBoard, keep) : emptyBoard;
}

const byCreation = (a: Task, b: Task) => a.created_at - b.created_at || a.id.localeCompare(b.id);

/** 放进一个任务或它的新版本。版本比手上旧的会被忽略。 */
export function upsertTask(state: TaskBoard, task: Task): TaskBoard {
  const tasks = state.rooms[task.room_id];
  if (!tasks) return state;
  const mine = tasks.find((t) => t.id === task.id);
  if (mine && mine.rev >= task.rev) return state;
  const next = mine ? tasks.map((t) => (t.id === task.id ? task : t)) : [...tasks, task].sort(byCreation);
  return { ...state, rooms: { ...state.rooms, [task.room_id]: next } };
}

/** 拉到的任务列表并进来。拉取期间事件送来的更新的版本保留。 */
export function mergeTasks(state: TaskBoard, roomId: RoomId, listed: readonly Task[]): TaskBoard {
  const mine = state.rooms[roomId];
  if (!mine) return state;
  const byId = new Map(listed.map((task) => [task.id, task]));
  for (const task of mine) {
    const theirs = byId.get(task.id);
    if (!theirs || task.rev > theirs.rev) byId.set(task.id, task);
  }
  return { ...state, rooms: { ...state.rooms, [roomId]: [...byId.values()].sort(byCreation) } };
}

/** 记下一次运行的最新样子。运行没有版本号：已经结束的不会被迟到的「进行中」盖掉。 */
export function upsertRun(state: TaskBoard, run: Run): TaskBoard {
  const mine = state.runs[run.id];
  if (mine && mine.state !== "running" && run.state === "running") return state;
  return { ...state, runs: { ...state.runs, [run.id]: run } };
}

export function upsertRuns(state: TaskBoard, runs: readonly Run[]): TaskBoard {
  return runs.reduce(upsertRun, state);
}

/** 一个任务见过的运行，按先后。 */
export function runsOf(state: TaskBoard, taskId: TaskId): Run[] {
  return Object.values(state.runs)
    .filter((run) => run.task_id === taskId)
    .sort((a, b) => a.attempt - b.attempt);
}

export const STATE_LABEL: Record<TaskState, string> = {
  draft: "草稿",
  open: "等着开工",
  working: "正在做",
  waiting: "等你回答",
  review: "等你验收",
  blocked: "卡住了",
  done: "完成",
  dropped: "不需要做",
  cancelled: "已取消",
};

/** 任务页里各组的先后：要人处理的在最前面，结束了的在最后。 */
export const STATE_ORDER: readonly TaskState[] = [
  "review",
  "waiting",
  "blocked",
  "working",
  "open",
  "draft",
  "done",
  "dropped",
  "cancelled",
];

export const isFinished = (state: TaskState) =>
  state === "done" || state === "dropped" || state === "cancelled";

/** 按状态分组，组的先后见 `STATE_ORDER`；组里新的在前。空的组不出现。 */
export function groupTasks(tasks: readonly Task[]): { state: TaskState; tasks: Task[] }[] {
  return STATE_ORDER.map((state) => ({
    state,
    tasks: tasks.filter((task) => task.state === state).reverse(),
  })).filter((group) => group.tasks.length > 0);
}

/** 这张交付卡是不是这个任务最新的一张：只有最新的那张上能验收、能回答。 */
export function isLatestDelivery(messages: readonly Message[], card: Message): boolean {
  const latest = [...messages]
    .reverse()
    .find((m) => m.kind === "delivery" && m.task_id === card.task_id);
  return latest?.id === card.id;
}
