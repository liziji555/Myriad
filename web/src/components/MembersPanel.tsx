import { useRef, useState, type ChangeEvent, type FormEvent } from "react";
import { api } from "../api/client";
import { importSummary, MAX_TEMPLATE_BYTES, saveTextFile } from "../api/templates";
import type { Team } from "../api/useWorkspace";
import type { Bot, ModelTier, Room } from "../api/types";

/** 一个房间里最多几个 Bot。和服务端的上限一致，超过时服务端也会拒绝。 */
const MAX_MEMBERS = 6;

const TIERS: readonly { id: ModelTier; label: string }[] = [
  { id: "cheap", label: "便宜" },
  { id: "mid", label: "中档" },
  { id: "strong", label: "强" },
];

interface Props {
  room: Room;
  /** 房间里的 Bot，按进房间的先后。 */
  members: readonly Bot[];
  /** 所有 Bot。 */
  bots: readonly Bot[];
  team: Team;
}

/** 正在编辑哪个 Bot；`"new"` 是新建。 */
type Editing = Bot | "new" | null;

export function MembersPanel({ room, members, bots, team }: Props) {
  const [editing, setEditing] = useState<Editing>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [imported, setImported] = useState<string | null>(null);
  const [updateExisting, setUpdateExisting] = useState(false);
  const filePicker = useRef<HTMLInputElement>(null);
  const outside = bots.filter((bot) => !room.member_bot_ids.includes(bot.id));
  const full = members.length >= MAX_MEMBERS;

  const run = async (action: () => Promise<unknown>) => {
    setError(null);
    setImported(null);
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

  const exportTeam = () =>
    run(async () => {
      const file = await api.exportRoom(room.id);
      saveTextFile(file.name, file.text);
    });

  const importFile = async (event: ChangeEvent<HTMLInputElement>) => {
    const file = event.target.files?.[0];
    // 清掉选择：同一个文件改完再选一次也要触发。
    event.target.value = "";
    if (!file) return;
    let summary = "";
    const done = await run(async () => {
      if (file.size > MAX_TEMPLATE_BYTES) throw new Error("模板文件太大了（超过 1 MB）");
      const text = await file.text();
      summary = importSummary(await api.importTemplate({ text, update_existing: updateExisting }));
    });
    if (done) setImported(summary);
  };

  if (editing) {
    return (
      <BotForm
        bot={editing === "new" ? null : editing}
        busy={busy}
        error={error}
        onCancel={() => {
          setError(null);
          setEditing(null);
        }}
        onSave={async (draft) => {
          const saved = await run(async () => {
            if (editing === "new") {
              const bot = await team.createBot(draft);
              // 在这个房间里新建的，顺手拉进来；房间满了就只建不拉。
              if (!full) await team.addMember(room.id, bot.id);
            } else {
              await team.updateBot(editing.id, draft);
            }
          });
          if (saved) setEditing(null);
        }}
        onDelete={
          editing === "new"
            ? undefined
            : async () => {
                if (!window.confirm(`删除 ${editing.name}？它发过的消息会留着。`)) return;
                if (await run(() => team.deleteBot(editing.id))) setEditing(null);
              }
        }
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
      {members.length === 0 ? (
        <p className="sa-panel-empty">这个房间还没有 Bot。</p>
      ) : (
        <ul className="sa-list" aria-label="房间里的 Bot">
          {members.map((bot) => (
            <li key={bot.id} className="sa-member">
              <div className="sa-row sa-row-static">
                <span className="sa-dot" data-state={bot.enabled ? "idle" : "off"} aria-hidden="true" />
                <span className="sa-row-label">{bot.name}</span>
                {bot.id === room.host_bot_id && <span className="sa-tag">入口</span>}
                {!bot.enabled && <span className="sa-tag">已停用</span>}
              </div>
              {bot.persona && <p className="sa-member-role">{bot.persona.split("\n")[0]}</p>}
              <div className="sa-member-actions">
                {bot.id !== room.host_bot_id && (
                  <button
                    type="button"
                    className="sa-link"
                    disabled={busy}
                    title="没有点名、也挑不出合适的 Bot 时，由入口 Bot 接话"
                    onClick={() => void run(() => team.updateRoom(room.id, { host_bot_id: bot.id }))}
                  >
                    设为入口
                  </button>
                )}
                <button type="button" className="sa-link" disabled={busy} onClick={() => setEditing(bot)}>
                  编辑
                </button>
                <button
                  type="button"
                  className="sa-link"
                  disabled={busy}
                  onClick={() => void run(() => team.removeMember(room.id, bot.id))}
                >
                  请出房间
                </button>
              </div>
            </li>
          ))}
        </ul>
      )}

      {outside.length > 0 && (
        <>
          <h3 className="sa-panel-heading">不在这个房间的 Bot</h3>
          <ul className="sa-list" aria-label="不在这个房间的 Bot">
            {outside.map((bot) => (
              <li key={bot.id} className="sa-member">
                <div className="sa-row sa-row-static">
                  <span className="sa-dot" data-state={bot.enabled ? "idle" : "off"} aria-hidden="true" />
                  <span className="sa-row-label">{bot.name}</span>
                </div>
                <div className="sa-member-actions">
                  <button
                    type="button"
                    className="sa-link"
                    disabled={busy || full}
                    title={full ? `一个房间最多 ${MAX_MEMBERS} 个 Bot` : undefined}
                    onClick={() => void run(() => team.addMember(room.id, bot.id))}
                  >
                    拉进房间
                  </button>
                  <button type="button" className="sa-link" disabled={busy} onClick={() => setEditing(bot)}>
                    编辑
                  </button>
                </div>
              </li>
            ))}
          </ul>
        </>
      )}

      <button type="button" className="sa-button sa-button-quiet" onClick={() => setEditing("new")}>
        新建 Bot
      </button>

      <h3 className="sa-panel-heading">模板</h3>
      <p className="sa-panel-note">
        把这个房间的 Bot 存成一个文件，换一台机器或者另开一个房间时导入。文件里只有名字、档位、分工，没有密钥和聊天记录。
      </p>
      <div className="sa-member-actions">
        <button
          type="button"
          className="sa-link"
          disabled={busy || members.length === 0}
          onClick={() => void exportTeam()}
        >
          导出这个房间的团队
        </button>
        <button
          type="button"
          className="sa-link"
          disabled={busy}
          title="新建模板里的 Bot；模板带了房间就新建那个房间"
          onClick={() => filePicker.current?.click()}
        >
          导入模板
        </button>
        <input
          ref={filePicker}
          type="file"
          accept=".toml,.json,text/plain,application/json"
          hidden
          aria-label="选择模板文件"
          onChange={(event) => void importFile(event)}
        />
      </div>
      <label className="sa-panel-note sa-check">
        <input
          type="checkbox"
          checked={updateExisting}
          disabled={busy}
          onChange={(event) => setUpdateExisting(event.target.checked)}
        />
        已经有同名的 Bot 时，照模板改它的档位、分工、是否启用（不勾就不动它）
      </label>
      {imported && (
        <p className="sa-panel-note" role="status">
          {imported}
        </p>
      )}
    </div>
  );
}

interface Draft {
  name: string;
  tier: ModelTier;
  persona: string;
  enabled?: boolean;
}

interface FormProps {
  /** 要改的 Bot；新建时为空。 */
  bot: Bot | null;
  busy: boolean;
  error: string | null;
  onSave(draft: Draft): Promise<void>;
  onCancel(): void;
  onDelete?(): Promise<void>;
}

function BotForm({ bot, busy, error, onSave, onCancel, onDelete }: FormProps) {
  const [name, setName] = useState(bot?.name ?? "");
  const [tier, setTier] = useState<ModelTier>(bot?.tier ?? "mid");
  const [persona, setPersona] = useState(bot?.persona ?? "");
  const [enabled, setEnabled] = useState(bot?.enabled ?? true);

  const submit = (event: FormEvent) => {
    event.preventDefault();
    void onSave({ name: name.trim(), tier, persona, ...(bot ? { enabled } : {}) });
  };

  return (
    <form className="sa-form" onSubmit={submit} aria-label={bot ? `编辑 ${bot.name}` : "新建 Bot"}>
      <label className="sa-field">
        <span>名字</span>
        <input
          value={name}
          maxLength={32}
          required
          autoFocus
          placeholder="比如 Coder。用来 @ 它，不能有空格"
          onChange={(event) => setName(event.target.value)}
        />
      </label>
      <label className="sa-field">
        <span>模型档位</span>
        <select value={tier} onChange={(event) => setTier(event.target.value as ModelTier)}>
          {TIERS.map(({ id, label }) => (
            <option key={id} value={id}>
              {label}
            </option>
          ))}
        </select>
      </label>
      <label className="sa-field">
        <span>分工和人设</span>
        <textarea
          rows={6}
          value={persona}
          placeholder="第一行写它负责什么，别的 Bot 和路由靠这一行决定找不找它"
          onChange={(event) => setPersona(event.target.value)}
        />
      </label>
      {bot && (
        <label className="sa-check">
          <input type="checkbox" checked={enabled} onChange={(event) => setEnabled(event.target.checked)} />
          <span>启用（停用后不会被点到，也不接话）</span>
        </label>
      )}
      {error && (
        <p className="sa-form-error" role="alert">
          {error}
        </p>
      )}
      <div className="sa-form-actions">
        <button type="submit" className="sa-button" disabled={busy || name.trim() === ""}>
          {busy ? "保存中…" : "保存"}
        </button>
        <button type="button" className="sa-button sa-button-quiet" disabled={busy} onClick={onCancel}>
          取消
        </button>
        {onDelete && (
          <button type="button" className="sa-link sa-link-danger" disabled={busy} onClick={() => void onDelete()}>
            删除
          </button>
        )}
      </div>
    </form>
  );
}
