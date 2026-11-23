import { useEffect, useState, type FormEvent } from "react";
import { connectionLabel, openConnect, type DesktopInfo } from "../api/desktop";
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
  /** 在桌面版里时：连的是本机还是哪台服务器。 */
  desktop: DesktopInfo | null;
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
  desktop,
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
        <BrandMark />
        <span className="sa-brand">Glimmer</span>
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
        {desktop && (
          <button
            type="button"
            className="sa-connect"
            disabled={desktop.pinned}
            title={desktop.pinned ? "启动时用 --workdir 或 --server 定死了，换不了" : "连接本机或服务器"}
            aria-label={`连接：${connectionLabel(desktop)}`}
            onClick={openConnect}
          >
            <Icon name={desktop.current?.mode === "server" ? "server" : "plug"} />
            <span className="sa-nav-label sa-connect-label">{connectionLabel(desktop)}</span>
          </button>
        )}
      </footer>
    </nav>
  );
}

/** Glimmer 的标：一个太极。 */
function BrandMark() {
  return (
    <svg className="sa-brand-mark" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
      <path transform="matrix(0.003510 0 0 -0.003510 -2.530 27.695)" d="M3530 6555 c-11 -2 -57 -10 -102 -19 -45 -9 -108 -20 -140 -26 -56 -10 -69 -12 -148 -30 -21 -5 -51 -11 -66 -14 -659 -141 -1015 -409 -1193 -896 -9 -24 -19 -58 -23 -76 -3 -18 -9 -43 -12 -56 -33 -143 -31 -146 56 -77 97 77 240 166 332 205 174 74 348 125 356 104 5 -11 2 -13 -126 -94 -65 -41 -134 -83 -286 -173 -275 -163 -537 -369 -690 -539 -19 -21 -41 -46 -50 -56 -70 -76 -126 -149 -220 -286 -68 -99 -159 -265 -206 -378 -11 -27 -30 -73 -42 -100 -80 -190 -147 -472 -171 -720 -6 -67 -8 -257 -3 -302 25 -222 68 -325 149 -352 31 -11 141 -9 193 2 145 33 217 55 329 102 68 29 227 107 269 132 12 7 45 26 72 42 63 37 205 132 258 171 130 98 280 221 366 301 23 21 52 47 64 58 12 11 26 25 32 30 5 6 27 25 48 44 21 19 39 35 40 36 1 1 19 17 40 36 21 19 46 41 56 50 241 220 519 425 756 557 52 29 166 86 194 98 13 6 43 19 66 29 206 92 488 164 724 187 65 6 312 8 357 3 21 -2 57 -6 80 -8 347 -38 721 -194 936 -391 25 -24 26 -32 4 -66 -52 -80 -108 -211 -133 -309 -14 -56 -17 -71 -28 -145 -33 -229 49 -538 205 -771 60 -89 162 -210 224 -267 20 -18 45 -40 55 -50 41 -38 86 -72 144 -111 100 -67 96 -70 96 74 0 58 2 117 4 131 2 15 6 41 8 59 7 51 27 144 40 192 13 46 22 74 71 206 30 82 44 124 53 163 10 43 19 47 94 35 22 -4 64 -8 94 -10 151 -11 162 -12 260 -24 126 -15 321 -67 426 -112 42 -18 51 -5 36 54 -82 338 -297 625 -600 801 -15 8 -39 22 -53 30 -158 91 -394 162 -601 180 -25 2 -60 5 -78 6 -44 4 -48 7 -72 61 -26 59 -83 176 -100 204 -7 12 -20 35 -29 50 -99 174 -170 275 -308 442 -179 215 -397 403 -662 568 -62 39 -241 131 -306 157 -23 10 -65 27 -93 39 -47 19 -71 27 -193 64 -130 39 -297 74 -427 88 -169 19 -427 19 -592 0 -72 -8 -100 -12 -160 -22 -142 -24 -159 -16 -66 31 78 39 167 132 215 222 24 47 19 49 -63 33 -126 -25 -144 -17 -60 27 137 72 274 203 331 317 29 59 24 70 -31 59z m222 -1715 c32 -32 -113 -179 -242 -245 -81 -41 -72 -59 24 -46 68 10 41 -40 -55 -104 -85 -56 -191 -78 -295 -63 -38 6 -61 10 -176 34 -120 25 -305 -65 -499 -245 -70 -65 -42 19 57 169 131 197 339 332 625 404 75 19 181 40 307 62 46 8 116 21 156 28 87 16 88 16 98 6z" />
    </svg>
  );
}
