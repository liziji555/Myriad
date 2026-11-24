import { describe, expect, it } from "vitest";
import { connectionLabel, desktopInvoke, toFailure, toForm, toTarget, type DesktopState } from "./desktop";

const local: DesktopState = {
  current: { mode: "local" },
  workdir: "/w",
  pinned: null,
  saved: null,
  connecting: null,
  failed: null,
};

describe("desktop", () => {
  it("is absent in a plain browser", () => {
    expect(desktopInvoke({})).toBeNull();
    expect(desktopInvoke({ __TAURI__: { core: { invoke: async () => null } } })).toBeNull();
    const invoke = async () => null;
    expect(desktopInvoke({ __SA_DESKTOP__: true, __TAURI__: { core: { invoke } } })).toBe(invoke);
  });

  it("labels the connection by where it runs and what it is doing", () => {
    expect(connectionLabel(null)).toBe("本机");
    expect(connectionLabel(local)).toBe("本机");
    expect(connectionLabel({ ...local, current: { mode: "server", server: "me@vps" } })).toBe("me@vps");
    expect(connectionLabel({ ...local, connecting: "me@vps" })).toBe("正在连 me@vps…");
    expect(connectionLabel({ ...local, failed: { server: "me@vps", reason: "x" } })).toBe("本机 · 没连上服务器");
  });

  it("splits the remembered address into the form and joins it back", () => {
    const form = toForm({ host: "me@1.2.3.4", port: 2222, key: "/k" });
    expect(form).toEqual({ host: "1.2.3.4", user: "me", port: "2222", key: "/k" });
    expect(toTarget(form)).toEqual({ host: "me@1.2.3.4", port: 2222, key: "/k" });
    expect(toForm(null)).toEqual({ host: "", user: "", port: "", key: "" });
    expect(toTarget({ host: " vps ", user: "", port: "", key: " " })).toEqual({ host: "vps", port: null, key: null });
    // 主机那格里已经写了用户，就不再拼一次。
    expect(toTarget({ host: "a@vps", user: "b", port: "", key: "" })).toMatchObject({ host: "a@vps" });
  });

  it("refuses an empty host and a bad port before asking the shell", () => {
    expect(toTarget({ host: " ", user: "me", port: "", key: "" })).toBe("没有填主机");
    for (const port of ["0", "65536", "22a", "-1"]) {
      expect(typeof toTarget({ host: "vps", user: "", port, key: "" })).toBe("string");
    }
  });

  it("reads a failure whether it is an object or a sentence", () => {
    const failure = { server: "me@vps", reason: "不认密钥", password: true };
    expect(toFailure(failure, "x")).toBe(failure);
    expect(toFailure("只有主窗口能用", "me@vps")).toEqual({ server: "me@vps", reason: "只有主窗口能用" });
  });
});
