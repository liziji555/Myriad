import type {
  EditQueuedRequest,
  QueuedId,
  RoomQueue,
  ApiError,
  ApprovalDecision,
  ApprovalId,
  Share,
  AuditPage,
  Bot,
  BotId,
  CreateBotRequest,
  CreateRoomRequest,
  CreateLinkRequest,
  CreateRoutineRequest,
  Extensions,
  ModelSettings,
  SaveModelsRequest,
  TestModelRequest,
  TestModelResult,
  CreateTaskRequest,
  ContextUsage,
  HarnessDesk,
  Health,
  ImportReport,
  ImportTemplateRequest,
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
  TemplateFile,
  UpdateApplyResult,
  UpdateBotRequest,
  UpdateRoomRequest,
  UpdateStatus,
  UpdateRoutineRequest,
  UsageSummary,
} from "./types";
import { auditSearch, type AuditQuery } from "./audit";

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
  roomContext: (roomId: RoomId) =>
    request<ContextUsage | null>(`/api/rooms/${encodeURIComponent(roomId)}/context`),
  listDesks: (roomId: RoomId) =>
    request<HarnessDesk[]>(`/api/rooms/${encodeURIComponent(roomId)}/desks`),
  closeDesk: (roomId: RoomId, botId: BotId, how: "accept" | "discard") =>
    request<HarnessDesk[]>(
      `/api/rooms/${encodeURIComponent(roomId)}/desks/${encodeURIComponent(botId)}/${how}`,
      { method: "POST" },
    ),
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
  listQueue: (roomId: RoomId) =>
    request<RoomQueue>(`/api/rooms/${encodeURIComponent(roomId)}/queue`),
  /** 排到待发队列最后：房间里的 Bot 都说完了再按顺序发出去。 */
  queueMessage: (roomId: RoomId, content: string) =>
    postJson<RoomQueue>(`/api/rooms/${encodeURIComponent(roomId)}/queue`, { content }),
  /** 改字或挪一位。已经发出去的会失败。 */
  editQueued: (id: QueuedId, body: EditQueuedRequest) =>
    postJson<RoomQueue>(`/api/queue/${encodeURIComponent(id)}`, body, "PATCH"),
  cancelQueued: (id: QueuedId) =>
    request<RoomQueue>(`/api/queue/${encodeURIComponent(id)}`, { method: "DELETE" }),
  stopReply: (roomId: RoomId) =>
    postJson<StopResponse>(`/api/rooms/${encodeURIComponent(roomId)}/stop`, {}),
  /** 只停房间里的一个 Bot：它正在说的这一轮和排着的队，别的 Bot 照常。 */
  stopBot: (roomId: RoomId, botId: BotId) =>
    postJson<StopResponse>(
      `/api/rooms/${encodeURIComponent(roomId)}/bots/${encodeURIComponent(botId)}/stop`,
      {},
    ),
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
  /** 取消任务。`note` 是原因，可以为空。 */
  cancelTask: (taskId: TaskId, note: string) =>
    postJson<Task>(taskPath(taskId, "cancel"), { note }),
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
  /** 这个房间的团队写成的模板文件：成员、谁是入口、房间叫什么。 */
  exportRoom: (roomId: RoomId) =>
    request<TemplateFile>(`/api/templates/rooms/${encodeURIComponent(roomId)}`),
  /** 所有 Bot 写成的模板文件。 */
  exportBots: () => request<TemplateFile>("/api/templates/bots"),
  /** 导入一份模板。已经有同名的 Bot 跳过；模板有一处不对就什么都不建，抛出原因。 */
  importTemplate: (body: ImportTemplateRequest) =>
    postJson<ImportReport>("/api/templates/import", body),
  /** 审计日志的一页，新的在前。 */
  audit: (query: AuditQuery) => request<AuditPage>(`/api/audit${auditSearch(query)}`),
  /** 现在有哪些技能、哪些外部 MCP 服务。 */
  extensions: () => request<Extensions>("/api/extensions"),
  models: () => request<ModelSettings>("/api/models"),
  saveModels: (body: SaveModelsRequest) => postJson<ModelSettings>("/api/models", body, "PUT"),
  testModel: (body: TestModelRequest) => postJson<TestModelResult>("/api/models/test", body),
  listShares: () => request<Share[]>("/api/shares"),
  listLinks: () => request<Link[]>("/api/links"),
  /** 把一个 GitHub 仓库连到房间。 */
  createLink: (body: CreateLinkRequest) => postJson<Link>("/api/links", body),
  deleteLink: (id: LinkId) =>
    request<void>(`/api/links/${encodeURIComponent(id)}`, { method: "DELETE" }),
  /** 给房间生成一个 Telegram 配对码。码只给这一次。 */
  createPairing: (roomId: RoomId) =>
    postJson<Pairing>(`/api/rooms/${encodeURIComponent(roomId)}/pairing`, {}),
  /** 检查服务端在 GitHub Releases 上有没有新版本。 */
  checkUpdate: () => request<UpdateStatus>("/api/update"),
  /** 下载校验并替换服务端二进制，必要时触发自动重启。 */
  applyUpdate: () => postJson<UpdateApplyResult>("/api/update", {}),
};
