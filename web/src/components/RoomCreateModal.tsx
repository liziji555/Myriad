import { useEffect, useState, type FormEvent } from "react";
import type { Bot, BotId, CreateRoomRequest, Discussion, Room } from "../api/types";
import { BotAvatar } from "./BotAvatar";
import { DISCUSSIONS } from "./MembersPanel";

const MAX_ROOM_MEMBERS = 6;

interface Props {
  bots: readonly Bot[];
  defaultMasterId: BotId | null;
  onCreate(body: CreateRoomRequest): Promise<Room>;
  onOpenNewBot(): void;
  onClose(): void;
}

/** 组建多 Bot 协作房间弹窗：先挑选已有 Bot 与主控，再建房间。 */
export function RoomCreateModal({ bots, defaultMasterId, onCreate, onOpenNewBot, onClose }: Props) {
  const enabledBots = bots.filter((b) => b.enabled);
  const initialSelected = enabledBots.slice(0, Math.min(2, enabledBots.length)).map((b) => b.id);
  const [name, setName] = useState("");
  const [selectedIds, setSelectedIds] = useState<BotId[]>(initialSelected);
  const [hostId, setHostId] = useState<BotId | null>(
    defaultMasterId && initialSelected.includes(defaultMasterId)
      ? defaultMasterId
      : (initialSelected[0] ?? null),
  );
  const [discussion, setDiscussion] = useState<Discussion>(
    initialSelected.length >= 2 ? "lead" : "normal",
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !busy) onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [busy, onClose]);

  const toggleMember = (id: BotId) => {
    setSelectedIds((prev) => {
      if (prev.includes(id)) {
        const next = prev.filter((item) => item !== id);
        if (hostId === id) setHostId(next[0] ?? null);
        return next;
      }
      if (prev.length >= MAX_ROOM_MEMBERS) return prev;
      const next = [...prev, id];
      if (!hostId) setHostId(id);
      if (next.length >= 2 && discussion === "normal") setDiscussion("lead");
      return next;
    });
  };

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (busy) return;
    if (!name.trim()) {
      setError("请填写协作房间名称。");
      return;
    }
    if (selectedIds.length === 0) {
      setError("请至少选择 1 个 Bot 加入房间。");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await onCreate({
        name: name.trim(),
        member_bot_ids: selectedIds,
        host_bot_id: hostId ?? selectedIds[0],
        discussion,
      });
      onClose();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div
      className="sa-palette-backdrop"
      onMouseDown={(event) => event.target === event.currentTarget && !busy && onClose()}
    >
      <div className="sa-connect-panel sa-create-modal" role="dialog" aria-modal="true" aria-label="组建协作房间">
        <header className="sa-connect-head">
          <h2>组建协作房间</h2>
          <button type="button" className="sa-link" disabled={busy} onClick={onClose}>
            关闭
          </button>
        </header>

        <form className="sa-form" onSubmit={submit}>
          <label className="sa-field">
            房间名称
            <input
              autoFocus
              maxLength={60}
              value={name}
              placeholder="例如：前端重构组、全栈开发室、代码审查组"
              onChange={(e) => setName(e.target.value)}
            />
          </label>

          <div className="sa-field">
            <div className="sa-room-picker-head">
              <span>选择加入房间的 Bot（{selectedIds.length}/{MAX_ROOM_MEMBERS}）</span>
              <button
                type="button"
                className="sa-link"
                onClick={() => {
                  onClose();
                  onOpenNewBot();
                }}
              >
                + 先去新建 Bot
              </button>
            </div>
            {enabledBots.length === 0 ? (
              <p className="sa-links-hint">还没有可用 Bot，请先创建一个 Bot。</p>
            ) : (
              <ul className="sa-bot-checklist">
                {enabledBots.map((bot) => {
                  const checked = selectedIds.includes(bot.id);
                  return (
                    <li key={bot.id}>
                      <label className="sa-bot-check-row" data-checked={checked || undefined}>
                        <input
                          type="checkbox"
                          checked={checked}
                          onChange={() => toggleMember(bot.id)}
                        />
                        <BotAvatar id={bot.id} name={bot.name} className="sa-row-avatar" />
                        <span className="sa-bot-check-info">
                          <strong>{bot.name}</strong>
                          <small>{bot.persona.split("\n")[0] || "通用助手"}</small>
                        </span>
                        <span className="sa-tag">{bot.harness === "builtin" ? "内置" : bot.harness}</span>
                      </label>
                    </li>
                  );
                })}
              </ul>
            )}
          </div>

          {selectedIds.length >= 2 && (
            <div className="sa-create-row">
              <label className="sa-field">
                房间主控（负责统筹派活）
                <select
                  value={hostId ?? selectedIds[0] ?? ""}
                  onChange={(e) => setHostId(e.target.value)}
                >
                  {selectedIds.map((id) => {
                    const b = enabledBots.find((item) => item.id === id);
                    return (
                      <option key={id} value={id}>
                        {b?.name ?? id}
                      </option>
                    );
                  })}
                </select>
              </label>

              <label className="sa-field">
                协作模式
                <select value={discussion} onChange={(e) => setDiscussion(e.target.value as Discussion)}>
                  {DISCUSSIONS.map((item) => (
                    <option key={item.id} value={item.id}>
                      {item.label}
                    </option>
                  ))}
                </select>
              </label>
            </div>
          )}

          {error && (
            <p className="sa-form-error" role="alert">
              {error}
            </p>
          )}

          <div className="sa-connect-actions">
            <button type="button" className="sa-button sa-button-quiet" disabled={busy} onClick={onClose}>
              取消
            </button>
            <button type="submit" className="sa-button" disabled={busy || selectedIds.length === 0}>
              {busy ? "组建中…" : "组建房间"}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}
