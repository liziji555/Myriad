import { useEffect, useState, type FormEvent } from "react";
import type { Presence, PresenceState } from "../api/presence";
import type { Bot, Room, RoomId } from "../api/types";
import { BotAvatar } from "./BotAvatar";
import type { PanelTab } from "./ContextPanel";
import { Icon, type IconName } from "./Icon";

/** 房间此刻的样子：有 Bot 在忙、有事等人，还是空着。 */
export type RoomState = "busy" | "warn" | "idle";

/** 左栏里能直接打开的面板页。 */
const NAV: readonly { tab: PanelTab; label: string; icon: IconName }[] = [
  { tab: "execution", label: "执行", icon: "graph" },
  { tab: "tasks", label: "任务", icon: "tasks" },
  { tab: "routines", label: "定时任务", icon: "clock" },
  { tab: "members", label: "队员和成员", icon: "users" },
  { tab: "links", label: "连接", icon: "link" },
  { tab: "extensions", label: "技能和 MCP", icon: "plug" },
  { tab: "files", label: "文件", icon: "file" },
  { tab: "usage", label: "用量", icon: "chart" },
  { tab: "audit", label: "审计", icon: "shield" },
];

/** 队员名字后面那几个字：一眼看出它在不在忙。 */
const SHORT: Record<PresenceState, string> = {
  approval: "等你批准",
  asking: "等你",
  tool: "在做事",
  writing: "在回复",
  working: "做任务",
  queued: "排队",
  idle: "空闲",
  off: "已停用",
};

interface Props {
  rooms: readonly Room[];
  bots: readonly Bot[];
  selectedRoomId: RoomId | null;
  roomStates: Readonly<Record<RoomId, RoomState>>;
  /** 当前房间里每个 Bot 在做什么。不在这个房间的 Bot 没有。 */
  presence: readonly Presence[];
  connected: boolean;
  version: string;
  /** 收成一条只有图标的窄条。 */
  collapsed: boolean;
  /** 窗口窄的时候左栏是盖在主线上的抽屉，没有收起这一说。 */
  docked: boolean;
  onToggleCollapsed(): void;
  /** 正开着的面板页，没开时是 null。 */
  activeTab: PanelTab | null;
  onShowPanel(tab: PanelTab): void;
  onPickBot(row: Presence): void;
  onSelect(id: RoomId): void;
  onCreateRoom(name: string): Promise<void>;
  /** 每加一就打开「新建房间」的输入框（命令面板里点的）。 */
  createSignal: number;
}

