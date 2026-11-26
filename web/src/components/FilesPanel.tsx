import { useEffect, useMemo, useState } from "react";
import { api } from "../api/client";
import { formatRewindSummary, isRewindConflict } from "../api/checkpoints";
import type {
  Bot,
  BotId,
  CheckpointInfo,
  DeskDiff,
  HarnessDesk,
  RewindReport,
  Room,
  WorkspaceEntry,
  WorkspaceFile,
} from "../api/types";
import { Icon } from "./Icon";

interface Props {
  room: Room;
  members: readonly Bot[];
  checkpoints?: readonly CheckpointInfo[];
  onRewindCheckpoint?(checkpointId: string, force: boolean): Promise<RewindReport>;
  onJumpMessage?(messageId: string): Promise<boolean> | boolean | void;
}

interface ParsedDiffLine {
  cls: string;
  lineNo: string;
  text: string;
}

function parsePatch(patch: string): { lines: ParsedDiffLine[]; added: number; removed: number } {
  const lines: ParsedDiffLine[] = [];
  let added = 0;
  let removed = 0;
  let newNo = 0;

  for (const raw of patch.split("\n")) {
    if (raw.startsWith("@@")) {
      const m = /\+(\d+)/.exec(raw);
      if (m) newNo = Number(m[1]) - 1;
      lines.push({ cls: "sa-diff-hunk", lineNo: "⋯", text: raw });
    } else if (raw.startsWith("+++") || raw.startsWith("---") || raw.startsWith("diff ") || raw.startsWith("index ")) {
      continue;
    } else if (raw.startsWith("+")) {
      added++;
      newNo++;
      lines.push({ cls: "sa-diff-add", lineNo: String(newNo), text: raw });
    } else if (raw.startsWith("-")) {
      removed++;
      lines.push({ cls: "sa-diff-del", lineNo: "−", text: raw });
    } else {
      if (newNo > 0) newNo++;
      lines.push({ cls: "sa-diff-ctx", lineNo: newNo > 0 ? String(newNo) : "", text: raw || " " });
    }
  }
  return { lines, added, removed };
}

/** 渲染带精确行号与统计的 Unified Diff（Lancer / t3code 风格）。 */
export function DiffView({ diff }: { diff: DeskDiff }) {
  const parsed = useMemo(() => parsePatch(diff.patch), [diff.patch]);

  if (diff.binary) {
    return <p className="sa-panel-empty">二进制文件，无法文本预览。</p>;
  }
  if (!diff.patch.trim() || parsed.lines.length === 0) {
    return <p className="sa-panel-empty">没有可见的文本差异。</p>;
  }
  return (
    <div className="sa-diff-wrap">
      <div className="sa-diff-stats">
        <span className="sa-diff-stat-add">+{parsed.added}</span>
        <span className="sa-diff-stat-del">−{parsed.removed}</span>
      </div>
      <div className="sa-diff" role="region" aria-label={`Diff ${diff.path}`}>
        {parsed.lines.map((row, i) => (
          <div key={i} className={`sa-diff-row ${row.cls}`}>
            <span className="sa-diff-ln" aria-hidden="true">
              {row.lineNo}
            </span>
            <span className="sa-diff-code">{row.text}</span>
          </div>
        ))}
        {diff.truncated && <p className="sa-panel-note">差异内容超过 256 KB，已截断显示。</p>}
      </div>
    </div>
  );
}

