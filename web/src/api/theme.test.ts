import { describe, expect, it } from "vitest";
import { nextTheme, parseTheme } from "./theme";

describe("parseTheme", () => {
  it("认得三种写法", () => {
    expect(parseTheme("light")).toBe("light");
    expect(parseTheme("dark")).toBe("dark");
    expect(parseTheme("system")).toBe("system");
  });
  it("没存过、存的不认识时跟着系统", () => {
    expect(parseTheme(null)).toBe("system");
    expect(parseTheme("")).toBe("system");
    expect(parseTheme("blue")).toBe("system");
  });
});

describe("nextTheme", () => {
  it("三种轮着换，换三次回到原处", () => {
    expect(nextTheme("system")).toBe("light");
    expect(nextTheme("light")).toBe("dark");
    expect(nextTheme("dark")).toBe("system");
  });
});
