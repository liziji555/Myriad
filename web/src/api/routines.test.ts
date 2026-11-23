import { describe, expect, it } from "vitest";
import { describeSchedule, formatWhen, lastRun, shownTriggers, triggerTitle } from "./routines";
import type { Message, Routine } from "./types";

describe("describeSchedule", () => {
  it("says the common schedules in plain words", () => {
    expect(describeSchedule("0 9 * * *")).toBe("每天 9:00");
    expect(describeSchedule("30 18 * * 1-5")).toBe("工作日 18:30");
    expect(describeSchedule("0 9 * * 1")).toBe("每周一 9:00");
    expect(describeSchedule("0 9 * * 7")).toBe("每周日 9:00");
    expect(describeSchedule("5 8 1 * *")).toBe("每月 1 日 8:05");
    expect(describeSchedule("*/30 * * * *")).toBe("每 30 分钟");
    expect(describeSchedule("0 * * * *")).toBe("每小时整点");
    expect(describeSchedule("15 * * * *")).toBe("每小时的第 15 分");
    expect(describeSchedule("0 */2 * * *")).toBe("每 2 小时");
  });

  it("leaves what it does not recognise as written", () => {
    for (const schedule of ["0 9 1 1 *", "0 9,18 * * *", "0 9 1 * 1", "每天九点", "0 9 * *"]) {
      expect(describeSchedule(schedule)).toBe(schedule);
    }
  });
});

const at = (day: number, hour: number, minute: number) => new Date(2026, 4, day, hour, minute).getTime();

describe("formatWhen", () => {
  it("drops the date for today", () => {
    expect(formatWhen(at(20, 9, 5), at(20, 23, 0))).toBe("今天 09:05");
    expect(formatWhen(at(21, 9, 5), at(20, 23, 0))).toBe("5 月 21 日 09:05");
  });
});

const routine = (patch: Partial<Routine>): Routine => ({
  id: "rtn_1",
  room_id: "room_1",
  bot_id: "bot_1",
  title: "早报",
  prompt: "看看新消息",
  schedule: "0 9 * * *",
  enabled: true,
  next_at: null,
  last_at: null,
  last_result: null,
  last_note: "",
  failures: 0,
  created_at: 1,
  updated_at: 1,
  ...patch,
});

describe("lastRun", () => {
  it("is empty before the first run and explains a failure", () => {
    expect(lastRun(routine({}))).toBeNull();
    const now = at(20, 12, 0);
    expect(lastRun(routine({ last_at: at(20, 9, 0), last_result: "silent" }), now)).toBe(
      "上一次 今天 09:00 没什么可说",
    );
    expect(
      lastRun(routine({ last_at: at(19, 9, 0), last_result: "failed", last_note: "模型报错" }), now),
    ).toBe("上一次 5 月 19 日 09:00 没做成：模型报错");
  });
});

let seq = 0;
const message = (patch: Partial<Message>): Message => ({
  id: `msg_${++seq}`,
  seq,
  room_id: "room_1",
  sender: { kind: "user" },
  content: "",
  mentions: [],
  hop: 0,
  reply_to: [],
  kind: "chat",
  steps: [],
  state: "complete",
  error: null,
  rev: 0,
  created_at: 1,
  updated_at: 1,
  ...patch,
});
const trigger = (bot: string) => message({ kind: "routine", content: "早报\n\n看看新消息", mentions: [bot] });
const reply = (bot: string) => message({ sender: { kind: "bot", bot_id: bot } });

describe("shownTriggers", () => {
  it("shows a trigger only when its Bot spoke", () => {
    const quiet = trigger("bot_1");
    const loud = trigger("bot_1");
    const other = trigger("bot_2");
    const shown = shownTriggers([quiet, loud, reply("bot_1"), other, reply("bot_1")]);
    expect([...shown]).toEqual([loud.id]);
  });

  it("does not credit a reply to the human's next message to the trigger", () => {
    const quiet = trigger("bot_1");
    expect(shownTriggers([quiet, message({}), reply("bot_1")]).size).toBe(0);
  });
});

describe("triggerTitle", () => {
  it("takes the first paragraph", () => {
    expect(triggerTitle("早报\n\n看看新消息\n\n第二段")).toBe("早报");
    expect(triggerTitle("只有一段")).toBe("只有一段");
  });
});
