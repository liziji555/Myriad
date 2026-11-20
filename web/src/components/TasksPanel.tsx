import { useEffect, useRef, useState, type FormEvent } from "react";
import { STATE_LABEL, groupTasks, isFinished, runsOf, type TaskBoard } from "../api/tasks";
import type { TaskActions } from "../api/useWorkspace";
import type { Bot, BotId, Room, Run, Task, TaskId } from "../api/types";
import { Artifacts, StateBadge, TaskActionBar } from "./TaskCards";

interface Props {
  room: Room;
  board: TaskBoard;
  /** 房间里启用着的 Bot。 */
  members: readonly Bot[];
  bots: readonly Bot[];
  actions: TaskActions;
  /** 要展开并滚到的任务（从时间线上点过来的）。 */
  focus: TaskId | null;
  onFocus(taskId: TaskId | null): void;
}

export function TasksPanel({ room, board, members, bots, actions, focus, onFocus }: Props) {
  const [creating, setCreating] = useState(false);
  const tasks = board.rooms[room.id] ?? [];
  const groups = groupTasks(tasks);
  const active = groups.filter((group) => !isFinished(group.state));
  const finished = groups.filter((group) => isFinished(group.state));
  const finishedCount = finished.reduce((sum, group) => sum + group.tasks.length, 0);
  const item = (task: Task) => (
    <TaskItem
      key={task.id}
      task={task}
      runs={runsOf(board, task.id)}
      members={members}
      bots={bots}
      actions={actions}
      open={focus === task.id}
      onToggle={() => onFocus(focus === task.id ? null : task.id)}
    />
  );

  return (
    <div className="sa-tasks">
      {creating ? (
        <NewTaskForm
          members={members}
          defaultOwner={room.host_bot_id}
          onCreate={async (body) => {
            const task = await actions.create(room.id, body);
            setCreating(false);
            onFocus(task.id);
          }}
          onCancel={() => setCreating(false)}
        />
      ) : (
        <button type="button" className="sa-button sa-button-quiet" onClick={() => setCreating(true)}>
          新建任务
        </button>
      )}
      {tasks.length === 0 && !creating && (
        <p className="sa-panel-empty">
          还没有任务。让 Bot 去做一件要花些功夫的事，它会开一个任务，做完在这里等你验收。
        </p>
      )}
      {active.map((group) => (
        <section key={group.state} aria-label={STATE_LABEL[group.state]}>
          <h3 className="sa-panel-heading">
            {STATE_LABEL[group.state]} · {group.tasks.length}
          </h3>
          <ul className="sa-task-list">{group.tasks.map(item)}</ul>
        </section>
      ))}
      {finishedCount > 0 && (
        <details className="sa-task-finished" open={finished.some((g) => g.tasks.some((t) => t.id === focus))}>
          <summary className="sa-panel-heading">已结束 · {finishedCount}</summary>
          <ul className="sa-task-list">{finished.flatMap((group) => group.tasks).map(item)}</ul>
        </details>
      )}
    </div>
  );
}

const RUN_LABEL: Record<Run["state"], string> = {
  running: "进行中",
  succeeded: "结束",
  failed: "出错",
  cancelled: "被停下",
  interrupted: "中断",
};

/** 任务的那句说明是什么意思，跟着状态变。 */
const NOTE_LABEL: Partial<Record<Task["state"], string>> = {
  waiting: "它问",
  blocked: "卡在",
  dropped: "原因",
};

