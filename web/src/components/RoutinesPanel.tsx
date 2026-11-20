import { useState, type FormEvent } from "react";
import { describeSchedule, formatWhen, lastRun, SCHEDULE_PRESETS } from "../api/routines";
import type { RoutineActions } from "../api/useWorkspace";
import type { Bot, BotId, Room, Routine } from "../api/types";

interface Props {
  room: Room;
  /** 房间里启用着的 Bot。 */
  members: readonly Bot[];
  bots: readonly Bot[];
  /** 这个房间的定时任务。 */
  routines: readonly Routine[];
  actions: RoutineActions;
}

type Editing = Routine | "new" | null;

/** 右栏的「定时」页：到点让一个 Bot 在这个房间里做一件事。 */
export function RoutinesPanel({ room, members, bots, routines, actions }: Props) {
  const [editing, setEditing] = useState<Editing>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  /** 刚点过「现在就做」的那个：结果要等一会儿才来，先说一声。 */
  const [started, setStarted] = useState<string | null>(null);

  const run = async (action: () => Promise<unknown>) => {
    setError(null);
    setBusy(true);
    try {
      await action();
      return true;
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
      return false;
    } finally {
      setBusy(false);
    }
  };

  if (editing) {
    return (
      <RoutineForm
        routine={editing === "new" ? null : editing}
        members={members}
        defaultBot={room.host_bot_id}
        busy={busy}
        error={error}
        onCancel={() => {
          setError(null);
          setEditing(null);
        }}
        onSave={async (draft) => {
          const saved = await run(() =>
            editing === "new"
              ? actions.create({ room_id: room.id, ...draft })
              : actions.update(editing.id, draft),
          );
          if (saved) setEditing(null);
        }}
      />
    );
  }

  return (
    <div className="sa-members">
      {error && (
        <p className="sa-form-error" role="alert">
          {error}
        </p>
      )}
      {routines.length === 0 ? (
        <p className="sa-panel-empty">
          还没有定时任务。到点之后负责的 Bot 会在这个房间里做一轮；没什么可说时它不出声。
        </p>
      ) : (
        <ul className="sa-list" aria-label="定时任务">
          {routines.map((routine) => {
            const bot = bots.find((b) => b.id === routine.bot_id);
            const last = lastRun(routine);
            return (
              <li key={routine.id} className="sa-member" data-enabled={routine.enabled}>
                <div className="sa-row sa-row-static">
                  <span className="sa-dot" data-state={routine.enabled ? "idle" : "off"} aria-hidden="true" />
                  <span className="sa-row-label">{routine.title}</span>
                  {!routine.enabled && <span className="sa-tag">已停用</span>}
                </div>
                <p className="sa-member-role" title={routine.schedule}>
                  {describeSchedule(routine.schedule)} · {bot?.name ?? "Bot"}
                  {routine.enabled && routine.next_at !== null && ` · 下一次 ${formatWhen(routine.next_at)}`}
                </p>
                {last && (
                  <p className="sa-routine-last" data-result={routine.last_result}>
                    {last}
                  </p>
                )}
                {started === routine.id && (
                  <p className="sa-routine-last" role="status">
                    已经开始做了，结果一会儿显示在这里。
                  </p>
                )}
                <div className="sa-member-actions">
                  <button
                    type="button"
                    className="sa-link"
                    disabled={busy}
                    onClick={async () => {
                      setStarted(null);
                      if (await run(() => actions.runNow(routine.id))) setStarted(routine.id);
                    }}
                  >
                    现在就做
                  </button>
                  <button
                    type="button"
                    className="sa-link"
                    disabled={busy}
                    onClick={() => void run(() => actions.update(routine.id, { enabled: !routine.enabled }))}
                  >
                    {routine.enabled ? "停用" : "启用"}
                  </button>
                  <button type="button" className="sa-link" disabled={busy} onClick={() => setEditing(routine)}>
                    编辑
                  </button>
                  <button
                    type="button"
                    className="sa-link sa-link-danger"
                    disabled={busy}
                    onClick={() => {
                      if (window.confirm(`删除定时任务「${routine.title}」？`)) {
                        void run(() => actions.remove(routine.id));
                      }
                    }}
                  >
                    删除
                  </button>
                </div>
              </li>
            );
          })}
        </ul>
      )}
      <button
        type="button"
        className="sa-button sa-button-quiet"
        disabled={members.length === 0}
        title={members.length === 0 ? "这个房间里还没有能干活的 Bot" : undefined}
        onClick={() => {
          setStarted(null);
          setEditing("new");
        }}
      >
        新建定时任务
      </button>
    </div>
  );
}

