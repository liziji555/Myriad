import { connectionLabel, type DesktopState } from "../api/desktop";
import type { Presence, PresenceState } from "../api/presence";
import type { Bot, BotId, Harness, Room, RoomId } from "../api/types";
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
  { tab: "members", label: "Bot 与成员管理", icon: "users" },
  { tab: "files", label: "文件与 Diff", icon: "file" },
  { tab: "links", label: "连接", icon: "link" },
  { tab: "models", label: "模型", icon: "bot" },
  { tab: "extensions", label: "技能和 MCP", icon: "plug" },
  { tab: "usage", label: "用量", icon: "chart" },
  { tab: "audit", label: "审计", icon: "shield" },
];

const SHORT_STATE: Record<PresenceState, string> = {
  approval: "等你批准",
  asking: "等你",
  tool: "在做事",
  writing: "在回复",
  working: "做任务",
  queued: "排队",
  idle: "空闲",
  off: "已停用",
};

const SHORT_HARNESS: Record<Harness, string> = {
  builtin: "内置",
  claude_code: "Claude",
  codex: "Codex",
  open_code: "OpenCode",
  gemini: "Gemini",
  kimi: "Kimi",
  acp: "ACP",
  command: "CLI",
};

interface Props {
  /** 主控 Bot（固定置顶，可直接对话或让它创建子 Bot）。 */
  masterBot: Bot | null;
  /** 1 对 1 专属 Bot 映射（用于判断当前是否正与某个 Bot 私聊）。 */
  directRoomOf(botId: BotId): Room | undefined;
  /** 多 Bot 协作房间（非单 Bot 1v1 私聊的房间）。 */
  groupRooms: readonly Room[];
  bots: readonly Bot[];
  selectedRoomId: RoomId | null;
  roomStates: Readonly<Record<RoomId, RoomState>>;
  presence: readonly Presence[];
  connected: boolean;
  version: string;
  collapsed: boolean;
  docked: boolean;
  onToggleCollapsed(): void;
  activeTab: PanelTab | null;
  onShowPanel(tab: PanelTab): void;
  onOpenBotChat(bot: Bot): void;
  onSelectRoom(id: RoomId): void;
  onNewBot(): void;
  onNewRoom(): void;
  desktop: DesktopState | null;
  onConnect(): void;
  updateAvailable: boolean;
  onUpdate(): void;
}

/**
 * 借鉴 ProkBot / GrokBot 的 Bot-First 左侧栏结构：
 * 1. 置顶「主控」中枢卡片（直接对话，或让主控用 create_bot 自主组建团队）；
 * 2. 快捷操作「+ 新建 Bot」「+ 组建房间」；
 * 3. 「我的 Bot」独立列表（点任意 Bot 直接 1v1 对话，不强迫先建房间）；
 * 4. 「协作房间」多 Bot 群聊列表；
 * 5. 「工作台」工具导航。
 */
