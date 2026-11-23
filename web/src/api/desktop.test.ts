import { describe, expect, it } from "vitest";
import { connectionLabel, desktopInfo } from "./desktop";

describe("desktop", () => {
  it("is absent in a plain browser", () => {
    expect(desktopInfo({})).toBeNull();
  });

  it("labels the connection by where it runs", () => {
    const local = desktopInfo({ __SA_DESKTOP__: { current: { mode: "local", workdir: "/w" }, pinned: false } });
    expect(local && connectionLabel(local)).toBe("本机");
    const server = desktopInfo({ __SA_DESKTOP__: { current: { mode: "server", server: "me@vps" }, pinned: true } });
    expect(server && connectionLabel(server)).toBe("me@vps");
  });
});
