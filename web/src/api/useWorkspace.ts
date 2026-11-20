import { useCallback, useEffect, useRef, useState } from "react";
import {
  applyDelta,
  emptyChat,
  mergePage,
  resetChat,
  upsertMessage,
  watchRoom,
  type ChatState,
} from "./chat";
import { ApiRequestError, api } from "./client";
import type { Event, Message, MessageDelta, Room, RoomId } from "./types";
import { applyEvent, emptyWorkspace, type Workspace } from "./workspace";

export type Phase = "loading" | "ready" | "unauthorized";

export interface WorkspaceView extends Workspace {
  readonly phase: Phase;
  /** 事件流当前是否连着。断开时浏览器会自动重连。 */
  readonly connected: boolean;
  readonly chat: ChatState;
  /** 每次需要重新拉消息时加一（重新连上、或者片段丢了）。 */
  readonly epoch: number;
  createRoom(name: string): Promise<Room>;
  /** 打开一个房间：关注它的消息事件，并拉最新一页。失败时抛出。 */
  openRoom(roomId: RoomId): Promise<void>;
  /** 再往前拉一页。 */
  loadOlder(roomId: RoomId): Promise<void>;
  sendMessage(roomId: RoomId, content: string): Promise<void>;
  stopReply(roomId: RoomId): Promise<void>;
}

const SNAPSHOT_RETRY_MS = 2000;

function applyChatEvent(chat: ChatState, event: Event): ChatState {
  return event.kind === "message.created" || event.kind === "message.updated"
    ? upsertMessage(chat, event.payload as Message)
    : chat;
}

/**
 * 工作区状态 = 快照 + 事件流。
 * 每次（重新）连上事件流，服务端先发 `ready`，这时拉一次快照；快照到手之前到达的事件先攒着，
 * 之后再补上，避免旧快照盖掉新事件。快照没拉到就隔一会儿重试，期间事件继续攒着。
 *
 * 消息另外按房间拉：打开房间时拉最新一页，之后靠事件和片段跟上。片段不落库，
 * 所以重新连上或者服务端说丢了片段（`resync`）时，把消息清掉让界面重新拉。
 */
export function useWorkspace(): WorkspaceView {
  const [workspace, setWorkspace] = useState<Workspace>(emptyWorkspace);
  const [chat, setChat] = useState<ChatState>(emptyChat);
  const [epoch, setEpoch] = useState(0);
  const [phase, setPhase] = useState<Phase>("loading");
  const [connected, setConnected] = useState(false);
  /** 界面上正开着的房间。重置消息时保留对它的关注，期间到达的事件不会丢。 */
  const activeRoom = useRef<RoomId | null>(null);
  const chatRef = useRef(chat);
  chatRef.current = chat;

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

    const reloadMessages = () => {
      setChat(resetChat(activeRoom.current));
      setEpoch((n) => n + 1);
    };

    source.addEventListener("ready", () => {
      setConnected(true);
      clearTimeout(retry);
      buffered = [];
      void loadSnapshot(++generation);
      reloadMessages();
    });

    source.addEventListener("resync", reloadMessages);

    source.addEventListener("delta", (message) => {
      const delta = JSON.parse((message as MessageEvent<string>).data) as MessageDelta;
      setChat((current) => applyDelta(current, delta));
    });

    source.onmessage = (message) => {
      const event = JSON.parse(message.data) as Event;
      setChat((current) => applyChatEvent(current, event));
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

  const openRoom = useCallback(async (roomId: RoomId) => {
    activeRoom.current = roomId;
    // 先关注再拉：拉取期间事件流送来的消息会留下来，和拉到的那一页合并。
    setChat((current) => watchRoom(current, roomId));
    const page = await api.listMessages(roomId);
    setChat((current) => mergePage(current, roomId, page, false));
  }, []);

  const loadOlder = useCallback(async (roomId: RoomId) => {
    const oldest = chatRef.current.rooms[roomId]?.messages[0];
    if (!oldest) return;
    const page = await api.listMessages(roomId, oldest.seq);
    setChat((current) => mergePage(current, roomId, page, true));
  }, []);

  const sendMessage = useCallback(async (roomId: RoomId, content: string) => {
    const message = await api.postMessage(roomId, content);
    // 事件流也会送来同一条；先放进去，发送的人不用等。
    setChat((current) => upsertMessage(current, message));
  }, []);

  const stopReply = useCallback(async (roomId: RoomId) => {
    await api.stopReply(roomId);
  }, []);

  return {
    ...workspace,
    phase,
    connected,
    chat,
    epoch,
    createRoom,
    openRoom,
    loadOlder,
    sendMessage,
    stopReply,
  };
}
