import { describe, expect, it } from "vitest";
import { outputPreview, PREVIEW_CHARS } from "./steps";

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
