import { describe, expect, it } from "vitest";
import { approvalChoices } from "./approvals";

const decisions = (can_allow_task: boolean, can_allow_always: boolean) =>
  approvalChoices({
    id: "apr_1",
    reason: "要执行命令：cargo test",
    expires_at: 0,
    can_allow_task,
    can_allow_always,
  }).map((choice) => choice.decision);

describe("approvalChoices", () => {
  it("offers allowing every command only when the server says the card can", () => {
    expect(decisions(true, true)).toEqual(["once", "always", "task", "deny"]);
    expect(decisions(false, true)).toEqual(["once", "always", "deny"]);
  });

  it("offers always allowing only when the server can remember it", () => {
    expect(decisions(false, false)).toEqual(["once", "deny"]);
  });
});
