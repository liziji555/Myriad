import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  applyDelta,
  emptyChat,
  mergePage,
  resetChat,
  removeMessage,
  setActivity,
  setQueue,
  upsertMessage,
  watchRoom,
  type ChatState,
} from "./chat";
import { ApiRequestError, api } from "./client";
import {
  emptyBoard,
  mergeTasks,
  resetBoard,
  upsertRun,
  upsertRuns,
  upsertTask,
  watchTasks,
  type TaskBoard,
} from "./tasks";
import type {
  ApprovalDecision,
  ApprovalId,
  Bot,
  BotId,
  CreateBotRequest,
  CreateRoutineRequest,
  Link,
  LinkId,
  Pairing,
  CreateTaskRequest,
  Event,
  Message,
  MessageDelta,
  MessageRef,
  NoticeId,
  Room,
  RoomActivity,
  RoomQueue,
  QueuedId,
  EditQueuedRequest,
  RoomId,
  Routine,
  RoutineId,
  Run,
  Task,
  TaskId,
  UpdateBotRequest,
  UpdateRoomRequest,
  UpdateRoutineRequest,
} from "./types";
import { applyEvent, emptyWorkspace, type Workspace } from "./workspace";

export type Phase = "loading" | "ready" | "unauthorized";

export interface Team {
  createBot(body: CreateBotRequest): Promise<Bot>;
  updateBot(botId: BotId, body: UpdateBotRequest): Promise<void>;
  deleteBot(botId: BotId): Promise<void>;
  updateRoom(roomId: RoomId, body: UpdateRoomRequest): Promise<void>;
  addMember(roomId: RoomId, botId: BotId): Promise<void>;
  removeMember(roomId: RoomId, botId: BotId): Promise<void>;
}

/** 对任务的操作。结果先放进本地状态，事件流稍后送来同样的内容。失败时抛出。 */
export interface TaskActions {
  create(roomId: RoomId, body: CreateTaskRequest): Promise<Task>;
  /** 让草稿、等回答、卡住的任务继续；任务正在做时只把话带给它。`ownerId` 给了就换负责人。 */
  resume(taskId: TaskId, note: string, ownerId?: BotId): Promise<void>;
  accept(taskId: TaskId): Promise<void>;
  reject(taskId: TaskId, note: string): Promise<void>;
  stop(taskId: TaskId): Promise<void>;
  /** 取消任务。`note` 是原因，可以为空。 */
  cancel(taskId: TaskId, note: string): Promise<void>;
  /** 拉一个任务的执行历史。 */
  loadRuns(taskId: TaskId): Promise<void>;
}

/** 对定时任务的操作。结果先放进本地状态，事件流稍后送来同样的内容。失败时抛出。 */
export interface RoutineActions {
  create(body: CreateRoutineRequest): Promise<void>;
  update(id: RoutineId, body: UpdateRoutineRequest): Promise<void>;
  remove(id: RoutineId): Promise<void>;
  /** 现在就做一次，不等到点。 */
  runNow(id: RoutineId): Promise<void>;
}

/** 对连接的操作。结果先放进本地状态，事件流稍后送来同样的内容。失败时抛出。 */
export interface LinkActions {
  /** 把一个 GitHub 仓库（`owner/name`）连到房间。 */
  linkRepo(roomId: RoomId, repo: string): Promise<void>;
  remove(id: LinkId): Promise<void>;
  /** 生成一个 Telegram 配对码。连上之后新的连接由事件流送来。 */
  pair(roomId: RoomId): Promise<Pairing>;
}

export interface WorkspaceView extends Workspace {
  readonly phase: Phase;
  /** 事件流当前是否连着。断开时浏览器会自动重连。 */
  readonly connected: boolean;
  readonly chat: ChatState;
  readonly board: TaskBoard;
  readonly tasks: TaskActions;
  readonly routineActions: RoutineActions;
  readonly linkActions: LinkActions;
  /** 每次需要重新拉消息时加一（重新连上、或者片段丢了）。 */
  readonly epoch: number;
  createRoom(name: string): Promise<Room>;
  /** Bot 和房间成员的管理。结果先放进本地状态，事件流稍后送来同样的内容。 */
  readonly team: Team;
  /** 打开一个房间：关注它的消息事件，并拉最新一页。失败时抛出。 */
  openRoom(roomId: RoomId): Promise<void>;
  /** 再往前拉一页。 */
  loadOlder(roomId: RoomId): Promise<void>;
  sendMessage(roomId: RoomId, content: string): Promise<void>;
  stopReply(roomId: RoomId): Promise<void>;
  /** 只停房间里的一个 Bot。 */
  stopBot(roomId: RoomId, botId: BotId): Promise<void>;
  /** Bot 忙的时候说的话排进待发队列，房间里的 Bot 都说完了按顺序发。 */
  queueMessage(roomId: RoomId, content: string): Promise<void>;
  /** 改排着的一句（改字、挪一位）。已经发出去的会失败。 */
  editQueued(id: QueuedId, edit: EditQueuedRequest): Promise<void>;
  cancelQueued(id: QueuedId): Promise<void>;
  decideApproval(id: ApprovalId, decision: ApprovalDecision): Promise<void>;
  /** 收起一条提醒。 */
  dismissNotice(id: NoticeId): Promise<void>;
}

