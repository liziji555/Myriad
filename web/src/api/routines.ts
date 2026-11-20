import type { Message, Routine, RoutineResult } from "./types";

const pad = (n: number) => String(n).padStart(2, "0");
const WEEKDAYS = ["周日", "周一", "周二", "周三", "周四", "周五", "周六", "周日"];
const isNumber = (text: string) => /^\d+$/.test(text);

/** 常用的时间表，新建时点一下就填上。 */
export const SCHEDULE_PRESETS: readonly { label: string; schedule: string }[] = [
  { label: "每天 9:00", schedule: "0 9 * * *" },
  { label: "工作日 9:00", schedule: "0 9 * * 1-5" },
  { label: "每周一 9:00", schedule: "0 9 * * 1" },
  { label: "每小时", schedule: "0 * * * *" },
  { label: "每 30 分钟", schedule: "*/30 * * * *" },
];

/**
 * 把五段式 cron（分 时 日 月 周）说成人话。只认常见的几种写法，认不出来就原样返回。
 * 写得对不对由服务端判断，这里只管显示。
 */
export function describeSchedule(schedule: string): string {
  const parts = schedule.trim().split(/\s+/);
  if (parts.length !== 5) return schedule;
  const [minute, hour, day, month, weekday] = parts as [string, string, string, string, string];
  if (month !== "*") return schedule;
  const anyDay = day === "*" && weekday === "*";

  const every = /^\*\/(\d+)$/.exec(minute);
  if (every && hour === "*" && anyDay) return `每 ${every[1]} 分钟`;
  if (!isNumber(minute)) return schedule;
  if (hour === "*" && anyDay) return Number(minute) === 0 ? "每小时整点" : `每小时的第 ${Number(minute)} 分`;
  const hours = /^\*\/(\d+)$/.exec(hour);
  if (hours && anyDay && Number(minute) === 0) return `每 ${hours[1]} 小时`;
  if (!isNumber(hour)) return schedule;

  const time = `${Number(hour)}:${pad(Number(minute))}`;
  if (anyDay) return `每天 ${time}`;
  if (day === "*") {
    if (weekday === "1-5") return `工作日 ${time}`;
    if (weekday === "6,0" || weekday === "0,6" || weekday === "6,7") return `周末 ${time}`;
    if (isNumber(weekday) && Number(weekday) <= 7) return `每${WEEKDAYS[Number(weekday)]} ${time}`;
    return schedule;
  }
  if (weekday === "*" && isNumber(day)) return `每月 ${Number(day)} 日 ${time}`;
  return schedule;
}

/** 时间点：今天的只写几点几分，别的日子带上月和日。 */
export function formatWhen(at: number, now: number = Date.now()): string {
  const date = new Date(at);
  const today = new Date(now);
  const time = `${pad(date.getHours())}:${pad(date.getMinutes())}`;
  const sameDay =
    date.getFullYear() === today.getFullYear() &&
    date.getMonth() === today.getMonth() &&
    date.getDate() === today.getDate();
  return sameDay ? `今天 ${time}` : `${date.getMonth() + 1} 月 ${date.getDate()} 日 ${time}`;
}

const RESULTS: Record<RoutineResult, string> = {
  spoke: "说了话",
  silent: "没什么可说",
  failed: "没做成",
  skipped: "跳过了",
};

/** 上一次做得怎么样，一句话。还没做过时是 `null`。 */
export function lastRun(routine: Routine, now: number = Date.now()): string | null {
  if (routine.last_at === null || routine.last_result === null) return null;
  const note = routine.last_note ? `：${routine.last_note}` : "";
  return `上一次 ${formatWhen(routine.last_at, now)} ${RESULTS[routine.last_result]}${note}`;
}

/** 定时任务到点时放进房间的那条消息，内容是「名字、空行、要做的事」。取出名字。 */
export function triggerTitle(content: string): string {
  const cut = content.indexOf("\n\n");
  return cut < 0 ? content : content.slice(0, cut);
}

/**
 * 哪些「定时任务到点了」要显示在时间线上：只有 Bot 真的说了话（或者正在说）的那些。
 * Bot 没什么可说时什么都不留，时间线上也就不出现这一行。
 */
export function shownTriggers(messages: readonly Message[]): Set<string> {
  const shown = new Set<string>();
  let pending: Message | null = null;
  for (const message of messages) {
    if (message.kind === "routine") {
      pending = message;
    } else if (message.sender.kind === "user") {
      pending = null;
    } else if (
      pending &&
      message.kind === "chat" &&
      message.sender.bot_id === pending.mentions[0]
    ) {
      shown.add(pending.id);
      pending = null;
    }
  }
  return shown;
}