export function Sidebar({
  masterBot,
  directRoomOf,
  groupRooms,
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
  onOpenBotChat,
  onSelectRoom,
  onNewBot,
  onNewRoom,
  desktop,
  onConnect,
  updateAvailable,
  onUpdate,
}: Props) {
  const narrow = docked && collapsed;
  const subBots = bots.filter((bot) => !masterBot || bot.id !== masterBot.id);
  const masterDirect = masterBot ? directRoomOf(masterBot.id) : undefined;
  const masterSelected = Boolean(masterDirect && masterDirect.id === selectedRoomId);
  const masterPresence = masterBot ? presence.find((p) => p.botId === masterBot.id) : undefined;
  const masterState: PresenceState = masterPresence?.state ?? (masterBot?.enabled ? "idle" : "off");

  return (
    <nav className="sa-sidebar" aria-label="左栏" data-collapsed={narrow}>
      <header className="sa-sidebar-head">
        <BrandMark />
        <span className="sa-brand">Myriad</span>
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
        {/* 1. 置顶「主控」入口 */}
        {masterBot && (
          <div className="sa-master-wrap">
            <button
              type="button"
              className="sa-master-card"
              aria-current={masterSelected ? "page" : undefined}
              data-state={masterState}
              title={`主控「${masterBot.name}」：点击进入主控对话，可直接指挥或让它为你创建子 Bot`}
              onClick={() => onOpenBotChat(masterBot)}
            >
              <BotAvatar
                id={masterBot.id}
                name={masterBot.name}
                state={masterPresence && masterState !== "idle" ? masterState : undefined}
                className="sa-master-avatar"
              />
              <span className="sa-master-info sa-nav-label">
                <span className="sa-master-top">
                  <strong>{masterBot.name}</strong>
                  <em className="sa-master-badge">主控</em>
                  <span className="sa-row-meta">{SHORT_STATE[masterState]}</span>
                </span>
                <small>主控中枢 · 可自主创建并调度 Bot</small>
              </span>
            </button>
          </div>
        )}

        {/* 2. 两个明确的一级动作：先建 Bot，再按需组建多 Bot 协作房间 */}
        <div className="sa-side-actions">
          <button type="button" className="sa-side-action-btn" data-primary="true" title="新建独立 Bot" onClick={onNewBot}>
            <Icon name="plus" size={14} />
            <span className="sa-nav-label">新建 Bot</span>
          </button>
          <button type="button" className="sa-side-action-btn" title="挑选多个 Bot 组建协作房间" onClick={onNewRoom}>
            <Icon name="users" size={14} />
            <span className="sa-nav-label">组建房间</span>
          </button>
        </div>

        {/* 3. 我的 Bot（1v1 直聊列表，由自己或主控创建） */}
        <section className="sa-section" aria-label="我的 Bot">
          <div className="sa-section-head">
            <h2 className="sa-section-title">我的 Bot</h2>
            <span className="sa-section-count sa-nav-label">{subBots.length}</span>
          </div>
          {subBots.length === 0 ? (
            <p className="sa-section-empty sa-nav-label">
              点「新建 Bot」，或在主控对话里让它帮你创建专职子 Bot。
            </p>
          ) : (
            <ul className="sa-list">
              {subBots.map((bot) => {
                const dm = directRoomOf(bot.id);
                const active = Boolean(dm && dm.id === selectedRoomId);
                const row = presence.find((candidate) => candidate.botId === bot.id);
                const state: PresenceState = row?.state ?? (bot.enabled ? "idle" : "off");
                const dmState = dm ? roomStates[dm.id] : undefined;
                return (
                  <li key={bot.id}>
                    <button
                      type="button"
                      className="sa-row"
                      aria-current={active ? "page" : undefined}
                      data-state={state}
                      title={`${bot.name}（${SHORT_HARNESS[bot.harness]}）· 点击 1 对 1 对话`}
                      onClick={() => onOpenBotChat(bot)}
                    >
                      <BotAvatar
                        id={bot.id}
                        name={bot.name}
                        state={
                          row && state !== "idle"
                            ? state
                            : dmState === "busy"
                              ? "working"
                              : bot.enabled
                                ? undefined
                                : "off"
                        }
                        className="sa-row-avatar"
                      />
                      <span className="sa-row-label">{bot.name}</span>
                      <span className="sa-row-meta">
                        {row && state !== "idle" ? SHORT_STATE[state] : SHORT_HARNESS[bot.harness]}
                      </span>
                    </button>
                  </li>
                );
              })}
            </ul>
          )}
        </section>

        {/* 4. 协作房间（多 Bot 群聊 / 项目室） */}
        <section className="sa-section" aria-label="协作房间">
          <div className="sa-section-head">
            <h2 className="sa-section-title">协作房间</h2>
            <span className="sa-section-count sa-nav-label">{groupRooms.length}</span>
          </div>
          {groupRooms.length === 0 ? (
            <p className="sa-section-empty sa-nav-label">
              需要多位 Bot 协同干活时，点「组建房间」拉它们进群。
            </p>
          ) : (
            <ul className="sa-list">
              {groupRooms.map((room) => (
                <li key={room.id}>
                  <button
                    type="button"
                    className="sa-row"
                    aria-current={room.id === selectedRoomId ? "page" : undefined}
                    title={`${room.name}（${room.member_bot_ids.length} 位成员）`}
                    onClick={() => onSelectRoom(room.id)}
                  >
                    <span className="sa-room-mark" aria-hidden="true">
                      {[...room.name.trim()][0] ?? "·"}
                      <span className="sa-dot" data-state={roomStates[room.id] ?? "idle"} />
                    </span>
                    <span className="sa-row-label">{room.name}</span>
                    <span className="sa-row-meta">{room.member_bot_ids.length} 人</span>
                  </button>
                </li>
              ))}
            </ul>
          )}
        </section>

        {/* 5. 工作台工具 */}
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
        <div className="sa-sidebar-foot-row">
          <span className="sa-sidebar-status">
            <span className="sa-dot" data-state={connected ? "idle" : "warn"} aria-hidden="true" />
            <span className="sa-nav-label">{connected ? "已连接" : "重连中…"}</span>
          </span>
          {desktop && (
            <button
              type="button"
              className="sa-connect"
              disabled={desktop.pinned === "workdir"}
              data-state={desktop.connecting ? "connecting" : desktop.failed ? "failed" : undefined}
              title={desktop.pinned === "workdir" ? "启动时用 --workdir 定死了用本机，换不了" : "连接本机或服务器"}
              aria-label={`连接：${connectionLabel(desktop)}`}
              onClick={onConnect}
            >
              <Icon name={desktop.current.mode === "server" ? "server" : "plug"} />
              <span className="sa-nav-label sa-connect-label">{connectionLabel(desktop)}</span>
            </button>
          )}
        </div>
        {version && (
          <div className="sa-sidebar-foot-meta sa-nav-label">
            <button
              type="button"
              className="sa-version-btn"
              data-update={updateAvailable || undefined}
              title={updateAvailable ? "发现新版本，点击查看并更新" : "点击检查更新"}
              onClick={onUpdate}
            >
              <span className="sa-version-text">v{version}</span>
              {updateAvailable ? (
                <span className="sa-update-badge">有新版本</span>
              ) : (
                <span className="sa-version-check">检查更新</span>
              )}
            </button>
          </div>
        )}
      </footer>
    </nav>
  );
}