function TaskItem({
  task,
  runs,
  members,
  bots,
  actions,
  open,
  onToggle,
}: {
  task: Task;
  runs: readonly Run[];
  members: readonly Bot[];
  bots: readonly Bot[];
  actions: TaskActions;
  open: boolean;
  onToggle(): void;
}) {
  const row = useRef<HTMLLIElement>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const { loadRuns } = actions;
  // 展开时拉一次执行历史；任务每变一次（又跑了一轮）再拉一次。
  useEffect(() => {
    if (!open) return;
    let stale = false;
    setLoadError(null);
    loadRuns(task.id).catch((cause: unknown) => {
      if (!stale) setLoadError(cause instanceof Error ? cause.message : String(cause));
    });
    return () => {
      stale = true;
    };
  }, [open, task.id, task.rev, loadRuns]);
  useEffect(() => {
    if (open) row.current?.scrollIntoView({ block: "nearest" });
  }, [open]);

  const owner = bots.find((bot) => bot.id === task.owner_bot_id)?.name;
  return (
    <li ref={row} className="sa-task" data-open={open} data-task-id={task.id}>
      <button type="button" className="sa-row" aria-expanded={open} onClick={onToggle}>
        <span className="sa-row-label">{task.title}</span>
        <span className="sa-row-meta">{owner ?? "没有负责人"}</span>
      </button>
      {open && (
        <div className="sa-task-detail">
          <StateBadge state={task.state} />
          {task.note !== "" && (
            <p className="sa-task-note">
              {NOTE_LABEL[task.state] ?? "说明"}：{task.note}
            </p>
          )}
          <dl className="sa-task-fields">
            <dt>要做什么</dt>
            <dd>{task.scope}</dd>
            {task.acceptance !== "" && (
              <>
                <dt>验收标准</dt>
                <dd>{task.acceptance}</dd>
              </>
            )}
          </dl>
          <Artifacts artifacts={task.artifacts} />
          {runs.length > 0 && (
            <ol className="sa-runs" aria-label="执行历史">
              {runs.map((run) => (
                <li key={run.id} data-state={run.state}>
                  <span className="sa-row-meta">
                    第 {run.attempt} 次 · {RUN_LABEL[run.state]}
                  </span>
                  {(run.report?.summary ?? run.error) && <p>{run.report?.summary ?? run.error}</p>}
                </li>
              ))}
            </ol>
          )}
          {loadError && (
            <p className="sa-form-error" role="alert">
              执行历史没加载出来：{loadError}
            </p>
          )}
          <TaskActionBar key={task.state} task={task} actions={actions} members={members} />
        </div>
      )}
    </li>
  );
}

function NewTaskForm({
  members,
  defaultOwner,
  onCreate,
  onCancel,
}: {
  members: readonly Bot[];
  defaultOwner: BotId | null;
  onCreate(body: { title: string; scope: string; acceptance?: string; owner_bot_id?: BotId }): Promise<void>;
  onCancel(): void;
}) {
  const [title, setTitle] = useState("");
  const [scope, setScope] = useState("");
  const [acceptance, setAcceptance] = useState("");
  const [owner, setOwner] = useState<BotId | "">(
    members.some((bot) => bot.id === defaultOwner) ? (defaultOwner ?? "") : "",
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await onCreate({
        title: title.trim(),
        scope: scope.trim(),
        ...(acceptance.trim() ? { acceptance: acceptance.trim() } : {}),
        ...(owner ? { owner_bot_id: owner } : {}),
      });
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
      setBusy(false);
    }
  };

  return (
    <form className="sa-form" onSubmit={(event) => void submit(event)} aria-label="新建任务">
      <label className="sa-field">
        <span>标题</span>
        <input value={title} maxLength={80} required autoFocus onChange={(event) => setTitle(event.target.value)} />
      </label>
      <label className="sa-field">
        <span>要做什么</span>
        <textarea
          rows={4}
          value={scope}
          required
          placeholder="写清楚做什么、相关的文件和背景"
          onChange={(event) => setScope(event.target.value)}
        />
      </label>
      <label className="sa-field">
        <span>验收标准（可以不写）</span>
        <textarea rows={2} value={acceptance} onChange={(event) => setAcceptance(event.target.value)} />
      </label>
      <label className="sa-field">
        <span>负责的 Bot</span>
        <select value={owner} onChange={(event) => setOwner(event.target.value)}>
          <option value="">先不指定（存成草稿）</option>
          {members.map((bot) => (
            <option key={bot.id} value={bot.id}>
              {bot.name}
            </option>
          ))}
        </select>
      </label>
      {error && (
        <p className="sa-form-error" role="alert">
          {error}
        </p>
      )}
      <div className="sa-form-actions">
        <button type="submit" className="sa-button" disabled={busy || title.trim() === "" || scope.trim() === ""}>
          {busy ? "正在创建…" : owner ? "创建并开工" : "存成草稿"}
        </button>
        <button type="button" className="sa-button sa-button-quiet" disabled={busy} onClick={onCancel}>
          取消
        </button>
      </div>
    </form>
  );
}
