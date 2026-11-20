import { useState } from "react";
import type { Notice, NoticeId, TaskId } from "../api/types";

interface Props {
  notices: readonly Notice[];
  onDismiss(id: NoticeId): Promise<void>;
  onShowTask(taskId: TaskId): void;
  onShowUsage(): void;
}

/** 时间线上方的提醒：花费快到上限、任务被自动停下……不是谁说的话，看完可以收起。 */
export function Notices({ notices, onDismiss, onShowTask, onShowUsage }: Props) {
  const [error, setError] = useState<string | null>(null);
  if (notices.length === 0) return null;
  return (
    <ul className="sa-notices" aria-label="提醒">
      {notices.map((notice) => (
        <li key={notice.id} className="sa-notice" data-level={notice.level} role="status">
          <p className="sa-notice-text">{notice.text}</p>
          <div className="sa-notice-actions">
            {notice.task_id && (
              <button type="button" className="sa-link" onClick={() => onShowTask(notice.task_id!)}>
                看任务
              </button>
            )}
            {(notice.kind === "budget" || notice.kind === "spend") && (
              <button type="button" className="sa-link" onClick={onShowUsage}>
                看用量
              </button>
            )}
            <button
              type="button"
              className="sa-link"
              onClick={() => {
                setError(null);
                onDismiss(notice.id).catch((cause: unknown) =>
                  setError(cause instanceof Error ? cause.message : String(cause)),
                );
              }}
            >
              收起
            </button>
          </div>
        </li>
      ))}
      {error && (
        <li className="sa-notice-error" role="alert">
          没能收起：{error}
        </li>
      )}
    </ul>
  );
}