/** Myriad 的标：跃鲲剪影。 */
function BrandMark() {
  return (
    <svg className="sa-brand-mark" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
      <path transform="matrix(0.003510 0 0 -0.003510 -2.530 27.695)" d="M3530 6555 c-11 -2 -57 -10 -102 -19 -45 -9 -108 -20 -140 -26 -56 -10 -69 -12 -148 -30 -21 -5 -51 -11 -66 -14 -659 -141 -1015 -409 -1193 -896 -9 -24 -19 -58 -23 -76 -3 -18 -9 -43 -12 -56 -33 -143 -31 -146 56 -77 97 77 240 166 332 205 174 74 348 125 356 104 5 -11 2 -13 -126 -94 -65 -41 -134 -83 -286 -173 -275 -163 -537 -369 -690 -539 -19 -21 -41 -46 -50 -56 -70 -76 -126 -149 -220 -286 -68 -99 -159 -265 -206 -378 -11 -27 -30 -73 -42 -100 -80 -190 -147 -472 -171 -720 -6 -67 -8 -257 -3 -302 25 -222 68 -325 149 -352 31 -11 141 -9 193 2 145 33 217 55 329 102 68 29 227 107 269 132 12 7 45 26 72 42 63 37 205 132 258 171 130 98 280 221 366 301 23 21 52 47 64 58 12 11 26 25 32 30 5 6 27 25 48 44 21 19 39 35 40 36 1 1 19 17 40 36 21 19 46 41 56 50 241 220 519 425 756 557 52 29 166 86 194 98 13 6 43 19 66 29 206 92 488 164 724 187 65 6 312 8 357 3 21 -2 57 -6 80 -8 347 -38 721 -194 936 -391 25 -24 26 -32 4 -66 -52 -80 -108 -211 -133 -309 -14 -56 -17 -71 -28 -145 -33 -229 49 -538 205 -771 60 -89 162 -210 224 -267 20 -18 45 -40 55 -50 41 -38 86 -72 144 -111 100 -67 96 -70 96 74 0 58 2 117 4 131 2 15 6 41 8 59 7 51 27 144 40 192 13 46 22 74 71 206 30 82 44 124 53 163 10 43 19 47 94 35 22 -4 64 -8 94 -10 151 -11 162 -12 260 -24 126 -15 321 -67 426 -112 42 -18 51 -5 36 54 -82 338 -297 625 -600 801 -15 8 -39 22 -53 30 -158 91 -394 162 -601 180 -25 2 -60 5 -78 6 -44 4 -48 7 -72 61 -26 59 -83 176 -100 204 -7 12 -20 35 -29 50 -99 174 -170 275 -308 442 -179 215 -397 403 -662 568 -62 39 -241 131 -306 157 -23 10 -65 27 -93 39 -47 19 -71 27 -193 64 -130 39 -297 74 -427 88 -169 19 -427 19 -592 0 -72 -8 -100 -12 -160 -22 -142 -24 -159 -16 -66 31 78 39 167 132 215 222 24 47 19 49 -63 33 -126 -25 -144 -17 -60 27 137 72 274 203 331 317 29 59 24 70 -31 59z m222 -1715 c32 -32 -113 -179 -242 -245 -81 -41 -72 -59 24 -46 68 10 41 -40 -55 -104 -85 -56 -191 -78 -295 -63 -38 6 -61 10 -176 34 -120 25 -305 -65 -499 -245 -70 -65 -42 19 57 169 131 197 339 332 625 404 75 19 181 40 307 62 46 8 116 21 156 28 87 16 88 16 98 6z" />
    </svg>
  );
}
