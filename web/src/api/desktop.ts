/** 桌面版主窗口里由外壳注入的：现在连的是本机还是哪台服务器。浏览器里没有。 */
export interface DesktopInfo {
  current: { mode: "local"; workdir: string } | { mode: "server"; server: string } | null;
  /** 用 --workdir、--server 启动的：换不了。 */
  pinned: boolean;
}

/** 跳到这个地址，桌面外壳会拦下来，打开「连接」窗口。 */
export const CONNECT_PATH = "/__desktop/connect";

export function desktopInfo(source: unknown = globalThis): DesktopInfo | null {
  const info = (source as { __SA_DESKTOP__?: DesktopInfo }).__SA_DESKTOP__;
  return info && typeof info === "object" ? info : null;
}

/** 左栏底下那个连接按钮上写的字。 */
export function connectionLabel(info: DesktopInfo): string {
  return info.current?.mode === "server" ? info.current.server : "本机";
}

export function openConnect(): void {
  window.location.assign(CONNECT_PATH);
}