export function Sidebar({
  rooms,
  bots,
  selectedRoomId,
  roomStates,
  presence,
  connected,
  version,
  collapsed,
  docked,
  onToggleCollapsed,
  activeTab,
  onShowPanel,
  onPickBot,
  onSelect,
  onCreateRoom,
  createSignal,
}: Props) {
  const [creating, setCreating] = useState(false);
  useEffect(() => {
    if (createSignal > 0) setCreating(true);
  }, [createSignal]);
  const [name, setName] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const narrow = docked && collapsed;

  const close = () => {
    setCreating(false);
    setName("");
    setError("");
  };

  const startCreating = () => {
    if (narrow) onToggleCollapsed();
    setCreating(true);
  };

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (busy) return;
    setBusy(true);
    try {
      await onCreateRoom(name);
      close();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "新建失败");
    } finally {
      setBusy(false);
    }
  };

  return (
    <nav className="sa-sidebar" aria-label="左栏" data-collapsed={narrow}>
      <header className="sa-sidebar-head">
        <span className="sa-brand-mark" aria-hidden="true" />
        <span className="sa-brand">苦乐</span>
        {docked && (
          <button
            type="button"
            className="sa-icon-button sa-sidebar-toggle"
            aria-label={collapsed ? "展开左栏" : "收起左栏"}
            title={collapsed ? "展开左栏" : "收起左栏"}
            onClick={onToggleCollapsed}
          >
            <Icon name="sidebar" />
          </button>
        )}
      </header>

      <div className="sa-sidebar-body">
        <button type="button" className="sa-new-room-button" title="新建房间" onClick={startCreating}>
          <Icon name="plus" />
          <span className="sa-nav-label">新建房间</span>
        </button>

        <section className="sa-section" aria-label="房间">
          <h2 className="sa-section-title">房间</h2>
          {creating && !narrow && (
            <form className="sa-new-room" onSubmit={submit}>
              <input
                autoFocus
                value={name}
                maxLength={60}
                placeholder="房间名，回车确认"
                aria-label="房间名"
                aria-invalid={error !== ""}
                onChange={(event) => {
                  setName(event.target.value);
                  setError("");
                }}
                onKeyDown={(event) => event.key === "Escape" && close()}
              />
              {error && (
                <p className="sa-field-error" role="alert">
                  {error}
                </p>
              )}
            </form>
          )}
          <ul className="sa-list">
            {rooms.map((room) => (
              <li key={room.id}>
                <button
                  type="button"
                  className="sa-row"
                  aria-current={room.id === selectedRoomId ? "page" : undefined}
                  title={room.name}
                  onClick={() => onSelect(room.id)}
                >
                  <span className="sa-room-mark" aria-hidden="true">
                    {[...room.name.trim()][0] ?? "·"}
                    <span className="sa-dot" data-state={roomStates[room.id] ?? "idle"} />
                  </span>
                  <span className="sa-row-label">{room.name}</span>
                </button>
              </li>
            ))}
          </ul>
        </section>

        <section className="sa-section" aria-label="队员">
          <h2 className="sa-section-title">队员</h2>
          <ul className="sa-list">
            {bots.map((bot) => {
              const row = presence.find((candidate) => candidate.botId === bot.id);
              const state: PresenceState = row?.state ?? (bot.enabled ? "idle" : "off");
              const meta = row ? SHORT[state] : bot.enabled ? "不在这个房间" : "已停用";
              const label = row ? `${bot.name}：${row.text}` : `${bot.name}：${meta}`;
              return (
                <li key={bot.id}>
                  <button
                    type="button"
                    className="sa-row"
                    data-state={state}
                    title={label}
                    aria-label={label}
                    onClick={() => (row && (row.messageId || row.taskId) ? onPickBot(row) : onShowPanel("members"))}
                  >
                    <BotAvatar
                      id={bot.id}
                      name={bot.name}
                      state={row && state !== "idle" ? state : bot.enabled ? undefined : "off"}
                      className="sa-row-avatar"
                    />
                    <span className="sa-row-label">{bot.name}</span>
                    <span className="sa-row-meta">{meta}</span>
                  </button>
                </li>
              );
            })}
          </ul>
        </section>

        <section className="sa-section" aria-label="工作台">
          <h2 className="sa-section-title">工作台</h2>
          <ul className="sa-list">
            {NAV.map((item) => (
              <li key={item.tab}>
                <button
                  type="button"
                  className="sa-row sa-nav-row"
                  aria-pressed={activeTab === item.tab}
                  disabled={selectedRoomId === null}
                  title={item.label}
                  onClick={() => onShowPanel(item.tab)}
                >
                  <Icon name={item.icon} />
                  <span className="sa-row-label">{item.label}</span>
                </button>
              </li>
            ))}
          </ul>
        </section>
      </div>

      <footer className="sa-sidebar-foot" role="status" title={connected ? "已连接" : "连接中断，正在重连…"}>
        <span className="sa-dot" data-state={connected ? "idle" : "warn"} aria-hidden="true" />
        <span className="sa-nav-label">{connected ? "已连接" : "连接中断，正在重连…"}</span>
        {version && <span className="sa-row-meta sa-nav-label">v{version}</span>}
      </footer>
    </nav>
  );
}
