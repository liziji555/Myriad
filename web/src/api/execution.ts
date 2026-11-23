import type { RoomChat } from "./chat";
import type { Presence, PresenceState } from "./presence";
import type { Bot, Message, ModelTier, Task, ToolStep } from "./types";

/** 执行面板上一个 Bot 的卡片：现场栏那一句之外，再加它这一次做到哪了。 */
export interface ExecCard extends Presence {
  tier: ModelTier;
  /** 这一次回复从什么时候开始（毫秒）。手上没有在写的回复就没有。 */
  since?: number;
  /** 说的是哪个任务。 */
  taskTitle?: string;
  /** 这一次回复里调过的工具，按先后。 */
  steps: readonly ToolStep[];
  /** 已经做完的几步（不管成没成）。 */
  done: number;
  /** 其中没成的几步：失败、被拒。 */
  failed: number;
  /** 上一次说完话是什么时候（毫秒）。空着的 Bot 才看这个。 */
  lastAt?: number;
}

export const EXEC_LABEL: Record<PresenceState, string> = {
  approval: "等你批准",
  asking: "等你处理",
  tool: "在执行",
  writing: "在写回复",
  working: "在做任务",
  queued: "排队中",
  idle: "空闲",
  off: "已停用",
};

const FINISHED: readonly ToolStep["state"][] = ["ok", "error", "cancelled", "denied"];

const byBot = (message: Message, bot: string) =>
  message.sender.kind === "bot" && message.sender.bot_id === bot;

/**
 * 房间里每个 Bot 一张执行卡片，顺序和现场栏一样（要人看的在前）。
 *
 * 用的是现场栏已经算好的状态，再从那个 Bot 还在写的消息里数步骤。
 */
export function executionOf(
  presence: readonly Presence[],
  members: readonly Bot[],
  chat: RoomChat | undefined,
  tasks: readonly Task[],
): ExecCard[] {
  const messages = chat?.messages ?? [];
  return presence.map((row): ExecCard => {
    // 卡片说的是哪个任务，就只数那个任务里的步骤，别把另一个任务的串进来。
    const live =
      messages.find((message) => message.id === row.messageId && message.state === "streaming") ??
      [...messages]
        .reverse()
        .find(
          (message) =>
            message.state === "streaming" &&
            byBot(message, row.botId) &&
            (row.taskId === undefined || message.task_id === row.taskId),
        );
    // 做任务一律用强档模型（见 sa-orchestrator 的 work.rs）。
    const tier =
      live?.kind === "run" ? "strong" : (members.find((bot) => bot.id === row.botId)?.tier ?? "mid");
    const steps = live?.steps ?? [];
    const last = live
      ? undefined
      : [...messages].reverse().find((message) => message.state !== "streaming" && byBot(message, row.botId));
    const taskId = row.taskId ?? live?.task_id;
    return {
      ...row,
      messageId: row.messageId ?? live?.id,
      taskId,
      tier,
      since: live?.created_at,
      taskTitle: tasks.find((task) => task.id === taskId)?.title,
      steps,
      done: steps.filter((step) => FINISHED.includes(step.state)).length,
      failed: steps.filter((step) => step.state === "error" || step.state === "denied").length,
      lastAt: last?.updated_at,
    };
  });
}

/** 用了多久：「8 秒」「3 分 05 秒」「1 小时 02 分」。 */
export function elapsed(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  const pad = (n: number) => String(n).padStart(2, "0");
  if (h > 0) return `${h} 小时 ${pad(m)} 分`;
  if (m > 0) return `${m} 分 ${pad(s)} 秒`;
  return `${s} 秒`;
}

/** 多久以前：「刚刚」「5 分钟前」「2 小时前」「3 天前」。 */
export function ago(ms: number): string {
  const minutes = Math.floor(Math.max(0, ms) / 60_000);
  if (minutes < 1) return "刚刚";
  if (minutes < 60) return `${minutes} 分钟前`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} 小时前`;
  return `${Math.floor(hours / 24)} 天前`;
}
