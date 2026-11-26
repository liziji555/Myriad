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
    setNotice("正在下载并校验桌面安装包，完成后会自动打开安装向导…");
    const result = (await invokeDesktop("install_desktop_update")) as { version: string; path: string };
    setNotice(`桌面版 v${result.version} 安装包已校验并启动（${result.path}）。`);
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
          setNotice(`${res.message} 正在下载桌面版安装包…`);
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
  const serverLabel = remoteServer ? `远端服务端（${remoteServer}）` : "服务端（sa）";
  const canSyncBoth = Boolean(serverUpdate?.can_self_update && desktopUpdate?.can_download);
  const releaseUrl = serverUpdate?.release_url ?? desktopUpdate?.release_url ?? null;

  return (
    <div
      className="sa-palette-backdrop"
      onMouseDown={(event) => event.target === event.currentTarget && !busy && onClose()}
    >
      <div className="sa-connect-panel" role="dialog" aria-modal="true" aria-label="检查更新">
        <header className="sa-connect-head">
          <h2>检查更新</h2>
          <button type="button" className="sa-link" disabled={busy !== null} onClick={onClose} aria-label="关闭">
            关闭
          </button>
        </header>

        <section className="sa-form">
          <div className="sa-field">
            {serverLabel}
            <code className="sa-connect-path">
              当前 v{serverUpdate?.current ?? serverVersion ?? "—"}
              {serverUpdate?.latest
                ? serverUpdate.available
                  ? ` → 最新 v${serverUpdate.latest}`
                  : `（已是最新 v${serverUpdate.latest}）`
                : serverUpdate
                  ? "（GitHub 尚未发布 Release）"
                  : checking
                    ? " · 正在检查…"
                    : ""}
            </code>
          </div>

          {invokeDesktop && (
            <div className="sa-field">
              本机桌面版
              <code className="sa-connect-path">
                当前 v{desktopUpdate?.current ?? desktop?.version ?? "—"}
                {desktopUpdate?.latest
                  ? desktopUpdate.available
                    ? ` → 最新 v${desktopUpdate.latest}`
                    : `（已是最新 v${desktopUpdate.latest}）`
                  : desktopUpdate
                    ? "（GitHub 尚未发布 Release）"
                    : checking
                      ? " · 正在检查…"
                      : ""}
              </code>
            </div>
          )}

          {serverUpdate?.notes && (
            <div className="sa-field">
              更新说明
              <pre className="sa-update-notes">{serverUpdate.notes}</pre>
            </div>
          )}

          <div className="sa-connect-actions">
            {canSyncBoth && (
              <button
                type="button"
                className="sa-button"
                disabled={busy !== null || checking}
                onClick={() => void runAction("both")}
              >
                {busy === "both" ? "正在同步更新…" : "一键同步更新（服务端 + 桌面版）"}
              </button>
            )}
            {serverUpdate?.can_self_update && (
              <button
                type="button"
                className={canSyncBoth ? "sa-button sa-button-quiet" : "sa-button"}
                disabled={busy !== null || checking}
                onClick={() => void runAction("server")}
              >
                {busy === "server" ? "正在更新服务端…" : "更新服务端"}
              </button>
            )}
            {invokeDesktop && desktopUpdate?.can_download && (
              <button
                type="button"
                className={canSyncBoth ? "sa-button sa-button-quiet" : "sa-button"}
                disabled={busy !== null || checking}
                onClick={() => void runAction("desktop")}
              >
                {busy === "desktop" ? "正在下载桌面版…" : "下载并安装桌面新版"}
              </button>
            )}
            <button
              type="button"
              className="sa-button sa-button-quiet"
              disabled={busy !== null || checking}
              onClick={() => void checkAll()}
            >
              {checking ? "检查中…" : "重新检查"}
            </button>
            {releaseUrl && /^https?:\/\//i.test(releaseUrl) && (
              <a className="sa-link" href={releaseUrl} target="_blank" rel="noreferrer">
                打开发布页
              </a>
            )}
          </div>

          <p className="sa-links-hint">
            {isRemote
              ? "远程连接时界面由服务器提供：更新服务端并自动重启后，页面重连即可用上新版界面；桌面外壳有更新时可一键下载校验并启动安装向导。"
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
