import type {
  ApiError,
  ApprovalDecision,
  ApprovalId,
  Bot,
  BotId,
  CreateBotRequest,
  CreateRoomRequest,
  Health,
  Message,
  MessagePage,
  Room,
  RoomId,
  StopResponse,
  UpdateBotRequest,
  UpdateRoomRequest,
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

const memberPath = (roomId: RoomId, botId: BotId) =>
  `/api/rooms/${encodeURIComponent(roomId)}/members/${encodeURIComponent(botId)}`;

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
};
