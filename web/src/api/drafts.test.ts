import { describe, expect, it } from "vitest";
import { MAX_DRAFT_CHARS, readDraft, writeDraft } from "./drafts";

function memory() {
  const items = new Map<string, string>();
  return {
    items,
    getItem: (key: string) => items.get(key) ?? null,
    setItem: (key: string, value: string) => void items.set(key, value),
    removeItem: (key: string) => void items.delete(key),
  };
}

describe("drafts", () => {
  it("每个房间各记各的", () => {
    const store = memory();
    writeDraft(store, "a", "写了一半");
    writeDraft(store, "b", "另一句");
    expect(readDraft(store, "a")).toBe("写了一半");
    expect(readDraft(store, "b")).toBe("另一句");
    expect(readDraft(store, "c")).toBe("");
  });

  it("清空之后不留东西", () => {
    const store = memory();
    writeDraft(store, "a", "写了一半");
    writeDraft(store, "a", "");
    expect(readDraft(store, "a")).toBe("");
    expect(store.items.size).toBe(0);
  });

  it("太长的不记，之前记的也去掉", () => {
    const store = memory();
    writeDraft(store, "a", "短的");
    writeDraft(store, "a", "字".repeat(MAX_DRAFT_CHARS + 1));
    expect(readDraft(store, "a")).toBe("");
  });

  it("浏览器不让存时不报错", () => {
    const broken = {
      getItem: () => {
        throw new Error("不让读");
      },
      setItem: () => {
        throw new Error("不让写");
      },
      removeItem: () => {
        throw new Error("不让删");
      },
    };
    expect(readDraft(broken, "a")).toBe("");
    expect(() => writeDraft(broken, "a", "x")).not.toThrow();
  });
});
