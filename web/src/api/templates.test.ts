import { describe, expect, it } from "vitest";
import { importSummary } from "./templates";
import type { Bot, ImportReport, Room } from "./types";

function bot(name: string): Bot {
  return { name } as Bot;
}

function report(patch: Partial<ImportReport>): ImportReport {
  return { created: [], skipped: [], room: null, ...patch };
}

describe("importSummary", () => {
  it("说清楚新建了谁、跳过了谁、有没有新房间", () => {
    const all = report({
      created: [bot("Coder"), bot("Tester")],
      skipped: ["Reviewer"],
      room: { name: "修 bug" } as Room,
    });
    expect(importSummary(all)).toBe(
      "新建了 Coder、Tester；已经有 Reviewer，没有动；新建了房间「修 bug」。",
    );
  });

  it("只有一部分时不留空的分句", () => {
    expect(importSummary(report({ skipped: ["A", "B"] }))).toBe("已经有 A、B，没有动。");
    expect(importSummary(report({ created: [bot("A")] }))).toBe("新建了 A。");
  });

  it("什么都没建时照实说", () => {
    expect(importSummary(report({}))).toBe("模板是空的，没有新建任何东西。");
  });
});
