import type { RoomChat } from "./chat";
import { mascotStateFromPresence, type MascotState } from "./mascotShapes";
import type { Bot, BotId, Message, MessageId, RoomActivity, RoomId, Task, TaskId } from "./types";

/** 房间里正在说话的那几个 Bot。旧版服务只报 `speaking` 一个。 */
export function speakersOf(activity: RoomActivity | null | undefined): BotId[] {
  if (!activity) return [];
  if (activity.speakers?.length) return activity.speakers;
  return activity.speaking ? [activity.speaking] : [];
}

/** 一个 Bot 此刻在做什么。越靠前越需要人看一眼。 */
export type PresenceState = "approval" | "tool" | "writing" | "working" | "asking" | "queued" | "idle" | "off";

export interface Presence {
  botId: BotId;
  name: string;
  state: PresenceState;
  /** 一句话：在做什么。 */
  text: string;
  /** 当前正在执行的工具名（用于驱动动态角色细分动作：检索/派活/编码）。 */
  activeTool?: string;
  /** 点一下跳到主线上的哪条消息。没有可跳的就没有。 */
  messageId?: MessageId;
  /** 说的是哪个任务。 */
  taskId?: TaskId;
}

/** 等人处理的任务状态，和它们在现场栏里的说法。 */
const ASKING: Partial<Record<Task["state"], string>> = {
  review: "等你验收",
  waiting: "等你回答",
  blocked: "卡住了",
};

const ORDER: readonly PresenceState[] = [
  "approval",
  "asking",
  "tool",
  "writing",
  "working",
  "queued",
  "idle",
  "off",
];

/** 一条还在写的消息说明它的 Bot 在做什么：等批准、在做某一步、还是在写字。 */
function fromMessage(
  message: Message,
  task: Task | undefined,
): Pick<Presence, "state" | "text" | "activeTool"> {
  const where = task ? `任务「${task.title}」：` : "";
  const waiting = message.steps.find((step) => step.state === "waiting");
  if (waiting) return { state: "approval", text: `${where}等你批准 ${waiting.title}`, activeTool: waiting.tool };
  const running = [...message.steps].reverse().find((step) => step.state === "running");
  if (running) return { state: "tool", text: `${where}${running.title}`, activeTool: running.tool };
  return { state: "writing", text: task ? `正在做任务「${task.title}」` : "正在写回复" };
}

/**
 * 房间里每个 Bot 此刻在做什么，需要人处理的排在前面。
 *
 * 只看手上有的东西：还在写的消息、房间的排队情况、任务的状态。
 */
export function presenceOf(
  members: readonly Bot[],
  chat: RoomChat | undefined,
  tasks: readonly Task[],
): Presence[] {
  const streaming = (chat?.messages ?? []).filter(
    (message) => message.state === "streaming" && message.sender.kind === "bot",
  );
  const queued = chat?.activity?.queued ?? [];
  const rows = members.map((bot): Presence => {
    const base = { botId: bot.id, name: bot.name };
    if (!bot.enabled) return { ...base, state: "off", text: "已停用" };
    // 一个 Bot 可能同时在对话里写、又在任务里做：挑最需要人看的那条。
    const mine = streaming
      .filter((message) => message.sender.kind === "bot" && message.sender.bot_id === bot.id)
      .map((message) => {
        const task = tasks.find((t) => t.id === message.task_id);
        return { message, task, ...fromMessage(message, message.kind === "run" ? task : undefined) };
      })
      .sort((a, b) => ORDER.indexOf(a.state) - ORDER.indexOf(b.state))[0];
    if (mine?.state === "approval") {
      return {
        ...base,
        state: mine.state,
        text: mine.text,
        activeTool: mine.activeTool,
        messageId: mine.message.id,
        taskId: mine.task?.id,
      };
    }
    const asking = tasks.find((task) => task.owner_bot_id === bot.id && ASKING[task.state]);
    if (asking) {
      return { ...base, state: "asking", text: `任务「${asking.title}」${ASKING[asking.state]}`, taskId: asking.id };
    }
    if (mine) {
      return {
        ...base,
        state: mine.state,
        text: mine.text,
        activeTool: mine.activeTool,
        messageId: mine.message.id,
        taskId: mine.task?.id,
      };
    }
    const working = tasks.find((task) => task.owner_bot_id === bot.id && task.state === "working");
    if (working) {
      return { ...base, state: "working", text: `正在做任务「${working.title}」`, taskId: working.id };
    }
    if (queued.includes(bot.id)) return { ...base, state: "queued", text: "排着队，等前面的说完" };
    return { ...base, state: "idle", text: "空闲" };
  });
  // 稳定排序：同一种状态里保持进房间的先后。
  return rows
    .map((row, index) => ({ row, index }))
    .sort((a, b) => ORDER.indexOf(a.row.state) - ORDER.indexOf(b.row.state) || a.index - b.index)
    .map(({ row }) => row);
}

/** 现场栏最前面的一句总结：有没有事等着人。 */
export function presenceSummary(rows: readonly Presence[]): string {
  const need = rows.filter((row) => row.state === "approval" || row.state === "asking").length;
  if (need > 0) return `${need} 件事等你`;
  const busy = rows.filter((row) => ["tool", "writing", "working", "queued"].includes(row.state)).length;
  return busy > 0 ? `${busy} 个 Bot 在忙` : "都空着";
}

