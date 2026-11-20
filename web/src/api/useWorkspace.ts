import { useCallback, useEffect, useState } from "react";
import { ApiRequestError, api } from "./client";
import type { Event, Room } from "./types";
import { applyEvent, emptyWorkspace, type Workspace } from "./workspace";

export type Phase = "loading" | "ready" | "unauthorized";

export interface WorkspaceView extends Workspace {
  readonly phase: Phase;
  /** 事件流当前是否连着。断开时浏览器会自动重连。 */
  readonly connected: boolean;
  createRoom(name: string): Promise<Room>;
}

/**
 * 工作区状态 = 快照 + 事件流。
 * 每次（重新）连上事件流，服务端先发 `ready`，这时拉一次快照；拉取期间到达的事件先攒着，
 * 快照到了再补上，避免旧快照盖掉新事件。
 */
export function useWorkspace(): WorkspaceView {
  const [workspace, setWorkspace] = useState<Workspace>(emptyWorkspace);
  const [phase, setPhase] = useState<Phase>("loading");
  const [connected, setConnected] = useState(false);

  useEffect(() => {
    const source = new EventSource("/api/events");
    let closed = false;
    let buffered: Event[] | null = null;

    source.addEventListener("ready", async () => {
      setConnected(true);
      buffered = [];
      try {
        const [rooms, bots] = await Promise.all([api.listRooms(), api.listBots()]);
        if (closed) return;
        setWorkspace(buffered.reduce(applyEvent, { rooms, bots }));
        setPhase("ready");
      } catch {
        // 快照没拉到：保持现状，下一次 ready 会重试。
      } finally {
        buffered = null;
      }
    });

    source.onmessage = (message) => {
      const event = JSON.parse(message.data) as Event;
      if (buffered) buffered.push(event);
      else setWorkspace((current) => applyEvent(current, event));
    };

    source.onerror = async () => {
      setConnected(false);
      // EventSource 不告诉我们失败原因，用一次普通请求区分「没有令牌」和「服务没开」。
      try {
        await api.listRooms();
      } catch (error) {
        if (!closed && error instanceof ApiRequestError && error.status === 401) {
          setPhase("unauthorized");
          source.close();
        }
      }
    };

    return () => {
      closed = true;
      source.close();
    };
  }, []);

  const createRoom = useCallback(async (name: string) => {
    const room = await api.createRoom({ name });
    setWorkspace((current) =>
      applyEvent(current, { seq: 0, room_id: room.id, kind: "room.created", payload: room, created_at: room.created_at }),
    );
    return room;
  }, []);

  return { ...workspace, phase, connected, createRoom };
}
