import { describe, expect, it } from "vitest";
import { approvalChoices } from "./approvals";

const card = (can_allow_task: boolean) => ({
  id: "apr_1",
  reason: "要执行命令：cargo test",
  expires_at: 0,
  can_allow_task,
});

const decisions = (tool: string, can_allow_task: boolean) =>
  approvalChoices(tool, card(can_allow_task)).map((choice) => choice.decision);

describe("approvalChoices", () => {
  it("offers allowing every command only when the server says the card can", () => {
    expect(decisions("shell", true)).toEqual(["once", "always", "task", "deny"]);
    expect(decisions("shell", false)).toEqual(["once", "always", "deny"]);
  });

  it("only commands and file writes can be remembered", () => {
    expect(decisions("write", false)).toEqual(["once", "always", "deny"]);
    expect(decisions("git_push", false)).toEqual(["once", "deny"]);
    expect(decisions("mcp__fs__read", false)).toEqual(["once", "deny"]);
  });
});