/** 停止一个活跃 Bot 工作所需的精确目标（跨房间对话或后台任务）。 */
export type BotStopTarget =
  | { kind: "chat"; roomId: RoomId }
  | { kind: "task"; taskId: TaskId };

/** 跨全部房间聚合出的单个 Bot 实时生命态（供左侧栏、中枢在岗阵列与智能排序共用）。 */
export interface GlobalBotSignal {
  botId: BotId;
  presence: PresenceState;
  mascotState: MascotState;
  text: string;
  activeTool?: string;
  unread?: "done" | "error";
  roomId?: RoomId;
  /** 仅当该 Bot 存在正在运行、可被中断的回复或任务时提供（独立于视觉最高优先级状态）。 */
  stopTarget?: BotStopTarget;
  /** 当停止动作指向的具体活动（如“停止回复”/“停止任务”）需要明确说明时的按钮文案。 */
  stopLabel?: string;
}

const SIGNAL_RANK: Record<MascotState, number> = {
  waiting: 0,
  error: 1,
  working: 2,
  searching: 2,
  sending: 2,
  thinking: 3,
  done: 4,
  idle: 5,
  sleeping: 6,
};

/** 扫描所有已加载房间与任务，加上未读完成/报错标记，产出每个 Bot 的全局最高优先级状态。 */
export function globalBotSignalsOf(
  bots: readonly Bot[],
  chatRooms: Readonly<Record<RoomId, RoomChat | undefined>>,
  taskRooms: Readonly<Record<RoomId, readonly Task[] | undefined>>,
  unreadMarks: Readonly<Record<BotId, "done" | "error" | undefined>>,
): Record<BotId, GlobalBotSignal> {
  const out: Record<BotId, GlobalBotSignal> = {};
  for (const bot of bots) {
    if (!bot.enabled) {
      out[bot.id] = {
        botId: bot.id,
        presence: "off",
        mascotState: "sleeping",
        text: "已停用 · 休眠中",
      };
      continue;
    }
    let best: GlobalBotSignal = {
      botId: bot.id,
      presence: "idle",
      mascotState: "idle",
      text: "空闲",
    };
    let bestStop: { rank: number; target: BotStopTarget; label: string } | undefined;
    const consider = (candidate: GlobalBotSignal) => {
      const rank = SIGNAL_RANK[candidate.mascotState];
      if (rank < SIGNAL_RANK[best.mascotState]) {
        best = candidate;
      }
      if (candidate.stopTarget) {
        const label =
          candidate.stopLabel ?? (candidate.stopTarget.kind === "task" ? "停止任务" : "停止回复");
        if (!bestStop || rank < bestStop.rank) {
          bestStop = { rank, target: candidate.stopTarget, label };
        }
      }
    };

    for (const [roomId, chat] of Object.entries(chatRooms)) {
      if (!chat) continue;
      const tasks = taskRooms[roomId] ?? [];
      for (const msg of chat.messages) {
        if (msg.state !== "streaming" || msg.sender.kind !== "bot" || msg.sender.bot_id !== bot.id) {
          continue;
        }
        const task = tasks.find((t) => t.id === msg.task_id);
        const info = fromMessage(msg, msg.kind === "run" ? task : undefined);
        const stopTarget: BotStopTarget =
          msg.kind === "run" && msg.task_id
            ? { kind: "task", taskId: msg.task_id }
            : { kind: "chat", roomId };
        consider({
          botId: bot.id,
          presence: info.state,
          mascotState: mascotStateFromPresence(info.state, info.activeTool),
          text: info.text,
          activeTool: info.activeTool,
          roomId,
          stopTarget,
        });
      }
      if (speakersOf(chat.activity).includes(bot.id)) {
        consider({
          botId: bot.id,
          presence: "writing",
          mascotState: "thinking",
          text: "正在回复",
          roomId,
          stopTarget: { kind: "chat", roomId },
        });
      }
      if (chat.activity?.queued.includes(bot.id)) {
        consider({
          botId: bot.id,
          presence: "queued",
          mascotState: "thinking",
          text: "排队中",
          roomId,
          stopTarget: { kind: "chat", roomId },
        });
      }
    }

    for (const [roomId, tasks] of Object.entries(taskRooms)) {
      if (!tasks) continue;
      for (const task of tasks) {
        if (task.owner_bot_id !== bot.id) continue;
        if (ASKING[task.state]) {
          consider({
            botId: bot.id,
            presence: "asking",
            mascotState: "waiting",
            text: `任务「${task.title}」${ASKING[task.state]}`,
            roomId,
          });
        } else if (task.state === "working") {
          consider({
            botId: bot.id,
            presence: "working",
            mascotState: "working",
            text: `做任务「${task.title}」`,
            roomId,
            stopTarget: { kind: "task", taskId: task.id },
          });
        }
      }
    }

    const mark = unreadMarks[bot.id];
    if (mark === "error") {
      consider({
        botId: bot.id,
        presence: "idle",
        mascotState: "error",
        text: "上一轮出错",
        unread: "error",
      });
    } else if (mark === "done") {
      consider({
        botId: bot.id,
        presence: "idle",
        mascotState: "done",
        text: "刚回复（未读）",
        unread: "done",
      });
    }

    if (bestStop) {
      best = {
        ...best,
        stopTarget: bestStop.target,
        stopLabel: bestStop.label,
      };
    }

    out[bot.id] = best;
  }
  return out;
}
