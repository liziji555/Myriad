import { useEffect, useState } from "react";
import { api } from "../api/client";
import type { DesktopState, DesktopUpdateInfo } from "../api/desktop";
import type { UpdateStatus } from "../api/types";

interface Props {
  serverVersion: string | null;
  desktop: DesktopState | null;
  invokeDesktop: ((command: string, args?: Record<string, unknown>) => Promise<unknown>) | null;
  initialServerUpdate: UpdateStatus | null;
  initialDesktopUpdate: DesktopUpdateInfo | null;
  onChecked(server: UpdateStatus | null, desktop: DesktopUpdateInfo | null): void;
  onClose(): void;
}

/** 检查更新与一键同步更新（远端服务端 + 本机桌面版）。 */
export function UpdatePanel({
  serverVersion,
  desktop,
  invokeDesktop,
  initialServerUpdate,
  initialDesktopUpdate,
  onChecked,
  onClose,
}: Props) {
  const [serverUpdate, setServerUpdate] = useState<UpdateStatus | null>(initialServerUpdate);
  const [desktopUpdate, setDesktopUpdate] = useState<DesktopUpdateInfo | null>(initialDesktopUpdate);
  const [checking, setChecking] = useState(false);
  const [busy, setBusy] = useState<"server" | "desktop" | "both" | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const checkAll = async () => {
    setChecking(true);
    setError(null);
    let nextServer: UpdateStatus | null = null;
    let nextDesktop: DesktopUpdateInfo | null = null;
    const errors: string[] = [];

    const tasks: Promise<void>[] = [
      api.checkUpdate().then(
        (found) => {
          nextServer = found;
          setServerUpdate(found);
        },
        (cause: unknown) => {
          errors.push(`服务端检查失败：${cause instanceof Error ? cause.message : String(cause)}`);
        },
      ),
    ];

    if (invokeDesktop) {
      tasks.push(
        (invokeDesktop("check_desktop_update") as Promise<DesktopUpdateInfo>).then(
          (found) => {
            nextDesktop = found;
            setDesktopUpdate(found);
          },
          (cause: unknown) => {
            errors.push(`桌面版检查失败：${cause instanceof Error ? cause.message : String(cause)}`);
          },
        ),
      );
    }

    await Promise.all(tasks);
    onChecked(nextServer ?? serverUpdate, nextDesktop ?? desktopUpdate);
    if (errors.length > 0) setError(errors.join("；"));
    setChecking(false);
  };

  useEffect(() => {
    void checkAll();
    // 面板打开时主动查一次最新 Release。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !busy) onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [busy, onClose]);

  const updateServer = async () => {
    setError(null);
    setNotice("正在下载并校验服务端更新包…");
    const result = await api.applyUpdate();
    setNotice(result.message);
    return result;
  };

  const updateDesktop = async () => {
    if (!invokeDesktop) return;
    setError(null);
    setNotice("正在下载并校验桌面更新包，完成后将自动就地更新并重启…");
    const result = (await invokeDesktop("install_desktop_update")) as {
      version: string;
      path: string;
      silent?: boolean;
    };
    setNotice(
      result.silent
        ? `桌面版 v${result.version} 已校验完成，正在自动更新并重启 Myriad…`
        : `桌面版 v${result.version} 安装包已校验并启动（${result.path}）。`,
    );
  };

  const runAction = async (kind: "server" | "desktop" | "both") => {
    setBusy(kind);
    setError(null);
    try {
      if (kind === "server") {
        await updateServer();
      } else if (kind === "desktop") {
        await updateDesktop();
      } else {
        const res = await updateServer();
        if (desktopUpdate?.can_download) {
          setNotice(`${res.message} 正在下载桌面版更新包…`);
          await updateDesktop();
        }
      }
    } catch (cause) {
      setNotice(null);
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setBusy(null);
    }
  };

  const remoteServer = desktop?.current.mode === "server" ? desktop.current.server : null;
  const isRemote = remoteServer !== null;
  const isDesktopLocal = Boolean(invokeDesktop && !isRemote);

  // 本机桌面模式下内置服务随桌面版一起打包，不需要单独更新服务端。
  const showServerUpdate = !isDesktopLocal && Boolean(serverUpdate?.can_self_update);
  const showDesktopUpdate = Boolean(invokeDesktop && desktopUpdate?.can_download);
  const canSyncBoth = showServerUpdate && showDesktopUpdate;
  const releaseUrl = serverUpdate?.release_url ?? desktopUpdate?.release_url ?? null;

  const desktopCurrent = desktopUpdate?.current ?? desktop?.version ?? serverVersion ?? "—";
  const serverCurrent = serverUpdate?.current ?? serverVersion ?? "—";

  return (
    <div
      className="sa-palette-backdrop"
      onMouseDown={(event) => event.target === event.currentTarget && !busy && onClose()}
    >
      <div className="sa-connect-panel sa-update-panel" role="dialog" aria-modal="true" aria-label="检查更新">
        <header className="sa-connect-head">
          <h2>软件更新</h2>
          <button type="button" className="sa-link" disabled={busy !== null} onClick={onClose} aria-label="关闭">
            关闭
          </button>
        </header>

        <section className="sa-update-body">
          <div className="sa-update-cards">
            {!isDesktopLocal && (
              <div className="sa-update-card">
                <div className="sa-update-card-head">
                  <span className="sa-update-card-title">
                    {remoteServer ? `远端服务端（${remoteServer}）` : "服务端（sa）"}
                  </span>
                  <StatusPill
                    checking={checking}
                    available={Boolean(serverUpdate?.available)}
                    hasLatest={Boolean(serverUpdate?.latest)}
                  />
                </div>
                <div className="sa-update-card-ver">
                  <span>当前 v{serverCurrent}</span>
                  {serverUpdate?.latest && serverUpdate.available && (
                    <>
                      <span className="sa-update-arrow">→</span>
                      <strong>最新 v{serverUpdate.latest}</strong>
                    </>
                  )}
                </div>
              </div>
            )}

            {invokeDesktop && (
              <div className="sa-update-card">
                <div className="sa-update-card-head">
                  <span className="sa-update-card-title">
                    {isDesktopLocal ? "Myriad 桌面版（含内置本机服务）" : "本机桌面版"}
                  </span>
                  <StatusPill
                    checking={checking}
                    available={Boolean(desktopUpdate?.available)}
                    hasLatest={Boolean(desktopUpdate?.latest)}
                  />
                </div>
                <div className="sa-update-card-ver">
                  <span>当前 v{desktopCurrent}</span>
                  {desktopUpdate?.latest && desktopUpdate.available && (
                    <>
                      <span className="sa-update-arrow">→</span>
                      <strong>最新 v{desktopUpdate.latest}</strong>
                    </>
                  )}
                </div>
              </div>
            )}
          </div>

          {serverUpdate?.notes && (
            <div className="sa-update-notes-wrap">
              <div className="sa-update-section-label">更新说明</div>
              <pre className="sa-update-notes">{serverUpdate.notes}</pre>
            </div>
          )}

          {(canSyncBoth || showServerUpdate || showDesktopUpdate) && (
            <div className="sa-update-primary">
              {canSyncBoth && (
                <button
                  type="button"
                  className="sa-button sa-update-btn"
                  disabled={busy !== null || checking}
                  onClick={() => void runAction("both")}
                >
                  {busy === "both" ? "正在同步更新…" : "同步更新服务端与桌面版"}
                </button>
              )}
              {showServerUpdate && (
                <button
                  type="button"
                  className={canSyncBoth ? "sa-button sa-button-quiet sa-update-btn" : "sa-button sa-update-btn"}
                  disabled={busy !== null || checking}
                  onClick={() => void runAction("server")}
                >
                  {busy === "server"
                    ? "正在更新服务端…"
                    : canSyncBoth
                      ? "仅更新服务端"
                      : "更新服务端"}
                </button>
              )}
              {showDesktopUpdate && (
                <button
                  type="button"
                  className={canSyncBoth ? "sa-button sa-button-quiet sa-update-btn" : "sa-button sa-update-btn"}
                  disabled={busy !== null || checking}
                  onClick={() => void runAction("desktop")}
                >
                  {busy === "desktop"
                    ? "正在下载并更新…"
                    : canSyncBoth
                      ? "仅更新桌面版"
                      : desktopUpdate?.latest
                        ? `立即更新到 v${desktopUpdate.latest}`
                        : "立即更新桌面版"}
                </button>
              )}
            </div>
          )}

          <div className="sa-update-secondary">
            <button
              type="button"
              className="sa-button sa-button-quiet sa-update-btn"
              disabled={busy !== null || checking}
              onClick={() => void checkAll()}
            >
              {checking ? "正在检查…" : "重新检查"}
            </button>
            {releaseUrl && /^https?:\/\//i.test(releaseUrl) && (
              <a className="sa-link sa-update-link" href={releaseUrl} target="_blank" rel="noreferrer">
                查看 GitHub 发布页 ↗
              </a>
            )}
          </div>

          <p className="sa-links-hint">
            {isRemote
              ? "当前已连接远端服务器：点击同步更新会自动更新服务器并重启服务，同时就地更新本机桌面端。"
              : isDesktopLocal
                ? "当前处于本机模式：桌面版已内置本机服务，直接更新桌面版即可。如需更新远端 VPS，请先点击左下角切换到服务器。"
                : "更新包均会核对 GitHub Release 的 SHA256SUMS.txt；服务端以 systemd 常驻运行时会自动重启生效。"}
          </p>
        </section>

        {notice && (
          <p className="sa-update-notice" role="status">
            {notice}
          </p>
        )}
        {error && (
          <p className="sa-form-error" role="alert">
            {error}
          </p>
        )}
      </div>
    </div>
  );
}

function StatusPill({
  checking,
  available,
  hasLatest,
}: {
  checking: boolean;
  available: boolean;
  hasLatest: boolean;
}) {
  if (checking) {
    return <span className="sa-update-pill">检查中…</span>;
  }
  if (available) {
    return <span className="sa-update-pill" data-state="available">可更新</span>;
  }
  if (hasLatest) {
    return <span className="sa-update-pill" data-state="latest">已是最新</span>;
  }
  return <span className="sa-update-pill">未发现发布</span>;
}
