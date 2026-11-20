import { describe, expect, it } from "vitest";
import { appendAudit, auditSearch, auditTime, auditWho } from "./audit";
import type { AuditEntry } from "./types";

function entry(id: number, patch: Partial<AuditEntry> = {}): AuditEntry {
  return {
    id,
    at: 0,
    actor: "user",
    actor_name: "",
    action: "bot.created",
    room_id: null,
    task_id: null,
    summary: "",
    detail: null,
    ...patch,
  };
}

describe("audit", () => {
  it("builds the query for the chosen scope and kind", () => {
    expect(auditSearch({ kind: "all" })).toBe("");
    expect(auditSearch({ kind: "approval", room: "room_1" })).toBe("?room=room_1&action=approval.");
    expect(auditSearch({ kind: "tool", task: "task_1", before: 40, limit: 30 })).toBe(
      "?task=task_1&action=tool.&before=40&limit=30",
    );
    // 名字里有特殊字符也不会串到别的参数上。
    expect(auditSearch({ kind: "all", room: "a&b=c" })).toBe("?room=a%26b%3Dc");
  });

  it("says who did it", () => {
    expect(auditWho(entry(1))).toBe("你");
    expect(auditWho(entry(1, { actor: "system" }))).toBe("程序");
    expect(auditWho(entry(1, { actor: "bot", actor_name: "Coder" }))).toBe("Coder");
    expect(auditWho(entry(1, { actor: "bot" }))).toBe("Bot");
    expect(auditWho(entry(1, { actor: "outside", actor_name: "Telegram" }))).toBe("Telegram");
    expect(auditWho(entry(1, { actor: "outside" }))).toBe("外面");
  });

  it("shows the date only when it is not today", () => {
    const now = new Date(2026, 10, 20, 18, 0, 0);
    const today = new Date(2026, 10, 20, 9, 5, 7).getTime();
    const earlier = new Date(2026, 10, 3, 9, 5, 7).getTime();
    expect(auditTime(today, now)).toBe("09:05:07");
    expect(auditTime(earlier, now)).toBe("11/3 09:05:07");
  });

  it("appends an older page without repeating entries", () => {
    const merged = appendAudit([entry(5), entry(4)], [entry(4), entry(3)]);
    expect(merged.map((item) => item.id)).toEqual([5, 4, 3]);
  });
});
