import { useCallback, useEffect, useRef, useState } from "react";
import { AUDIT_KINDS, appendAudit, auditTime, auditWho, type AuditKind } from "../api/audit";
import { api } from "../api/client";
import type { AuditEntry, Room, Task } from "../api/types";

const PAGE = 50;

type Scope = "room" | "task" | "all";

interface Props {
  room: Room;
  /** 任务页里展开着的任务：可以只看它的记录。 */
  task: Task | null;
}

/**
 * 右栏的「审计」页：谁在什么时候批准了什么、Bot 动手做了什么、任务走到了哪一步、程序自己停下了什么。
 * 只看不改。一件事没做成，从这里能顺着时间找到是卡在哪一步。
 */
export function AuditPanel({ room, task }: Props) {
  const [scope, setScope] = useState<Scope>("room");
  const [kind, setKind] = useState<AuditKind>("all");
  const [entries, setEntries] = useState<AuditEntry[] | null>(null);
  const [hasMore, setHasMore] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // 换了条件之后，先前发出去的请求回来了也不用。
  const latest = useRef(0);

  // 展开的任务收起来了，「这个任务」这一项也就没有了。
  const effective: Scope = scope === "task" && !task ? "room" : scope;
  const roomId = effective === "room" ? room.id : undefined;
  const taskId = effective === "task" ? task?.id : undefined;

  const load = useCallback(
    async (before?: number) => {
      const ticket = ++latest.current;
      setBusy(true);
      try {
        const page = await api.audit({ room: roomId, task: taskId, kind, before, limit: PAGE });
        if (ticket !== latest.current) return;
        setEntries((have) => (before === undefined ? page.entries : appendAudit(have ?? [], page.entries)));
        setHasMore(page.has_more);
        setError(null);
      } catch (cause) {
        if (ticket !== latest.current) return;
        setError(cause instanceof Error ? cause.message : String(cause));
      }
      setBusy(false);
    },
    [roomId, taskId, kind],
  );

  useEffect(() => {
    setEntries(null);
    void load();
  }, [load]);

  const scopes: { id: Scope; label: string }[] = [
    { id: "room", label: "这个房间" },
    ...(task ? [{ id: "task" as const, label: `任务「${task.title}」` }] : []),
    { id: "all", label: "所有房间" },
  ];
  const oldest = entries?.[entries.length - 1];

  return (
    <div className="sa-members">
      <div className="sa-presets" role="group" aria-label="看哪里的记录">
        {scopes.map(({ id, label }) => (
          <button
            key={id}
            type="button"
            className="sa-chip sa-audit-chip"
            aria-pressed={effective === id}
            onClick={() => setScope(id)}
          >
            {label}
          </button>
        ))}
      </div>
      <div className="sa-presets" role="group" aria-label="看哪一类记录">
        {AUDIT_KINDS.map(({ id, label }) => (
          <button key={id} type="button" className="sa-chip" aria-pressed={kind === id} onClick={() => setKind(id)}>
            {label}
          </button>
        ))}
      </div>
      {error && (
        <p className="sa-form-error" role="alert">
          审计记录没拉到：{error}
        </p>
      )}
      {entries === null ? (
        !error && <p className="sa-panel-empty">正在加载…</p>
      ) : entries.length === 0 ? (
        <p className="sa-panel-empty">
          还没有这样的记录。审批、Bot 执行命令和改文件、任务换状态、改 Bot 和房间的设置，都会记在这里。
        </p>
      ) : (
        <ol className="sa-audit" aria-label="审计记录">
          {entries.map((entry) => (
            <li key={entry.id} className="sa-audit-entry" data-actor={entry.actor}>
              <div className="sa-audit-head">
                <span className="sa-audit-who">{auditWho(entry)}</span>
                <time className="sa-row-meta" dateTime={new Date(entry.at).toISOString()}>
                  {auditTime(entry.at)}
                </time>
              </div>
              <p className="sa-audit-summary">{entry.summary}</p>
              {entry.detail && (
                <details className="sa-audit-detail">
                  <summary>细节</summary>
                  <pre>{entry.detail}</pre>
                </details>
              )}
              <span className="sa-audit-action">{entry.action}</span>
            </li>
          ))}
        </ol>
      )}
      <div className="sa-audit-buttons">
        {hasMore && oldest && (
          <button
            type="button"
            className="sa-button sa-button-quiet"
            disabled={busy}
            onClick={() => void load(oldest.id)}
          >
            更早的
          </button>
        )}
        <button type="button" className="sa-button sa-button-quiet" disabled={busy} onClick={() => void load()}>
          {busy ? "正在刷新…" : "刷新"}
        </button>
      </div>
    </div>
  );
}
