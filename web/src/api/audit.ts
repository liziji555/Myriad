import type { AuditEntry, RoomId, TaskId } from "./types";

/** 审计页上按动作分的几类。`prefix` 是动作名的开头，空的就是全部。 */
export const AUDIT_KINDS = [
  { id: "all", label: "全部", prefix: "" },
  { id: "approval", label: "审批", prefix: "approval." },
  { id: "tool", label: "Bot 动手", prefix: "tool." },
  { id: "task", label: "任务", prefix: "task." },
  { id: "notice", label: "提醒", prefix: "notice." },
] as const;

export type AuditKind = (typeof AUDIT_KINDS)[number]["id"];

export interface AuditQuery {
  room?: RoomId;
  task?: TaskId;
  kind: AuditKind;
  /** 只要比这一条更早的（翻页）。 */
  before?: number;
  limit?: number;
}

/** 拼出 `/api/audit` 后面的查询串。 */
export function auditSearch(query: AuditQuery): string {
  const params = new URLSearchParams();
  if (query.room) params.set("room", query.room);
  if (query.task) params.set("task", query.task);
  const prefix = AUDIT_KINDS.find((kind) => kind.id === query.kind)?.prefix ?? "";
  if (prefix) params.set("action", prefix);
  if (query.before !== undefined) params.set("before", String(query.before));
  if (query.limit !== undefined) params.set("limit", String(query.limit));
  const search = params.toString();
  return search ? `?${search}` : "";
}

/** 这件事是谁做的。 */
export function auditWho(entry: Pick<AuditEntry, "actor" | "actor_name">): string {
  switch (entry.actor) {
    case "user":
      return "你";
    case "system":
      return "程序";
    case "bot":
      return entry.actor_name || "Bot";
    case "outside":
      return entry.actor_name || "外面";
  }
}

/** 什么时候的事：今天的只写几点几分几秒，别的日子带上月和日。 */
export function auditTime(ms: number, now: Date = new Date()): string {
  const date = new Date(ms);
  const sameDay = date.toDateString() === now.toDateString();
  return date.toLocaleString("zh-CN", {
    ...(sameDay ? {} : { month: "numeric", day: "numeric" }),
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });
}

/** 接上更早的一页。同一条不放两次（刷新和翻页撞在一起时）。 */
export function appendAudit(have: readonly AuditEntry[], older: readonly AuditEntry[]): AuditEntry[] {
  const seen = new Set(have.map((entry) => entry.id));
  return [...have, ...older.filter((entry) => !seen.has(entry.id))];
}
