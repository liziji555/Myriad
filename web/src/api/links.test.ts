import { describe, expect, it } from "vitest";
import {
  looksLikeRepo,
  minutesLeft,
  pairingCommand,
  pairingLink,
  repoFromInput,
  safeUrl,
  sourceName,
  splitEvent,
} from "./links";

describe("splitEvent", () => {
  it("separates the one-line summary from what the outsider wrote", () => {
    expect(splitEvent("alice 开了 issue #12「登录报错」\n\n点登录就 500\n\n复现步骤…")).toEqual({
      summary: "alice 开了 issue #12「登录报错」",
      body: "点登录就 500\n\n复现步骤…",
    });
    expect(splitEvent("CI「CI」通过了（分支 main）\n\n")).toEqual({
      summary: "CI「CI」通过了（分支 main）",
      body: "",
    });
    expect(splitEvent("只有一句")).toEqual({ summary: "只有一句", body: "" });
  });
});

describe("safeUrl", () => {
  it("only lets http and https through", () => {
    expect(safeUrl("https://github.com/me/app/issues/12")).toBe("https://github.com/me/app/issues/12");
    expect(safeUrl("http://ghe.local/x")).toBe("http://ghe.local/x");
    expect(safeUrl("javascript:alert(1)")).toBeUndefined();
    expect(safeUrl("data:text/html,x")).toBeUndefined();
    expect(safeUrl("不是地址")).toBeUndefined();
    expect(safeUrl(undefined)).toBeUndefined();
  });
});

describe("pairing", () => {
  const pairing = { code: "ABC234", expires_at: 600_000 };

  it("gives the command to type and, once the bot is known, a link", () => {
    expect(pairingCommand(pairing)).toBe("/pair ABC234");
    expect(pairingLink(pairing)).toBeUndefined();
    expect(pairingLink({ ...pairing, bot_username: "sa_bot" })).toBe("https://t.me/sa_bot?start=ABC234");
    expect(pairingLink({ ...pairing, bot_username: "a/b?x" })).toBeUndefined();
  });

  it("counts the minutes left, rounding up", () => {
    expect(minutesLeft(pairing, 0)).toBe(10);
    expect(minutesLeft(pairing, 541_000)).toBe(1);
    expect(minutesLeft(pairing, 600_000)).toBe(0);
    expect(minutesLeft(pairing, 700_000)).toBe(0);
  });
});

describe("repos", () => {
  it("accepts owner/name and pulls it out of a pasted address", () => {
    expect(looksLikeRepo(" me/app ")).toBe(true);
    expect(looksLikeRepo("me/my.app-2")).toBe(true);
    for (const bad of ["", "app", "me/", "me/app/x", "me/a b"]) expect(looksLikeRepo(bad)).toBe(false);

    expect(repoFromInput("https://github.com/Me/App")).toBe("Me/App");
    expect(repoFromInput("github.com/me/app.git")).toBe("me/app");
    expect(repoFromInput("https://github.com/me/app/issues/12")).toBe("me/app");
    expect(repoFromInput(" me/app ")).toBe("me/app");
  });

  it("names the sources", () => {
    expect(sourceName("github")).toBe("GitHub");
    expect(sourceName("telegram")).toBe("Telegram");
  });
});
