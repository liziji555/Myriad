import { useEffect, useRef, useState, type ChangeEvent, type FormEvent } from "react";
import { api } from "../api/client";
import { importSummary, MAX_TEMPLATE_BYTES, saveTextFile } from "../api/templates";
import type { Team } from "../api/useWorkspace";
import type { Bot, Discussion, Harness, HarnessDesk, ModelTier, Room, Share } from "../api/types";

/** 一个房间里最多几个 Bot。和服务端的上限一致，超过时服务端也会拒绝。 */
const MAX_MEMBERS = 6;

const TIERS: readonly { id: ModelTier; label: string }[] = [
  { id: "cheap", label: "便宜" },
  { id: "mid", label: "中档" },
  { id: "strong", label: "强" },
];

/** Bot 由谁来干活。除了内置的，都是在跑 Myriad 的这台机器上启动对应的命令行。 */
const HARNESSES: readonly { id: Harness; label: string }[] = [
  { id: "builtin", label: "内置（直接调模型）" },
  { id: "claude_code", label: "Claude Code" },
  { id: "codex", label: "Codex" },
  { id: "open_code", label: "OpenCode" },
  { id: "command", label: "自定义命令" },
];

const harnessLabel = (harness: Harness) => HARNESSES.find((h) => h.id === harness)?.label ?? harness;

/** 多久看一次共享目录连没连着。 */
const SHARES_REFRESH_MS = 10_000;

/** 别的电脑用 `sa share` 共享过来的目录，每隔一会儿刷新一次。 */
function useShares(): Share[] {
  const [shares, setShares] = useState<Share[]>([]);
  useEffect(() => {
    let alive = true;
    const load = () =>
      api.listShares().then(
        (found) => alive && setShares(found),
        () => {},
      );
    void load();
    const timer = window.setInterval(() => void load(), SHARES_REFRESH_MS);
    return () => {
      alive = false;
      window.clearInterval(timer);
    };
  }, []);
  return shares;
}

/** 选择框里一个共享目录的样子。 */
export function shareLabel(share: Share): string {
  const state = share.online ? "在线" : "没连着";
  const mode = share.read_only ? "，只读" : share.shell ? "，能执行命令" : "";
  return `${share.name}（${state}${mode}）`;
}

/** 没点名的话怎么接。点了名的总是只给被点到的。 */
export const DISCUSSIONS: readonly { id: Discussion; label: string; note: string }[] = [
  { id: "normal", label: "普通", note: "没点名时挑一个最合适的 Bot 接" },
  { id: "parallel", label: "并行回复", note: "没点名时每个 Bot 各答一份，同时开始" },
  { id: "rounds", label: "轮流讨论", note: "没点名时按进房间的先后一个接一个说，后说的看得到先说的" },
  { id: "blind", label: "独立评议", note: "没点名时每个 Bot 各答一份，这一轮互相看不到对方的答案" },
  { id: "lead", label: "主 Agent 派活", note: "入口 Bot 当主 Agent：没点名的话交给它，它把活派给成员，成员做完自动交回给它决定下一步" },
];

