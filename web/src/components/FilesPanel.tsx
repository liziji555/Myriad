import { useEffect, useState } from "react";
import { api } from "../api/client";
import type { Bot, BotId, DeskDiff, HarnessDesk, Room, WorkspaceEntry, WorkspaceFile } from "../api/types";

interface Props {
  room: Room;
  members: readonly Bot[];
}

/** 渲染带行色标注的 Unified Diff。 */
export function DiffView({ diff }: { diff: DeskDiff }) {
  if (diff.binary) {
    return <p className="sa-panel-empty">二进制文件，无法文本预览。</p>;
  }
  if (!diff.patch.trim()) {
    return <p className="sa-panel-empty">没有可见的文本差异。</p>;
  }
  return (
    <div className="sa-diff" role="region" aria-label={`Diff ${diff.path}`}>
      {diff.patch.split("\n").map((line, i) => {
        const cls = line.startsWith("@@")
          ? "sa-diff-hunk"
          : line.startsWith("+") && !line.startsWith("+++")
            ? "sa-diff-add"
            : line.startsWith("-") && !line.startsWith("---")
              ? "sa-diff-del"
              : "sa-diff-ctx";
        return (
          <div key={i} className={cls}>
            {line || " "}
          </div>
        );
      })}
      {diff.truncated && <p className="sa-panel-note">差异内容超过 256 KB，已截断显示。</p>}
    </div>
  );
}

