import { describe, expect, it } from "vitest";
import { outputPreview, PREVIEW_CHARS, stepIcon, stepKinds } from "./steps";

describe("outputPreview", () => {
  it("短的原样给出，没有「查看完整」", () => {
    expect(outputPreview("一行\n两行")).toEqual({ text: "一行\n两行", cut: false });
  });

  it("行数多的只露出开头几行", () => {
    const preview = outputPreview("1\n2\n3\n4\n5\n6");
    expect(preview).toEqual({ text: "1\n2\n3\n4", cut: true });
  });

  it("一行特别长的按字数截", () => {
    const preview = outputPreview("字".repeat(PREVIEW_CHARS + 10));
    expect(preview.text.length).toBe(PREVIEW_CHARS);
    expect(preview.cut).toBe(true);
  });
});

describe("stepIcon", () => {
  it("自带的工具各有各的图标", () => {
    expect(stepIcon("read")).toBe("read");
    expect(stepIcon("list_dir")).toBe("read");
    expect(stepIcon("glob")).toBe("search");
    expect(stepIcon("grep")).toBe("search");
    expect(stepIcon("write")).toBe("edit");
    expect(stepIcon("edit")).toBe("edit");
    expect(stepIcon("bash")).toBe("command");
    expect(stepIcon("shell")).toBe("command");
    expect(stepIcon("web_fetch")).toBe("web");
    expect(stepIcon("git_push")).toBe("git");
    expect(stepIcon("open_pr")).toBe("git");
  });
  it("MCP 的工具不管叫什么都算外接的", () => {
    expect(stepIcon("mcp__files__read_file")).toBe("plug");
    expect(stepIcon("mcp__demo__git_push")).toBe("plug");
  });
  it("不认识的给通用图标", () => {
    expect(stepIcon("")).toBe("tool");
    expect(stepIcon("zzz")).toBe("tool");
  });
});

describe("stepKinds", () => {
  it("同一类只留一个，按先后排", () => {
    expect(stepKinds(["read", "grep", "list_dir", "shell", "read"])).toEqual(["read", "search", "command"]);
  });
  it("没有步骤时是空的", () => {
    expect(stepKinds([])).toEqual([]);
  });
});