interface Draft {
  bot_id: BotId;
  title: string;
  prompt: string;
  schedule: string;
}

interface FormProps {
  routine: Routine | null;
  members: readonly Bot[];
  defaultBot: BotId | null;
  busy: boolean;
  error: string | null;
  onSave(draft: Draft): Promise<void>;
  onCancel(): void;
}

function RoutineForm({ routine, members, defaultBot, busy, error, onSave, onCancel }: FormProps) {
  const [title, setTitle] = useState(routine?.title ?? "");
  const [prompt, setPrompt] = useState(routine?.prompt ?? "");
  const [schedule, setSchedule] = useState(routine?.schedule ?? "0 9 * * *");
  const fallback = members.find((bot) => bot.id === defaultBot)?.id ?? members[0]?.id ?? "";
  const [botId, setBotId] = useState<BotId>(routine?.bot_id ?? fallback);
  // 原来负责的 Bot 已经不在房间里了：下拉框里没有它，保存前要另选一个。
  const known = members.some((bot) => bot.id === botId);
  const ready = title.trim() !== "" && prompt.trim() !== "" && schedule.trim() !== "" && known;
  const plain = describeSchedule(schedule);

  const submit = (event: FormEvent) => {
    event.preventDefault();
    void onSave({ bot_id: botId, title: title.trim(), prompt: prompt.trim(), schedule: schedule.trim() });
  };

  return (
    <form className="sa-form" onSubmit={submit} aria-label={routine ? `编辑 ${routine.title}` : "新建定时任务"}>
      <label className="sa-field">
        <span>名字</span>
        <input
          value={title}
          maxLength={40}
          required
          autoFocus
          placeholder="比如 早报"
          onChange={(event) => setTitle(event.target.value)}
        />
      </label>
      <label className="sa-field">
        <span>由谁做</span>
        <select value={known ? botId : ""} onChange={(event) => setBotId(event.target.value)}>
          {!known && <option value="">请选择</option>}
          {members.map((bot) => (
            <option key={bot.id} value={bot.id}>
              {bot.name}
            </option>
          ))}
        </select>
      </label>
      <label className="sa-field">
        <span>什么时候做（分 时 日 月 周，按本机时间）</span>
        <input
          value={schedule}
          required
          spellCheck={false}
          placeholder="0 9 * * *"
          onChange={(event) => setSchedule(event.target.value)}
        />
      </label>
      <div className="sa-presets" role="group" aria-label="常用的时间">
        {SCHEDULE_PRESETS.map((preset) => (
          <button
            key={preset.schedule}
            type="button"
            className="sa-chip"
            aria-pressed={schedule.trim() === preset.schedule}
            onClick={() => setSchedule(preset.schedule)}
          >
            {preset.label}
          </button>
        ))}
      </div>
      {plain !== schedule && <p className="sa-routine-last">也就是：{plain}</p>}
      <label className="sa-field">
        <span>到点时让它做什么</span>
        <textarea
          rows={6}
          value={prompt}
          required
          placeholder="比如：看看仓库里有没有新的 issue，有就总结一下；没有就不用说话"
          onChange={(event) => setPrompt(event.target.value)}
        />
      </label>
      {error && (
        <p className="sa-form-error" role="alert">
          {error}
        </p>
      )}
      <div className="sa-form-actions">
        <button type="submit" className="sa-button" disabled={busy || !ready}>
          {busy ? "保存中…" : "保存"}
        </button>
        <button type="button" className="sa-button sa-button-quiet" disabled={busy} onClick={onCancel}>
          取消
        </button>
      </div>
    </form>
  );
}