/** 工作目录与 Harness 独立工位的文件浏览 + 改动 Diff 审查。 */
export function FilesPanel({ room, members }: Props) {
  const [desks, setDesks] = useState<readonly HarnessDesk[]>([]);
  const [scope, setScope] = useState<"workspace" | BotId>("workspace");
  const [mode, setMode] = useState<"changes" | "all">("all");
  const [dir, setDir] = useState("");
  const [entries, setEntries] = useState<readonly WorkspaceEntry[]>([]);
  const [selectedFile, setSelectedFile] = useState<WorkspaceFile | null>(null);
  const [selectedDiff, setSelectedDiff] = useState<DeskDiff | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    api.listDesks(room.id).then(
      (found) => {
        if (!alive) return;
        setDesks(found);
        const dirty = found.find((d) => d.changes.length > 0);
        if (dirty) {
          setScope(dirty.bot_id);
          setMode("changes");
        }
      },
      () => {},
    );
    return () => {
      alive = false;
    };
  }, [room.id]);

  const activeDesk = scope === "workspace" ? undefined : desks.find((d) => d.bot_id === scope);
  const botName = (id: BotId) => members.find((b) => b.id === id)?.name ?? id;

  useEffect(() => {
    if (mode !== "all") return;
    let alive = true;
    setLoading(true);
    setError(null);
    api.listFiles(room.id, dir, scope === "workspace" ? undefined : scope).then(
      (found) => {
        if (!alive) return;
        setEntries(found);
        setLoading(false);
      },
      (cause: unknown) => {
        if (!alive) return;
        setEntries([]);
        setError(cause instanceof Error ? cause.message : String(cause));
        setLoading(false);
      },
    );
    return () => {
      alive = false;
    };
  }, [room.id, dir, scope, mode]);

  const openFile = (path: string) => {
    setError(null);
    setSelectedDiff(null);
    void api.readFile(room.id, path, scope === "workspace" ? undefined : scope).then(
      (file) => setSelectedFile(file),
      (cause: unknown) => setError(cause instanceof Error ? cause.message : String(cause)),
    );
  };

  const openDiff = (botId: BotId, path: string) => {
    setError(null);
    setSelectedFile(null);
    void api.deskDiff(room.id, botId, path).then(
      (diff) => setSelectedDiff(diff),
      (cause: unknown) => setError(cause instanceof Error ? cause.message : String(cause)),
    );
  };

  const parentDir = dir.includes("/") ? dir.slice(0, dir.lastIndexOf("/")) : "";

  return (
    <div className="sa-files-panel">
      <div className="sa-files-bar">
        <select
          aria-label="查看范围"
          value={scope}
          onChange={(event) => {
            const next = event.target.value as "workspace" | BotId;
            setScope(next);
            setDir("");
            setSelectedFile(null);
            setSelectedDiff(null);
            if (next === "workspace") setMode("all");
          }}
        >
          <option value="workspace">主工作目录</option>
          {desks.map((desk) => (
            <option key={desk.bot_id} value={desk.bot_id}>
              {botName(desk.bot_id)} 独立目录（{desk.changes.length} 处改动）
            </option>
          ))}
        </select>
        {activeDesk && (
          <div className="sa-files-tabs" role="tablist">
            <button
              type="button"
              role="tab"
              aria-selected={mode === "changes"}
              onClick={() => {
                setMode("changes");
                setSelectedFile(null);
              }}
            >
              改动 ({activeDesk.changes.length})
            </button>
            <button
              type="button"
              role="tab"
              aria-selected={mode === "all"}
              onClick={() => {
                setMode("all");
                setSelectedDiff(null);
              }}
            >
              全部文件
            </button>
          </div>
        )}
      </div>

      {error && (
        <p className="sa-form-error" role="alert">
          {error}
        </p>
      )}

      {mode === "changes" && activeDesk ? (
        activeDesk.changes.length === 0 ? (
          <p className="sa-panel-empty">这个独立目录里还没有改动。</p>
        ) : (
          <ul className="sa-files-list">
            {activeDesk.changes.map((change) => (
              <li key={change.path}>
                <button
                  type="button"
                  className="sa-file-row"
                  data-active={selectedDiff?.path === change.path || undefined}
                  onClick={() => openDiff(activeDesk.bot_id, change.path)}
                >
                  <span className="sa-tag">{change.label}</span>
                  <span className="sa-file-path">{change.path}</span>
                </button>
              </li>
            ))}
          </ul>
        )
      ) : (
        <>
          {dir && (
            <div className="sa-files-crumb">
              <button type="button" className="sa-link" onClick={() => setDir(parentDir)}>
                ← 上一级
              </button>
              <code>{dir}</code>
            </div>
          )}
          {loading ? (
            <p className="sa-panel-empty">加载中…</p>
          ) : entries.length === 0 ? (
            <p className="sa-panel-empty">目录为空。</p>
          ) : (
            <ul className="sa-files-list">
              {entries.map((entry) => (
                <li key={entry.path}>
                  <button
                    type="button"
                    className="sa-file-row"
                    data-active={selectedFile?.path === entry.path || undefined}
                    onClick={() => (entry.is_dir ? setDir(entry.path) : openFile(entry.path))}
                  >
                    <span className="sa-file-kind">{entry.is_dir ? "📁" : "📄"}</span>
                    <span className="sa-file-path">{entry.name}</span>
                  </button>
                </li>
              ))}
            </ul>
          )}
        </>
      )}

      {selectedDiff && (
        <section className="sa-files-preview">
          <header className="sa-files-preview-head">
            <code>{selectedDiff.path}</code>
            <button type="button" className="sa-link" onClick={() => setSelectedDiff(null)}>
              收起
            </button>
          </header>
          <DiffView diff={selectedDiff} />
        </section>
      )}

      {selectedFile && (
        <section className="sa-files-preview">
          <header className="sa-files-preview-head">
            <code>{selectedFile.path}</code>
            <button type="button" className="sa-link" onClick={() => setSelectedFile(null)}>
              收起
            </button>
          </header>
          {selectedFile.binary ? (
            <p className="sa-panel-empty">二进制文件，无法文本预览。</p>
          ) : (
            <pre className="sa-file-content">{selectedFile.content}</pre>
          )}
          {selectedFile.truncated && <p className="sa-panel-note">文件超过 512 KB，已截断显示。</p>}
        </section>
      )}
    </div>
  );
}