const SNAPSHOT_RETRY_MS = 2000;

function applyChatEvent(chat: ChatState, event: Event): ChatState {
  switch (event.kind) {
    case "message.created":
    case "message.updated":
      return upsertMessage(chat, event.payload as Message);
    case "message.deleted":
      return removeMessage(chat, event.payload as MessageRef);
    case "room.activity":
      return setActivity(chat, event.payload as RoomActivity);
    case "queue.updated":
      return setQueue(chat, event.payload as RoomQueue);
    default:
      return chat;
  }
}

function applyTaskEvent(board: TaskBoard, event: Event): TaskBoard {
  switch (event.kind) {
    case "task.created":
    case "task.updated":
      return upsertTask(board, event.payload as Task);
    case "run.updated":
      return upsertRun(board, event.payload as Run);
    default:
      return board;
  }
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
  const [board, setBoard] = useState<TaskBoard>(emptyBoard);
  const [epoch, setEpoch] = useState(0);
  const [phase, setPhase] = useState<Phase>("loading");
  const [connected, setConnected] = useState(false);
  /** 界面上正开着的房间。重置消息时保留对它的关注，期间到达的事件不会丢。 */
  const activeRoom = useRef<RoomId | null>(null);
  const chatRef = useRef(chat);
  chatRef.current = chat;
  /** 每次 `ready`/`resync` 重置消息时递增，用来作废在途的 `openRoom`/`loadOlder`。 */
  const resetGen = useRef(0);
  /** 每个房间的 `openRoom` 序号，防止旧请求覆盖新请求。 */
  const roomLoadGen = useRef<Record<RoomId, number>>({});
  /** 每个房间队列的更新序号（SSE 或本地改动都会递增），防止旧的 `listQueue` 覆盖新队列。 */
  const queueVersion = useRef<Record<RoomId, number>>({});

  useEffect(() => {
    let source: EventSource | null = null;
    let closed = false;
    let buffered: Event[] | null = null;
    // 每次 `ready` 换一代；旧一代的快照和重试作废。
    let generation = 0;
    let retry: ReturnType<typeof setTimeout> | undefined;
    let reconnect: ReturnType<typeof setTimeout> | undefined;
    let reconnectDelay = SNAPSHOT_RETRY_MS;

    const stop = () => {
      closed = true;
      clearTimeout(retry);
      clearTimeout(reconnect);
      source?.close();
      source = null;
    };

    const loadSnapshot = async (mine: number) => {
      try {
        const [rooms, bots, notices, routines, links] = await Promise.all([
          api.listRooms(),
          api.listBots(),
          api.listNotices(),
          api.listRoutines(),
          api.listLinks(),
        ]);
        if (closed || mine !== generation) return;
        setWorkspace(
          (buffered ?? []).reduce(applyEvent, { rooms, bots, notices, routines, links }),
        );
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
      resetGen.current += 1;
      setChat(resetChat(activeRoom.current));
      setBoard(resetBoard(activeRoom.current));
      setEpoch((n) => n + 1);
    };

    const connect = () => {
      if (closed) return;
      clearTimeout(reconnect);
      source?.close();
      const current = new EventSource("/api/events");
      source = current;

      current.addEventListener("ready", () => {
        if (closed || source !== current) return;
        reconnectDelay = SNAPSHOT_RETRY_MS;
        setConnected(true);
        clearTimeout(retry);
        buffered = [];
        void loadSnapshot(++generation);
        reloadMessages();
      });

      current.addEventListener("resync", () => {
        if (closed || source !== current) return;
        reloadMessages();
      });

      current.addEventListener("delta", (message) => {
        if (closed || source !== current) return;
        const delta = JSON.parse((message as MessageEvent<string>).data) as MessageDelta;
        setChat((state) => applyDelta(state, delta));
      });

      current.onmessage = (message) => {
        if (closed || source !== current) return;
        const event = JSON.parse(message.data) as Event;
        if (event.kind === "queue.updated" && event.room_id) {
          queueVersion.current[event.room_id] = (queueVersion.current[event.room_id] ?? 0) + 1;
        }
        setChat((state) => applyChatEvent(state, event));
        setBoard((state) => applyTaskEvent(state, event));
        if (buffered) buffered.push(event);
        else setWorkspace((state) => applyEvent(state, event));
      };

      current.onerror = async () => {
        if (closed || source !== current) return;
        setConnected(false);
        const permanentlyClosed = current.readyState === EventSource.CLOSED;
        // EventSource 不告诉我们失败原因，用一次普通请求区分「没有令牌」和「服务没开」。
        try {
          await api.listRooms();
        } catch (error) {
          if (!closed && error instanceof ApiRequestError && error.status === 401) {
            setPhase("unauthorized");
            stop();
            return;
          }
        }
        // 非 200（例如反向代理 502/503）会让浏览器把 EventSource 置为 CLOSED 并永久放弃重连，这里主动重建。
        if (!closed && source === current && permanentlyClosed) {
          const wait = reconnectDelay;
          reconnectDelay = Math.min(reconnectDelay * 2, 15_000);
          reconnect = setTimeout(connect, wait);
        }
      };
    };

    connect();
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

  const team = useMemo<Team>(() => {
    // 接口返回的就是最新的样子：当成一条事件放进去，不用等事件流。
    const apply = (kind: Event["kind"], payload: Room | Bot | { id: BotId }) =>
      setWorkspace((current) =>
        applyEvent(current, { seq: 0, room_id: null, kind, payload, created_at: 0 }),
      );
    return {
      createBot: async (body) => {
        const bot = await api.createBot(body);
        apply("bot.created", bot);
        return bot;
      },
      updateBot: async (botId, body) => apply("bot.updated", await api.updateBot(botId, body)),
      deleteBot: async (botId) => {
        await api.deleteBot(botId);
        apply("bot.deleted", { id: botId });
      },
      updateRoom: async (roomId, body) => apply("room.updated", await api.updateRoom(roomId, body)),
      addMember: async (roomId, botId) => apply("room.updated", await api.addMember(roomId, botId)),
      removeMember: async (roomId, botId) =>
        apply("room.updated", await api.removeMember(roomId, botId)),
    };
  }, []);

  const openRoom = useCallback(async (roomId: RoomId) => {
    activeRoom.current = roomId;
    const myReset = resetGen.current;
    const myGen = (roomLoadGen.current[roomId] ?? 0) + 1;
    roomLoadGen.current[roomId] = myGen;
    const myQueueVer = queueVersion.current[roomId] ?? 0;
    // 先关注再拉：拉取期间事件流送来的消息会留下来，和拉到的那一页合并。
    setChat((current) => watchRoom(current, roomId));
    setBoard((current) => watchTasks(current, roomId));
    const [page, tasks, queue] = await Promise.all([
      api.listMessages(roomId),
      api.listTasks(roomId),
      api.listQueue(roomId),
    ]);
    if (resetGen.current !== myReset || roomLoadGen.current[roomId] !== myGen) return;
    const keepLiveQueue = (queueVersion.current[roomId] ?? 0) !== myQueueVer;
    setChat((current) => {
      const merged = mergePage(current, roomId, page, false);
      return keepLiveQueue ? merged : setQueue(merged, queue);
    });
    setBoard((current) => mergeTasks(current, roomId, tasks));
  }, []);

  const tasks = useMemo<TaskActions>(() => {
    // 接口返回的就是任务最新的样子，直接放进去，不用等事件流。
    const put = (task: Task) => setBoard((current) => upsertTask(current, task));
    return {
      create: async (roomId, body) => {
        const task = await api.createTask(roomId, body);
        put(task);
        return task;
      },
      resume: async (taskId, note, ownerId) =>
        put(await api.resumeTask(taskId, { note, ...(ownerId ? { owner_bot_id: ownerId } : {}) })),
      accept: async (taskId) => put(await api.acceptTask(taskId)),
      reject: async (taskId, note) => put(await api.rejectTask(taskId, note)),
      stop: async (taskId) => {
        await api.stopTask(taskId);
      },
      cancel: async (taskId, note) => put(await api.cancelTask(taskId, note)),
      loadRuns: async (taskId) => {
        const detail = await api.getTask(taskId);
        setBoard((current) => upsertRuns(upsertTask(current, detail.task), detail.runs));
      },
    };
  }, []);

  const routineActions = useMemo<RoutineActions>(() => {
    const apply = (kind: Event["kind"], payload: Routine | { id: RoutineId }) =>
      setWorkspace((current) =>
        applyEvent(current, { seq: 0, room_id: null, kind, payload, created_at: 0 }),
      );
    return {
      create: async (body) => apply("routine.created", await api.createRoutine(body)),
      update: async (id, body) => apply("routine.updated", await api.updateRoutine(id, body)),
      remove: async (id) => {
        await api.deleteRoutine(id);
        apply("routine.deleted", { id });
      },
      runNow: async (id) => {
        await api.runRoutine(id);
      },
    };
  }, []);

  const linkActions = useMemo<LinkActions>(() => {
    const apply = (kind: Event["kind"], payload: Link | { id: LinkId }) =>
      setWorkspace((current) =>
        applyEvent(current, { seq: 0, room_id: null, kind, payload, created_at: 0 }),
      );
    return {
      linkRepo: async (roomId, repo) =>
        apply("link.created", await api.createLink({ room_id: roomId, repo })),
      remove: async (id) => {
        await api.deleteLink(id);
        apply("link.deleted", { id });
      },
      pair: (roomId) => api.createPairing(roomId),
    };
  }, []);

  const loadOlder = useCallback(async (roomId: RoomId) => {
    const oldest = chatRef.current.rooms[roomId]?.messages[0];
    if (!oldest) return;
    const myReset = resetGen.current;
    const myGen = roomLoadGen.current[roomId] ?? 0;
    const page = await api.listMessages(roomId, oldest.seq);
    if (resetGen.current !== myReset || (roomLoadGen.current[roomId] ?? 0) !== myGen) return;
    setChat((current) => mergePage(current, roomId, page, true));
  }, []);

  const sendMessage = useCallback(async (roomId: RoomId, content: string) => {
    const message = await api.postMessage(roomId, content);
    // 事件流也会送来同一条；先放进去，发送的人不用等。
    setChat((current) => upsertMessage(current, message));
  }, []);

  // 接口返回的就是最新的整个队列，直接放进去，不用等事件流。
  const queueMessage = useCallback(async (roomId: RoomId, content: string) => {
    const queue = await api.queueMessage(roomId, content);
    queueVersion.current[roomId] = (queueVersion.current[roomId] ?? 0) + 1;
    setChat((current) => setQueue(current, queue));
  }, []);

  const editQueued = useCallback(async (id: QueuedId, edit: EditQueuedRequest) => {
    const queue = await api.editQueued(id, edit);
    setChat((current) => setQueue(current, queue));
  }, []);

  const cancelQueued = useCallback(async (id: QueuedId) => {
    const queue = await api.cancelQueued(id);
    setChat((current) => setQueue(current, queue));
  }, []);

  const stopReply = useCallback(async (roomId: RoomId) => {
    await api.stopReply(roomId);
  }, []);

  const stopBot = useCallback(async (roomId: RoomId, botId: BotId) => {
    await api.stopBot(roomId, botId);
  }, []);

  const decideApproval = useCallback(async (id: ApprovalId, decision: ApprovalDecision) => {
    // 结果（步骤变成进行中或没有批准）由事件流送来，这里不用改状态。
    await api.decideApproval(id, decision);
  }, []);

  const dismissNotice = useCallback(async (id: NoticeId) => {
    const notice = await api.dismissNotice(id);
    setWorkspace((current) =>
      applyEvent(current, {
        seq: 0,
        room_id: notice.room_id,
        kind: "notice.updated",
        payload: notice,
        created_at: 0,
      }),
    );
  }, []);

  return {
    ...workspace,
    phase,
    connected,
    chat,
    board,
    tasks,
    routineActions,
    linkActions,
    epoch,
    createRoom,
    team,
    openRoom,
    loadOlder,
    sendMessage,
    queueMessage,
    editQueued,
    cancelQueued,
    stopReply,
    stopBot,
    decideApproval,
    dismissNotice,
  };
}
