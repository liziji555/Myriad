import { describe, expect, it } from "vitest";
import { mcpDot, mcpStatus, skillAudience } from "./extensions";

describe("extensions", () => {
  it("says how an MCP server is doing", () => {
    const ready = { name: "files", state: "ready" as const, tools: ["read_file", "list"] };
    expect(mcpStatus(ready)).toBe("已连上 · 2 个工具");
    expect(mcpDot(ready)).toBe("idle");
    const failed = { name: "x", state: "failed" as const, tools: [], error: "找不到命令" };
    expect(mcpStatus(failed)).toBe("没连上");
    expect(mcpDot(failed)).toBe("off");
    const connecting = { name: "y", state: "connecting" as const, tools: [] };
    expect(mcpStatus(connecting)).toBe("正在连接");
    expect(mcpDot(connecting)).toBe("warn");
  });

  it("says who may use a skill", () => {
    expect(skillAudience({ name: "a", description: "", bots: [] })).toBe("所有 Bot");
    expect(skillAudience({ name: "a", description: "", bots: ["Coder", "助手"] })).toBe("Coder、助手");
  });
});