/** 轮流讨论最多转几圈。和服务端的上限一致。 */
const MAX_ROUNDS = 3;

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
  const shares = useShares();
  const filePicker = useRef<HTMLInputElement>(null);
  const [desks, setDesks] = useState<readonly HarnessDesk[]>([]);
  const hasHarness = members.some((bot) => bot.harness !== "builtin");
  useEffect(() => {
    if (!hasHarness) {
      setDesks([]);
      return;
    }
    let live = true;
    api.listDesks(room.id).then(
      (found) => live && setDesks(found),
      () => {},
    );
    return () => {
      live = false;
    };
  }, [room.id, hasHarness]);
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
    if (done) {
      setImported(summary);
      // 「照模板改已有的」只管这一次：下一份模板要改得重新勾。
      setUpdateExisting(false);
    }
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
      <h3 className="sa-panel-heading">讨论方式</h3>
      <div className="sa-discussion">
        <label className="sa-field">
          <span>没点名的话</span>
          <select
            value={room.discussion}
            disabled={busy}
            onChange={(event) =>
              void run(() => team.updateRoom(room.id, { discussion: event.target.value as Discussion }))
            }
          >
            {DISCUSSIONS.map(({ id, label }) => (
              <option key={id} value={id}>
                {label}
              </option>
            ))}
          </select>
        </label>
        {room.discussion === "rounds" && (
          <label className="sa-field">
            <span>转几圈</span>
            <select
              value={room.rounds}
              disabled={busy}
              onChange={(event) => void run(() => team.updateRoom(room.id, { rounds: Number(event.target.value) }))}
            >
              {Array.from({ length: MAX_ROUNDS }, (_, index) => index + 1).map((rounds) => (
                <option key={rounds} value={rounds}>
                  {rounds} 圈
                </option>
              ))}
            </select>
          </label>
        )}
      </div>
      <p className="sa-panel-note">
        {DISCUSSIONS.find(({ id }) => id === room.discussion)?.note}。用 @ 点了名的话只给被点到的。
      </p>

      <h3 className="sa-panel-heading">工作目录</h3>
      <div className="sa-discussion">
        <label className="sa-field">
          <span>文件在哪</span>
          <select
            aria-label="工作目录"
            value={room.share ?? ""}
            disabled={busy}
            onChange={(event) => void run(() => team.updateRoom(room.id, { share: event.target.value }))}
          >
            <option value="">这台服务的工作目录</option>
            {shares.map((share) => (
              <option key={share.name} value={share.name}>
                {shareLabel(share)}
              </option>
            ))}
            {room.share && !shares.some((share) => share.name === room.share) && (
              <option value={room.share}>{room.share}（没连着）</option>
            )}
          </select>
        </label>
      </div>
      <p className="sa-panel-note">
        {room.share
          ? shares.find((share) => share.name === room.share)?.shell
            ? `Bot 看、搜、改的是共享目录「${room.share}」里的文件，执行命令、跑 harness 也在那台电脑上，每条命令、每次启动 harness 都要你批准；那台电脑没连着时会等它连上。这里还不能开任务。`
            : `Bot 看、搜、改的是共享目录「${room.share}」里的文件，在那台电脑上执行；那台电脑没连着时会等它连上。那边运行 sa share 时加了 --commands 才能执行命令、跑 harness；这里还不能开任务。`
          : "在别的电脑上运行 sa share 目录 --server 这台服务器，就能把那边的目录共享过来，在这里选它。"}
      </p>

      <h3 className="sa-panel-heading">成员</h3>
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
                {bot.harness !== "builtin" && <span className="sa-tag">{harnessLabel(bot.harness)}</span>}
                {!bot.enabled && <span className="sa-tag">已停用</span>}
              </div>
              {bot.persona && <p className="sa-member-role">{bot.persona.split("\n")[0]}</p>}
              <DeskView
                desk={desks.find((desk) => desk.bot_id === bot.id)}
                busy={busy}
                onClose={(how) => void run(async () => setDesks(await api.closeDesk(room.id, bot.id, how)))}
              />
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
        已经有同名的 Bot 时，照模板改它的档位、分工、是否启用（不勾就不动它；只管下一次导入）
      </label>
      {imported && (
        <p className="sa-panel-note" role="status">
          {imported}
        </p>
      )}
    </div>
  );
}

/** harness 成员的独立目录：改了哪些文件，带回还是丢掉。 */
function DeskView({
  desk,
  busy,
  onClose,
}: {
  desk: HarnessDesk | undefined;
  busy: boolean;
  onClose(how: "accept" | "discard"): void;
}) {
  if (!desk) return null;
  return (
    <div className="sa-member-desk" aria-label="独立目录">
      <p className="sa-member-role">
        独立目录{desk.branch ? `（分支 ${desk.branch}）` : ""}：
        {desk.changes.length === 0 ? "还没有改动" : `改了 ${desk.changes.length} 个文件`}
      </p>
      {desk.changes.length > 0 && (
        <ul className="sa-member-changes">
          {desk.changes.map((change) => (
            <li key={change.path}>
              {change.label} {change.path}
            </li>
          ))}
        </ul>
      )}
      <div className="sa-member-actions">
        <button type="button" className="sa-link" disabled={busy} onClick={() => onClose("accept")}>
          带回工作目录
        </button>
        <button type="button" className="sa-link sa-link-danger" disabled={busy} onClick={() => onClose("discard")}>
          丢掉改动
        </button>
      </div>
    </div>
  );
}

interface Draft {
  name: string;
  tier: ModelTier;
  persona: string;
  harness: Harness;
  harness_command: string;
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
  const [harness, setHarness] = useState<Harness>(bot?.harness ?? "builtin");
  const [command, setCommand] = useState(bot?.harness_command ?? "");

  const submit = (event: FormEvent) => {
    event.preventDefault();
    const harness_command = harness === "builtin" ? "" : command.trim();
    void onSave({ name: name.trim(), tier, persona, harness, harness_command, ...(bot ? { enabled } : {}) });
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
        <span>由谁来干</span>
        <select value={harness} onChange={(event) => setHarness(event.target.value as Harness)}>
          {HARNESSES.map(({ id, label }) => (
            <option key={id} value={id}>
              {label}
            </option>
          ))}
        </select>
      </label>
      {harness !== "builtin" && (
        <label className="sa-field">
          <span>{harness === "command" ? "命令" : "命令（不填就用默认的）"}</span>
          <input
            value={command}
            maxLength={300}
            required={harness === "command"}
            placeholder={harness === "command" ? "比如 python my_harness.py" : "比如 /usr/local/bin/claude"}
            onChange={(event) => setCommand(event.target.value)}
          />
        </label>
      )}
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
