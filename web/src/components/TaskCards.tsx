import { useState } from "react";
import { STATE_LABEL, isFinished } from "../api/tasks";
import type { TaskActions } from "../api/useWorkspace";
import type { Artifact, Bot, BotId, Task, TaskState } from "../api/types";

export function StateBadge({ state }: { state: TaskState }) {
  return (
    <span className="sa-badge" data-state={state}>
      {STATE_LABEL[state]}
    </span>
  );
}

const ARTIFACT_LABEL: Record<Artifact["kind"], string> = {
  pr: "PR",
  branch: "分支",
  file: "文件",
  report: "报告",
};

export function Artifacts({ artifacts }: { artifacts: readonly Artifact[] }) {
  if (artifacts.length === 0) return null;
  return (
    <ul className="sa-artifacts" aria-label="交付物">
      {artifacts.map((artifact) => (
        <li key={artifact.id}>
          <span className="sa-tag">{ARTIFACT_LABEL[artifact.kind]}</span>
          <span className="sa-artifact-ref" title={artifact.reference}>
            {artifact.reference}
          </span>
          <span className="sa-row-meta">{artifact.summary}</span>
        </li>
      ))}
    </ul>
  );
}

const NOTE_HINT: Partial<Record<TaskState, string>> = {
  review: "哪里不行？打回重做时要写",
  waiting: "写下你的回答",
  blocked: "补充说明（可以不写）",
  working: "补充一句，它下一步就能看到",
};

interface ActionProps {
  task: Task;
  actions: TaskActions;
  /** 房间里启用着的 Bot，可以当负责人。 */
  members: readonly Bot[];
}

/** 一个任务现在能做的操作：验收、打回、回答、继续、停止、取消。结束了的任务没有操作。 */
export function TaskActionBar({ task, actions, members }: ActionProps) {
  const [note, setNote] = useState("");
  const [owner, setOwner] = useState<BotId | "">(task.owner_bot_id ?? "");
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  if (isFinished(task.state)) return null;

  const run = async (name: string, action: () => Promise<void>) => {
    setBusy(name);
    setError(null);
    try {
      await action();
      setNote("");
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    }
    setBusy(null);
  };
  const { state } = task;
  const written = note.trim() !== "";
  // 草稿要先定负责人；卡住的任务可以顺便换一个。
  const pickOwner = state === "draft" || state === "blocked";
  const changedOwner = owner !== "" && owner !== task.owner_bot_id ? owner : undefined;
  const button = (name: string, label: string, action: () => Promise<void>, enabled = true, quiet = true) => (
    <button
      type="button"
      className={quiet ? "sa-button sa-button-quiet" : "sa-button"}
      disabled={busy !== null || !enabled}
      onClick={() => void run(name, action)}
    >
      {busy === name ? "正在提交…" : label}
    </button>
  );

  return (
    <div className="sa-task-actions" role="group" aria-label={`任务「${task.title}」的操作`}>
      {pickOwner && (
        <label className="sa-field">
          <span>负责的 Bot</span>
          <select value={owner} onChange={(event) => setOwner(event.target.value)}>
            <option value="">还没有负责人</option>
            {members.map((bot) => (
              <option key={bot.id} value={bot.id}>
                {bot.name}
              </option>
            ))}
          </select>
        </label>
      )}
      {NOTE_HINT[state] && (
        <textarea
          rows={2}
          value={note}
          placeholder={NOTE_HINT[state]}
          aria-label={NOTE_HINT[state]}
          onChange={(event) => setNote(event.target.value)}
        />
      )}
      {error && (
        <p className="sa-form-error" role="alert">
          {error}
        </p>
      )}
      <div className="sa-form-actions">
        {state === "review" && button("accept", "验收通过", () => actions.accept(task.id), true, false)}
        {state === "review" && button("reject", "打回重做", () => actions.reject(task.id, note), written)}
        {state === "waiting" &&
          button("resume", "回答并继续", () => actions.resume(task.id, note), written, false)}
        {state === "blocked" &&
          button("resume", "继续", () => actions.resume(task.id, note, changedOwner), owner !== "", false)}
        {state === "draft" &&
          button("resume", "开工", () => actions.resume(task.id, "", owner || undefined), owner !== "", false)}
        {state === "working" && button("resume", "带话给它", () => actions.resume(task.id, note), written)}
        {state === "working" && button("stop", "停止", () => actions.stop(task.id))}
        <button
          type="button"
          className="sa-link sa-link-danger"
          disabled={busy !== null}
          onClick={() => {
            if (window.confirm(`取消任务「${task.title}」？它的改动不会保留。`)) {
              void run("cancel", () => actions.cancel(task.id));
            }
          }}
        >
          取消任务
        </button>
      </div>
    </div>
  );
}

/** 交付卡的标题：最新的那张跟着任务现在的状态说话。 */
export function deliveryHeading(task: Task | undefined, latest: boolean): string {
  if (!task || !latest) return "之前交的结果";
  switch (task.state) {
    case "review":
      return "做完了，等你验收";
    case "waiting":
      return "有个问题要问你";
    case "blocked":
      return "卡住了";
    case "dropped":
      return "这件事不需要做";
    case "done":
      return "已经验收";
    case "cancelled":
      return "任务已取消";
    default:
      // 打回或者继续之后又开工了。
      return "之前交的结果";
  }
}
