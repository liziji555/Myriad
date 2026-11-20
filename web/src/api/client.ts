import type {
  ApiError,
  Bot,
  CreateRoomRequest,
  Health,
  Message,
  MessagePage,
  Room,
  RoomId,
  StopResponse,
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
  return response.json() as Promise<T>;
}

function postJson<T>(path: string, body: unknown): Promise<T> {
  return request<T>(path, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
}

export const api = {
  health: () => request<Health>("/api/health"),
  listRooms: () => request<Room[]>("/api/rooms"),
  listBots: () => request<Bot[]>("/api/bots"),
  createRoom: (body: CreateRoomRequest) => postJson<Room>("/api/rooms", body),
  /** 房间里 `before`（某条消息的 seq）之前的一页消息；不给就是最新的一页。 */
  listMessages: (roomId: RoomId, before?: number) =>
    request<MessagePage>(
      `/api/rooms/${encodeURIComponent(roomId)}/messages${before === undefined ? "" : `?before=${before}`}`,
    ),
  postMessage: (roomId: RoomId, content: string) =>
    postJson<Message>(`/api/rooms/${encodeURIComponent(roomId)}/messages`, { content }),
  stopReply: (roomId: RoomId) =>
    postJson<StopResponse>(`/api/rooms/${encodeURIComponent(roomId)}/stop`, {}),
};
