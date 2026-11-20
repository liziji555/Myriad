import type { ApiError, Bot, CreateRoomRequest, Health, Room } from "./types";

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
};
