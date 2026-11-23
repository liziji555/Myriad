import { describe, expect, it } from "vitest";
import type { Flow, FlowNode } from "../api/flow";
import { glyphOf, relatedTo } from "./WorkTree";

const node = (over: Partial<FlowNode>): FlowNode =>
  ({
    id: "n",
    lane: "a",
    row: 0,
    kind: "step",
    tone: "ok",
    label: "",
    messageId: "m",
    run: "r",
    ...over,
  }) as FlowNode;

describe("工作树", () => {
  it("节点形状按 Lancer 的事件分类", () => {
    expect(glyphOf(node({ kind: "handoff" }))).toBe("handoff");
    expect(glyphOf(node({ kind: "inbox" }))).toBe("xref");
    expect(glyphOf(node({ kind: "task" }))).toBe("fork");
    expect(glyphOf(node({ kind: "step", tone: "error" }))).toBe("error");
    expect(glyphOf(node({ kind: "step", tone: "waiting" }))).toBe("wait");
    expect(glyphOf(node({ kind: "start" }))).toBe("checkpoint");
  });

  it("点中节点时只留下和它连着的节点", () => {
    const flow = {
      lanes: [],
      nodes: [],
      rows: 0,
      rounds: [],
      edges: [
        { id: "e1", from: "a", to: "b", kind: "dispatch", live: false, at: 0 },
        { id: "e2", from: "c", to: "d", kind: "peer", live: false, at: 0 },
      ],
    } as unknown as Flow;
    expect(relatedTo(flow, null)).toBeNull();
    expect([...(relatedTo(flow, "b") ?? [])].sort()).toEqual(["a", "b"]);
  });
});
