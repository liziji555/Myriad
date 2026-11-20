import { describe, expect, it } from "vitest";
import { mentionAt } from "./Composer";
import { splitMentions } from "./mentions";

describe("splitMentions", () => {
  it("只高亮真正点到的 Bot，不分大小写，长名字优先", () => {
    expect(splitMentions("@coder 和 @CoderPro 看一下，抄送 @nobody", ["Coder", "CoderPro"])).toEqual([
      { text: "@coder", mention: true },
      { text: " 和 ", mention: false },
      { text: "@CoderPro", mention: true },
      { text: " 看一下，抄送 @nobody", mention: false },
    ]);
  });

  it("没有点名时原样返回", () => {
    expect(splitMentions("邮箱 a@b.com", [])).toEqual([{ text: "邮箱 a@b.com", mention: false }]);
    expect(splitMentions("", ["Coder"])).toEqual([]);
  });
});

describe("mentionAt", () => {
  it("找到光标前正在输入的 @名字", () => {
    expect(mentionAt("请 @Co", 5)).toEqual({ start: 2, query: "Co" });
    expect(mentionAt("@", 1)).toEqual({ start: 0, query: "" });
  });

  it("名字后面已经有空格、或者根本没有 @ 时不弹菜单", () => {
    expect(mentionAt("@Coder 看", 8)).toBeNull();
    expect(mentionAt("你好", 2)).toBeNull();
    // 只看光标前面。
    expect(mentionAt("你好 @Coder", 2)).toBeNull();
  });
});