/** 工作目录与 Harness 独立工位的文件浏览 + 改动 Diff 审查 + 轮次检查点回滚。 */
export function FilesPanel({ room, members, checkpoints = [], onRewindCheckpoint, onJumpMessage }: Props) {
  const [desks, setDesks] = useState<readonly HarnessDesk[]>([]);
  const [scope, setScope] = useState<"workspace" | BotId>("workspace");
  const [mode, setMode] = useState<"changes" | "all" | "checkpoints">("all");
  const [dir, setDir] = useState("");
  const [entries, setEntries] = useState<readonly WorkspaceEntry[]>([]);
  const [selectedFile, setSelectedFile] = useState<WorkspaceFile | null>(null);
  const [selectedDiff, setSelectedDiff] = useState<DeskDiff | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [confirmCp, setConfirmCp] = useState<string | null>(null);
  const [cpBusy, setCpBusy] = useState(false);
  const [cpConflict, setCpConflict] = useState<{ id: string; message: string } | null>(null);
  const [cpNotice, setCpNotice] = useState<string | null>(null);

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
          className="sa-select"
          aria-label="查看范围"
          value={scope}
          onChange={(event) => {
            const next = event.target.value as "workspace" | BotId;
            setScope(next);
            setDir("");
            setSelectedFile(null);
            setSelectedDiff(null);
            if (next === "workspace") {
              setMode("all");
            } else if (mode === "checkpoints") {
              setMode("changes");
            }
          }}
        >
          <option value="workspace">主工作目录 (main)</option>
          {desks.map((desk) => (
            <option key={desk.bot_id} value={desk.bot_id}>
              {botName(desk.bot_id)} 工位（{desk.changes.length} 改动）
            </option>
          ))}
        </select>
        {activeDesk ? (
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
        ) : (
          !room.share && (
            <div className="sa-files-tabs" role="tablist">
              <button
                type="button"
                role="tab"
                aria-selected={mode === "all"}
                onClick={() => setMode("all")}
              >
                全部文件
              </button>
              <button
                type="button"
                role="tab"
                aria-selected={mode === "checkpoints"}
                onClick={() => {
                  setMode("checkpoints");
                  setSelectedFile(null);
                  setSelectedDiff(null);
                }}
              >
                检查点 ({checkpoints.length})
              </button>
            </div>
          )
        )}
      </div>

      {error && (
        <p className="sa-form-error" role="alert">
          {error}
        </p>
      )}
      {cpNotice && (
        <p className="sa-checkpoint-ok" role="status">
          {cpNotice}
        </p>
      )}

      {mode === "checkpoints" && !activeDesk ? (
        checkpoints.length === 0 ? (
          <p className="sa-panel-empty">
            这个房间还没有文件检查点。内置 Bot 每次修改或新建工作区文件时会自动快照改动前的版本，方便随时一键回滚。
          </p>
        ) : (
          <ul className="sa-checkpoint-list" aria-label="文件检查点">
            {checkpoints.map((cp) => {
              const expanded = confirmCp === cp.id;
              const conflictHere = cpConflict?.id === cp.id ? cpConflict.message : null;
              const runRewind = async (force: boolean) => {
                if (!onRewindCheckpoint) return;
                setCpBusy(true);
                setError(null);
                setCpConflict(null);
                setCpNotice(null);
                try {
                  const report = await onRewindCheckpoint(cp.id, force);
                  setCpNotice(formatRewindSummary(report));
                  setConfirmCp(null);
                } catch (cause) {
                  const msg = cause instanceof Error ? cause.message : String(cause);
                  if (!force && isRewindConflict(cause)) {
                    setCpConflict({ id: cp.id, message: msg });
                  } else {
                    setError(msg);
                  }
                }
                setCpBusy(false);
              };
              return (
                <li key={cp.id} className="sa-checkpoint-card">
                  <div className="sa-checkpoint-card-head">
                    <span className="sa-row-label">{cp.bot_name}</span>
                    <time className="sa-row-meta" dateTime={new Date(cp.created_at).toISOString()}>
                      {new Date(cp.created_at).toLocaleString("zh-CN", {
                        month: "numeric",
                        day: "numeric",
                        hour: "2-digit",
                        minute: "2-digit",
                        hour12: false,
                      })}
                    </time>
                  </div>
                  <ul className="sa-checkpoint-files">
                    {cp.files.map((file) => (
                      <li key={file}>
                        <code>{file}</code>
                      </li>
                    ))}
                  </ul>
                  <div className="sa-checkpoint-card-actions">
                    {onJumpMessage && (
                      <button
                        type="button"
                        className="sa-link"
                        onClick={() => {
                          setError(null);
                          void Promise.resolve(onJumpMessage(cp.id)).then((found) => {
                            if (found === false) {
                              setError("这条检查点对应的消息已超出当前可定位的历史范围或已被清理。");
                            }
                          });
                        }}
                      >
                        定位消息
                      </button>
                    )}
                    {onRewindCheckpoint && (
                      <button
                        type="button"
                        className="sa-link"
                        disabled={cpBusy}
                        onClick={() => {
                          setConfirmCp(expanded ? null : cp.id);
                          setCpConflict(null);
                          setError(null);
                        }}
                      >
                        {expanded ? "取消" : "回滚至此轮之前"}
                      </button>
                    )}
                  </div>
                  {expanded && (
                    <div className="sa-checkpoint-confirm">
                      {conflictHere && (
                        <p className="sa-msg-error" role="alert">
                          {conflictHere}
                        </p>
                      )}
                      <div className="sa-checkpoint-actions">
                        {conflictHere ? (
                          <button
                            type="button"
                            className="sa-button sa-button-danger"
                            disabled={cpBusy}
                            onClick={() => void runRewind(true)}
                          >
                            {cpBusy ? "正在强制回滚…" : "确认强制覆盖回滚"}
                          </button>
                        ) : (
                          <button
                            type="button"
                            className="sa-button"
                            disabled={cpBusy}
                            onClick={() => void runRewind(false)}
                          >
                            {cpBusy ? "正在回滚…" : "确认回滚"}
                          </button>
                        )}
                        <button
                          type="button"
                          className="sa-button sa-button-quiet"
                          disabled={cpBusy}
                          onClick={() => {
                            setConfirmCp(null);
                            setCpConflict(null);
                          }}
                        >
                          取消
                        </button>
                      </div>
                    </div>
                  )}
                </li>
              );
            })}
          </ul>
        )
      ) : mode === "changes" && activeDesk ? (
        activeDesk.changes.length === 0 ? (
          <p className="sa-panel-empty">这个独立工位里还没有改动。</p>
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
                    <span className="sa-file-kind" aria-hidden="true">
                      <Icon name={entry.is_dir ? "file" : "read"} size={13} />
                    </span>
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
