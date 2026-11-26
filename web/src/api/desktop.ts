/** 桌面版里「连接」面板要的东西：现在用的是本机还是哪台服务器，正在连谁，上次为什么没连上。浏览器里没有。 */
export interface DesktopState {
  /** 桌面外壳自己的版本号（与远端服务器版本可能不同）。 */
  version?: string;
  current: { mode: "local" } | { mode: "server"; server: string };
  /** 本机的工作目录。 */
  workdir: string | null;
  /** 启动时用 --workdir 或 --server 定死了的那一样，换不了。 */
  pinned: "workdir" | "server" | null;
  /** 上次填的服务器。 */
  saved: { host: string; port: number | null; key: string | null } | null;
  /** 正在后台连的服务器。 */
  connecting: string | null;
  failed: ConnectFailure | null;
}

export interface ConnectFailure {
  server: string;
  reason: string;
  /** 服务器不认这台电脑的密钥：可以用密码登录一次把密钥装上。 */
  password?: boolean;
  /** 是自己取消的、或者又连了别的，不算失败。 */
  cancelled?: boolean;
}

/** 桌面版检查自身更新的结果。 */
export interface DesktopUpdateInfo {
  current: string;
  latest: string | null;
  available: boolean;
  release_url: string | null;
  asset_name: string | null;
  can_download: boolean;
}

/** 面板里填的服务器。后三项可以留空。 */
export interface ServerForm {
  host: string;
  user: string;
  port: string;
  key: string;
}

type Invoke = (command: string, args?: Record<string, unknown>) => Promise<unknown>;

/** 桌面外壳给主窗口的命令入口。浏览器里是 `null`。 */
export function desktopInvoke(source: unknown = globalThis): Invoke | null {
  const page = source as { __SA_DESKTOP__?: unknown; __TAURI__?: { core?: { invoke?: Invoke } } };
  const invoke = page.__TAURI__?.core?.invoke;
  return page.__SA_DESKTOP__ && invoke ? invoke : null;
}

/** 外壳在连接状态变了、菜单里点了「连接」时在 window 上发的事件。 */
export const DESKTOP_CHANGED = "sa-desktop";
export const DESKTOP_OPEN = "sa-desktop-open";

/** 左栏底下那个连接按钮上写的字。 */
export function connectionLabel(state: DesktopState | null): string {
  if (!state) return "本机";
  if (state.connecting) return `正在连 ${state.connecting}…`;
  if (state.current.mode === "server") return state.current.server;
  return state.failed ? "本机 · 没连上服务器" : "本机";
}

/** 记着的 `用户@主机` 拆回表单的两格。 */
export function toForm(saved: DesktopState["saved"]): ServerForm {
  const full = saved?.host ?? "";
  const at = full.lastIndexOf("@");
  return {
    host: at < 0 ? full : full.slice(at + 1),
    user: at < 0 ? "" : full.slice(0, at),
    port: saved?.port == null ? "" : String(saved.port),
    key: saved?.key ?? "",
  };
}

/** 表单拼成命令的参数。端口不是 1–65535 的整数就说哪里不对。 */
export function toTarget(form: ServerForm): { host: string; port: number | null; key: string | null } | string {
  const host = form.host.trim();
  const user = form.user.trim();
  if (host === "") return "没有填主机";
  const port = form.port.trim();
  if (port !== "" && !(/^\d+$/.test(port) && Number(port) >= 1 && Number(port) <= 65535)) {
    return "端口要是 1 到 65535 之间的数";
  }
  return {
    host: user === "" || host.includes("@") ? host : `${user}@${host}`,
    port: port === "" ? null : Number(port),
    key: form.key.trim() === "" ? null : form.key.trim(),
  };
}

/** 命令失败时交回来的东西：连接的失败是对象，别的是一句话。 */
export function toFailure(error: unknown, server: string): ConnectFailure {
  if (error && typeof error === "object" && "reason" in error) return error as ConnectFailure;
  return { server, reason: String(error) };
}
