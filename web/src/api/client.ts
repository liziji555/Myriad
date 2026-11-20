import type {
  ApiError,
  ApprovalDecision,
  ApprovalId,
  Bot,
  BotId,
  CreateBotRequest,
  CreateRoomRequest,
  CreateLinkRequest,
  CreateRoutineRequest,
  CreateTaskRequest,
  Health,
  Message,
  MessagePage,
  Notice,
  NoticeId,
  Room,
  RoomId,
  Link,
  LinkId,
  Pairing,
  Routine,
  RoutineId,
  StopResponse,
  Task,
  TaskActionRequest,
  TaskDetail,
  TaskId,
  UpdateBotRequest,
  UpdateRoomRequest,
  UpdateRoutineRequest,
  UsageSummary,
} from "./types";

export class ApiRequestError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
  ) {
    super(message);
  }
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  let response: Response;
  try {
    response = await fetch(path, init);
  } catch {
    throw new ApiRequestError(0, "network", "连不上本机服务，请确认 sa serve 正在运行");
  }
  if (!response.ok) {
    const body: Partial<ApiError> = await response.json().catch(() => ({}));
    throw new ApiRequestError(
      response.status,
      body.code ?? "http_error",
      body.message ?? `请求失败（${response.status}）`,
    );
  }
  // 204 没有内容可读。
  if (response.status === 204) return undefined as T;
  return response.json() as Promise<T>;
}

function postJson<T>(path: string, body: unknown, method = "POST"): Promise<T> {
  return request<T>(path, {
    method,
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
}

const taskPath = (taskId: TaskId, action?: string) =>
  `/api/tasks/${encodeURIComponent(taskId)}${action ? `/${action}` : ""}`;

const memberPath = (roomId: RoomId, botId: BotId) =>
  `/api/rooms/${encodeURIComponent(roomId)}/members/${encodeURIComponent(botId)}`;

const routinePath = (id: RoutineId, action?: string) =>
  `/api/routines/${encodeURIComponent(id)}${action ? `/${action}` : ""}`;

export const api = {
  health: () => request<Health>("/api/health"),
  listRooms: () => request<Room[]>("/api/rooms"),
  listBots: () => request<Bot[]>("/api/bots"),
  createRoom: (body: CreateRoomRequest) => postJson<Room>("/api/rooms", body),
  updateRoom: (roomId: RoomId, body: UpdateRoomRequest) =>
    postJson<Room>(`/api/rooms/${encodeURIComponent(roomId)}`, body, "PATCH"),
  addMember: (roomId: RoomId, botId: BotId) =>
    request<Room>(memberPath(roomId, botId), { method: "PUT" }),
  removeMember: (roomId: RoomId, botId: BotId) =>
    request<Room>(memberPath(roomId, botId), { method: "DELETE" }),
  createBot: (body: CreateBotRequest) => postJson<Bot>("/api/bots", body),
  updateBot: (botId: BotId, body: UpdateBotRequest) =>
    postJson<Bot>(`/api/bots/${encodeURIComponent(botId)}`, body, "PATCH"),
  deleteBot: (botId: BotId) =>
    request<void>(`/api/bots/${encodeURIComponent(botId)}`, { method: "DELETE" }),
  /** 房间里 `before`（某条消息的 seq）之前的一页消息；不给就是最新的一页。 */
  listMessages: (roomId: RoomId, before?: number) =>
    request<MessagePage>(
      `/api/rooms/${encodeURIComponent(roomId)}/messages${before === undefined ? "" : `?before=${before}`}`,
    ),
  postMessage: (roomId: RoomId, content: string) =>
    postJson<Message>(`/api/rooms/${encodeURIComponent(roomId)}/messages`, { content }),
  stopReply: (roomId: RoomId) =>
    postJson<StopResponse>(`/api/rooms/${encodeURIComponent(roomId)}/stop`, {}),
  /** 答复一张审批卡。卡片已经处理过或过期时抛出 `approval_closed`。 */
  decideApproval: (id: ApprovalId, decision: ApprovalDecision) =>
    postJson<void>(`/api/approvals/${encodeURIComponent(id)}/decide`, { decision }),
  listTasks: (roomId: RoomId) => request<Task[]>(`/api/rooms/${encodeURIComponent(roomId)}/tasks`),
  createTask: (roomId: RoomId, body: CreateTaskRequest) =>
    postJson<Task>(`/api/rooms/${encodeURIComponent(roomId)}/tasks`, body),
  /** 一个任务和它的执行历史。 */
  getTask: (taskId: TaskId) => request<TaskDetail>(taskPath(taskId)),
  /** 让草稿、等回答、卡住的任务继续；任务正在做时只把 `note` 带给它。 */
  resumeTask: (taskId: TaskId, body: TaskActionRequest) =>
    postJson<Task>(taskPath(taskId, "resume"), body),
  /** 验收通过。改动带不回工作目录时抛出 `conflict`，任务留在等验收。 */
  acceptTask: (taskId: TaskId) => postJson<Task>(taskPath(taskId, "accept"), {}),
  rejectTask: (taskId: TaskId, note: string) =>
    postJson<Task>(taskPath(taskId, "reject"), { note }),
  stopTask: (taskId: TaskId) => postJson<StopResponse>(taskPath(taskId, "stop"), {}),
  cancelTask: (taskId: TaskId) => postJson<Task>(taskPath(taskId, "cancel"), {}),
  /** 今天和最近几天花了多少。 */
  usage: () => request<UsageSummary>("/api/usage"),
  /** 还没收起来的提醒，新的在前。 */
  listNotices: () => request<Notice[]>("/api/notices"),
  dismissNotice: (id: NoticeId) =>
    postJson<Notice>(`/api/notices/${encodeURIComponent(id)}/dismiss`, {}),
  listRoutines: () => request<Routine[]>("/api/routines"),
  createRoutine: (body: CreateRoutineRequest) => postJson<Routine>("/api/routines", body),
  updateRoutine: (id: RoutineId, body: UpdateRoutineRequest) =>
    postJson<Routine>(routinePath(id), body, "PATCH"),
  deleteRoutine: (id: RoutineId) => request<void>(routinePath(id), { method: "DELETE" }),
  /** 现在就做一次。马上返回；做完之后结果由事件流送来。房间里正在对话时抛出 `conflict`。 */
  runRoutine: (id: RoutineId) => postJson<Routine>(routinePath(id, "run"), {}),
  listLinks: () => request<Link[]>("/api/links"),
  /** 把一个 GitHub 仓库连到房间。 */
  createLink: (body: CreateLinkRequest) => postJson<Link>("/api/links", body),
  deleteLink: (id: LinkId) =>
    request<void>(`/api/links/${encodeURIComponent(id)}`, { method: "DELETE" }),
  /** 给房间生成一个 Telegram 配对码。码只给这一次。 */
  createPairing: (roomId: RoomId) =>
    postJson<Pairing>(`/api/rooms/${encodeURIComponent(roomId)}/pairing`, {}),
};
