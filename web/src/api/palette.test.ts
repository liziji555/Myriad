import { describe, expect, it } from "vitest";
import { fuzzyScore, MAX_RECENTS, pushRecent, searchPalette, type PaletteItem } from "./palette";

const items: PaletteItem[] = [
  { id: "room:1", group: "room", title: "开发" },
  { id: "room:2", group: "room", title: "产品讨论" },
  { id: "bot:1", group: "bot", title: "Coder", hint: "写代码" },
  { id: "bot:2", group: "bot", title: "Reviewer", hint: "复核代码" },
  { id: "task:1", group: "task", title: "修复登录页的报错", hint: "开发 · 正在做" },
  { id: "action:new-room", group: "action", title: "新建房间" },
  { id: "action:usage", group: "action", title: "打开用量" },
];
const ids = (found: PaletteItem[]) => found.map((item) => item.id);

describe("fuzzyScore", () => {
  it("matches characters in order, not necessarily adjacent", () => {
    expect(fuzzyScore("cdr", "Coder")).not.toBeNull();
    expect(fuzzyScore("rdc", "Coder")).toBeNull();
    expect(fuzzyScore("登录", "修复登录页的报错")).not.toBeNull();
    expect(fuzzyScore("", "anything")).toBe(0);
    expect(fuzzyScore("  ", "anything")).toBe(0);
  });

  it("prefers adjacent characters, the start, and shorter text", () => {
    const score = (query: string, text: string) => fuzzyScore(query, text) ?? -Infinity;
    expect(score("co", "Coder")).toBeGreaterThan(score("co", "c-x-o"));
    expect(score("re", "Reviewer")).toBeGreaterThan(score("re", "Coder helper"));
    expect(score("开发", "开发")).toBeGreaterThan(score("开发", "开发环境的讨论"));
  });
});

describe("searchPalette", () => {
  it("ranks a title match above a match in the small print", () => {
    expect(ids(searchPalette(items, "开发", "all", []))).toEqual(["room:1", "task:1"]);
    expect(ids(searchPalette(items, "代码", "all", []))).toEqual(["bot:1", "bot:2"]);
  });

  it("stays inside the chosen tab", () => {
    expect(ids(searchPalette(items, "开", "action", []))).toEqual(["action:usage"]);
    expect(ids(searchPalette(items, "", "bot", []))).toEqual(["bot:1", "bot:2"]);
    expect(searchPalette(items, "zzz", "all", [])).toEqual([]);
  });

  it("puts recently used items first when nothing is typed, and skips ones that are gone", () => {
    const found = searchPalette(items, "", "all", ["action:usage", "room:gone", "bot:2"]);
    expect(ids(found).slice(0, 3)).toEqual(["action:usage", "bot:2", "room:1"]);
    expect(found).toHaveLength(items.length);
  });
});

describe("pushRecent", () => {
  it("moves a reused item to the front and keeps the list short", () => {
    expect(pushRecent(["a", "b", "c"], "b")).toEqual(["b", "a", "c"]);
    const many = Array.from({ length: 10 }, (_, i) => `item-${i}`);
    expect(pushRecent(many, "new")).toHaveLength(MAX_RECENTS);
  });
});
