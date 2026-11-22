import { describe, expect, it } from "vitest";
import { AVATAR_HUES, hueOf, initialOf } from "./avatar";

describe("hueOf", () => {
  it("同一个 id 每次都是同一种颜色", () => {
    expect(hueOf("bot-1")).toBe(hueOf("bot-1"));
  });
  it("总在范围里，空 id 也不出错", () => {
    for (const id of ["", "a", "bot-助手", "0195f2c4-7b3a-7c11-9a55-2f0d6c1e8a01"]) {
      const hue = hueOf(id);
      expect(Number.isInteger(hue)).toBe(true);
      expect(hue).toBeGreaterThanOrEqual(0);
      expect(hue).toBeLessThan(AVATAR_HUES);
    }
  });
  it("不同的 id 不都落在同一种颜色上", () => {
    const hues = new Set(Array.from({ length: 40 }, (_, index) => hueOf(`bot-${index}`)));
    expect(hues.size).toBeGreaterThan(4);
  });
});

describe("initialOf", () => {
  it("取名字的第一个字", () => {
    expect(initialOf("助手")).toBe("助");
    expect(initialOf("  reviewer")).toBe("R");
  });
  it("不把一个表情拆成两半", () => {
    expect(initialOf("🦊狐狸")).toBe("🦊");
  });
  it("名字是空的给一个点", () => {
    expect(initialOf("   ")).toBe("·");
  });
});
