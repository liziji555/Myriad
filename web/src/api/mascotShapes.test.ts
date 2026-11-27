import { describe, expect, it } from "vitest";
import {
  MASCOT_COLORS,
  MASCOT_SHAPES,
  mascotStateFromPresence,
  mascotTimingFor,
  resolveMascotColor,
  resolveMascotShape,
} from "./mascotShapes";
import { globalBotSignalsOf } from "./presence";
import type { Bot } from "./types";

function fakeBot(overrides: Partial<Bot> = {}): Bot {
  return {
    id: "bot-1",
    name: "助手",
    kind: "assistant",
    tier: "mid",
    persona: "",
    enabled: true,
    created_at: 1,
    harness: "builtin",
    harness_command: "",
    is_master: false,
    role: "",
    avatar_shape: "",
    avatar_color: "",
    avatar_image: "",
    ...overrides,
  };
}

describe("mascotShapes", () => {
  it("resolves master defaults to orb + obsidian and sub-bots deterministically", () => {
    const masterShape = resolveMascotShape("b-master", "", true);
    const masterColor = resolveMascotColor("b-master", "", true);
    expect(masterShape.id).toBe("orb");
    expect(masterColor.id).toBe("obsidian");

    const subColor1 = resolveMascotColor("b-sub-1", "", false);
    const subColor2 = resolveMascotColor("b-sub-1", "", false);
    expect(subColor1.id).toBe(subColor2.id);
    expect(subColor1.id).not.toBe("obsidian");

    expect(resolveMascotShape("b-sub-1", "cat", false).id).toBe("cat");
    expect(resolveMascotColor("b-sub-1", "aurora", false).id).toBe("aurora");
    expect(MASCOT_SHAPES.length).toBe(12);
    expect(MASCOT_COLORS.length).toBe(12);
  });

  it("maps tool names and presence states into granular mascot animations", () => {
    expect(mascotStateFromPresence("off")).toBe("sleeping");
    expect(mascotStateFromPresence("approval")).toBe("waiting");
    expect(mascotStateFromPresence("writing")).toBe("thinking");
    expect(mascotStateFromPresence("tool", "grep")).toBe("searching");
    expect(mascotStateFromPresence("tool", "ask_bot")).toBe("sending");
    expect(mascotStateFromPresence("tool", "shell")).toBe("working");
    expect(mascotStateFromPresence("idle")).toBe("idle");

    const timing = mascotTimingFor("bot-xyz");
    expect(timing.delaySec).toBeLessThanOrEqual(0);
    expect(timing.speed).toBeGreaterThanOrEqual(0.9);
  });

  it("aggregates cross-room global bot signals including unread done and sleeping states", () => {
    const master = fakeBot({ id: "m1", name: "主控", is_master: true });
    const worker = fakeBot({ id: "w1", name: "前端" });
    const sleeper = fakeBot({ id: "s1", name: "暂停", enabled: false });

    const signals = globalBotSignalsOf(
      [master, worker, sleeper],
      {
        "room-1": {
          messages: [
            {
              id: "msg-1",
              seq: 1,
              room_id: "room-1",
              sender: { kind: "bot", bot_id: "w1" },
              kind: "chat",
              content: "",
              state: "streaming",
              steps: [
                {
                  id: "c1",
                  tool: "grep",
                  title: "搜索代码",
                  state: "running",
                },
              ],
              mentions: [],
              reply_to: [],
              error: null,
              rev: 1,
              hop: 0,
              created_at: 10,
              updated_at: 10,
            },
          ],
          hasMore: false,
          loaded: true,
          queue: [],
        },
      },
      {},
      { m1: "done" },
    );

    expect(signals.m1?.mascotState).toBe("done");
    expect(signals.m1?.stopTarget).toBeUndefined();
    expect(signals.w1?.mascotState).toBe("searching");
    expect(signals.w1?.activeTool).toBe("grep");
    expect(signals.w1?.stopTarget).toEqual({ kind: "chat", roomId: "room-1" });
    expect(signals.s1?.mascotState).toBe("sleeping");
    expect(signals.s1?.stopTarget).toBeUndefined();
  });
});
