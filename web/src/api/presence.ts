import type { RoomChat } from "./chat";
import type { Bot, BotId, Message, MessageId, RoomActivity, Task, TaskId } from "./types";

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
function fromMessage(message: Message, task: Task | undefined): Pick<Presence, "state" | "text"> {
  const where = task ? `任务「${task.title}」：` : "";
  const waiting = message.steps.find((step) => step.state === "waiting");
  if (waiting) return { state: "approval", text: `${where}等你批准 ${waiting.title}` };
  const running = [...message.steps].reverse().find((step) => step.state === "running");
  if (running) return { state: "tool", text: `${where}${running.title}` };
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
      return { ...base, state: mine.state, text: mine.text, messageId: mine.message.id, taskId: mine.task?.id };
    }
    const asking = tasks.find((task) => task.owner_bot_id === bot.id && ASKING[task.state]);
    if (asking) {
      return { ...base, state: "asking", text: `任务「${asking.title}」${ASKING[asking.state]}`, taskId: asking.id };
    }
    if (mine) {
      return { ...base, state: mine.state, text: mine.text, messageId: mine.message.id, taskId: mine.task?.id };
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
