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

const SNAPSHOT_RETRY_MS = 2000;

/**
 * 工作区状态 = 快照 + 事件流。
 * 每次（重新）连上事件流，服务端先发 `ready`，这时拉一次快照；快照到手之前到达的事件先攒着，
 * 之后再补上，避免旧快照盖掉新事件。快照没拉到就隔一会儿重试，期间事件继续攒着。
 */
export function useWorkspace(): WorkspaceView {
  const [workspace, setWorkspace] = useState<Workspace>(emptyWorkspace);
  const [phase, setPhase] = useState<Phase>("loading");
  const [connected, setConnected] = useState(false);

  useEffect(() => {
    const source = new EventSource("/api/events");
    let closed = false;
    let buffered: Event[] | null = null;
    // 每次 `ready` 换一代；旧一代的快照和重试作废。
    let generation = 0;
    let retry: ReturnType<typeof setTimeout> | undefined;

    const stop = () => {
      closed = true;
      clearTimeout(retry);
      source.close();
    };

    const loadSnapshot = async (mine: number) => {
      try {
        const [rooms, bots] = await Promise.all([api.listRooms(), api.listBots()]);
        if (closed || mine !== generation) return;
        setWorkspace((buffered ?? []).reduce(applyEvent, { rooms, bots }));
        buffered = null;
        setPhase("ready");
      } catch (error) {
        if (closed || mine !== generation) return;
        if (error instanceof ApiRequestError && error.status === 401) {
          setPhase("unauthorized");
          stop();
          return;
        }
        retry = setTimeout(() => loadSnapshot(mine), SNAPSHOT_RETRY_MS);
      }
    };

    source.addEventListener("ready", () => {
      setConnected(true);
      clearTimeout(retry);
      buffered = [];
      void loadSnapshot(++generation);
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
          stop();
        }
      }
    };

    return stop;
  }, []);

  const createRoom = useCallback(async (name: string) => {
    const room = await api.createRoom({ name });
    setWorkspace((current) =>
      applyEvent(current, {
        seq: 0,
        room_id: room.id,
        kind: "room.created",
        payload: room,
        created_at: room.created_at,
      }),
    );
    return room;
  }, []);

  return { ...workspace, phase, connected, createRoom };
}
