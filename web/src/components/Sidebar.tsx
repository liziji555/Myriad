import { useMemo, useState } from "react";
import { connectionLabel, type DesktopState } from "../api/desktop";
import type { GlobalBotSignal, Presence, PresenceState } from "../api/presence";
import type { Bot, BotId, Harness, Room, RoomId } from "../api/types";
import { BotMascot, RoomMascotCollage } from "./BotMascot";
import type { PanelTab } from "./ContextPanel";
import { Icon, type IconName } from "./Icon";

/** 房间此刻的样子：有 Bot 在忙、有事等人，还是空着。 */
export type RoomState = "busy" | "warn" | "idle";

/** 左栏里能直接打开的面板页。 */
const NAV: readonly { tab: PanelTab; label: string; icon: IconName }[] = [
  { tab: "profile", label: "Bot 档案与形象", icon: "bot" },
  { tab: "execution", label: "执行态势", icon: "graph" },
  { tab: "tasks", label: "任务", icon: "tasks" },
  { tab: "routines", label: "定时任务", icon: "clock" },
  { tab: "members", label: "成员与工位", icon: "users" },
  { tab: "files", label: "文件与 Diff", icon: "file" },
  { tab: "links", label: "连接", icon: "link" },
  { tab: "models", label: "模型", icon: "bot" },
  { tab: "extensions", label: "技能和 MCP", icon: "plug" },
  { tab: "usage", label: "用量", icon: "chart" },
  { tab: "audit", label: "审计", icon: "shield" },
];

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

const MASCOT_LABEL: Record<string, string> = {
  waiting: "等你处理",
  error: "出错",
  working: "工作中",
  searching: "检索中",
  sending: "协作中",
  thinking: "思考中",
  done: "刚完成",
  idle: "空闲",
  sleeping: "休眠",
};

type ListOrderMode = "default" | "activity";
const ORDER_KEY = "myriad.sidebar.order.v1";

function loadOrderMode(): ListOrderMode {
  try {
    return window.localStorage.getItem(ORDER_KEY) === "activity" ? "activity" : "default";
  } catch {
    return "default";
  }
}

interface Props {
  /** 主控 Bot（后端持久化 is_master，固定置顶于中枢卡）。 */
  masterBot: Bot | null;
  /** 1 对 1 专属 Bot 映射（用于判断当前是否正与某个 Bot 私聊）。 */
  directRoomOf(botId: BotId): Room | undefined;
  /** 多 Bot 协作房间（非单 Bot 1v1 私聊的房间）。 */
  groupRooms: readonly Room[];
  bots: readonly Bot[];
  selectedRoomId: RoomId | null;
  roomStates: Readonly<Record<RoomId, RoomState>>;
  presence: readonly Presence[];
  globalSignals: Readonly<Record<BotId, GlobalBotSignal>>;
  connected: boolean;
  version: string;
  collapsed: boolean;
  docked: boolean;
  onToggleCollapsed(): void;
  activeTab: PanelTab | null;
  onShowPanel(tab: PanelTab): void;
  onOpenBotChat(bot: Bot): void;
  onOpenBotProfile(bot: Bot): void;
  onSelectRoom(id: RoomId): void;
  onNewBot(): void;
  onNewRoom(): void;
  desktop: DesktopState | null;
  onConnect(): void;
  updateAvailable: boolean;
  onUpdate(): void;
}

/**
 * 融合 Grok Bot + DS-Bot + RokBot + ProkBot Plus 的 Bot-First 左侧栏：
 * 1. 置顶「中枢主控」Living Stage 大卡（黑白立体动态中枢形象 + 在岗子 Bot 迷你阵列 + 一键定制形象）；
 * 2. 一级动作「+ 新建 Bot」「+ 组建房间」；
 * 3. 「我的 Bot」独立列表（支持默认列表 / RokBot 动态四段分组：待处理 → 进行中 → 未读 → 空闲）；
 * 4. 「协作房间」多 Bot 群聊（自动展示成员拼图头像）；
 * 5. 「工作台」导航与连接状态底栏。
 */
export function Sidebar({
  masterBot,
  directRoomOf,
  groupRooms,
  bots,
  selectedRoomId,
  roomStates,
  globalSignals,
  connected,
  version,
  collapsed,
  docked,
  onToggleCollapsed,
  activeTab,
  onShowPanel,
  onOpenBotChat,
  onOpenBotProfile,
  onSelectRoom,
  onNewBot,
  onNewRoom,
  desktop,
  onConnect,
  updateAvailable,
  onUpdate,
}: Props) {
  const [orderMode, setOrderMode] = useState<ListOrderMode>(loadOrderMode);
  const narrow = docked && collapsed;

  const toggleOrderMode = () => {
    setOrderMode((prev) => {
      const next: ListOrderMode = prev === "default" ? "activity" : "default";
      try {
        window.localStorage.setItem(ORDER_KEY, next);
      } catch {
        // ignore storage errors
      }
      return next;
    });
  };

  const subBots = useMemo(
    () => bots.filter((bot) => !masterBot || bot.id !== masterBot.id),
    [bots, masterBot],
  );

  // 中枢卡片下方的「在岗小队」条：实时展示全房间正在忙或等审批的子 Bot（最多 5 个）。
  const activeCrew = useMemo(
    () =>
      subBots.filter((bot) => {
        const s = globalSignals[bot.id]?.mascotState;
        return (
          s === "waiting" ||
          s === "working" ||
          s === "searching" ||
          s === "sending" ||
          s === "thinking"
        );
      }),
    [subBots, globalSignals],
  );

  const groupedSubBots = useMemo(() => {
    if (orderMode === "default") {
      return [{ id: "all", label: null as string | null, items: subBots }];
    }
    const pending: Bot[] = [];
    const active: Bot[] = [];
    const unread: Bot[] = [];
    const quiet: Bot[] = [];
    for (const bot of subBots) {
      const st = globalSignals[bot.id]?.mascotState ?? (bot.enabled ? "idle" : "sleeping");
      if (st === "waiting" || st === "error") pending.push(bot);
      else if (st === "working" || st === "searching" || st === "sending" || st === "thinking") active.push(bot);
      else if (st === "done") unread.push(bot);
      else quiet.push(bot);
    }
    return [
      { id: "pending", label: "待处理", items: pending },
      { id: "active", label: "进行中", items: active },
      { id: "unread", label: "刚完成", items: unread },
      { id: "quiet", label: "空闲与休眠", items: quiet },
    ].filter((group) => group.items.length > 0);
  }, [orderMode, subBots, globalSignals]);

  const masterDirect = masterBot ? directRoomOf(masterBot.id) : undefined;
  const masterSelected = Boolean(masterDirect && masterDirect.id === selectedRoomId);
  const masterSignal = masterBot ? globalSignals[masterBot.id] : undefined;
  const masterMascotState =
    masterSignal?.mascotState ?? (masterBot?.enabled ? "idle" : "sleeping");
  const masterPresenceState: PresenceState =
    masterSignal?.presence ?? (masterBot?.enabled ? "idle" : "off");

  const renderBotRow = (bot: Bot) => {
    const dm = directRoomOf(bot.id);
    const active = Boolean(dm && dm.id === selectedRoomId);
    const sig = globalSignals[bot.id];
    const dmBusy = dm && roomStates[dm.id] === "busy";
    const mascotState =
      sig && sig.mascotState !== "idle"
        ? sig.mascotState
        : dmBusy
          ? "working"
          : bot.enabled
            ? "idle"
            : "sleeping";
    const subtitle =
      sig && sig.mascotState !== "idle"
        ? sig.text
        : bot.role || SHORT_HARNESS[bot.harness];
    return (
      <li key={bot.id} className="sa-bot-row-item">
        <button
          type="button"
          className="sa-row sa-bot-row"
          aria-current={active ? "page" : undefined}
          data-state={sig?.presence ?? (bot.enabled ? "idle" : "off")}
          data-mascot-state={mascotState}
          title={`${bot.name}（${bot.role || SHORT_HARNESS[bot.harness]}）· 点击 1 对 1 对话`}
          onClick={() => onOpenBotChat(bot)}
        >
          <BotMascot
            id={bot.id}
            name={bot.name}
            isMaster={bot.is_master}
            shape={bot.avatar_shape}
            color={bot.avatar_color}
            image={bot.avatar_image}
            state={mascotState}
            activeTool={sig?.activeTool}
            size={narrow ? 34 : 30}
            interactive={!narrow}
          />
          <span className="sa-bot-row-copy sa-nav-label">
            <span className="sa-bot-row-top">
              <span className="sa-row-label">{bot.name}</span>
              <span className="sa-row-meta">
                {mascotState !== "idle" && mascotState !== "sleeping"
                  ? MASCOT_LABEL[mascotState]
                  : SHORT_HARNESS[bot.harness]}
              </span>
            </span>
            <small className="sa-bot-row-sub">{subtitle}</small>
          </span>
        </button>
        {!narrow && (
          <button
            type="button"
            className="sa-row-profile-btn"
            title={`查看或定制「${bot.name}」的形象与档案`}
            aria-label={`定制 ${bot.name} 档案`}
            onClick={(event) => {
              event.stopPropagation();
              onOpenBotProfile(bot);
            }}
          >
            ⋯
          </button>
        )}
      </li>
    );
  };

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
        {/* 1. 置顶中枢主控 Living Stage 卡 */}
        {masterBot && (
          <div className="sa-master-wrap">
            <div
              className="sa-master-hero"
              data-selected={masterSelected ? "true" : undefined}
              data-state={masterPresenceState}
            >
              <button
                type="button"
                className="sa-master-card"
                aria-current={masterSelected ? "page" : undefined}
                title={`中枢主控「${masterBot.name}」：点击进入主控对话`}
                onClick={() => onOpenBotChat(masterBot)}
              >
                <BotMascot
                  id={masterBot.id}
                  name={masterBot.name}
                  isMaster={masterBot.is_master}
                  shape={masterBot.avatar_shape}
                  color={masterBot.avatar_color}
                  image={masterBot.avatar_image}
                  state={masterMascotState}
                  activeTool={masterSignal?.activeTool}
                  size={narrow ? 36 : 52}
                  gaze={!narrow}
                  interactive={!narrow}
                />
                <span className="sa-master-info sa-nav-label">
                  <span className="sa-master-top">
                    <strong>{masterBot.name}</strong>
                    <em className="sa-master-badge">主控</em>
                  </span>
                  <small className="sa-master-role">
                    {masterSignal && masterMascotState !== "idle"
                      ? masterSignal.text
                      : masterBot.role || "中枢调度 · 创建并指挥子 Bot"}
                  </small>
                </span>
              </button>
              {!narrow && (
                <button
                  type="button"
                  className="sa-master-edit-btn"
                  title="定制主控形象与档案"
                  aria-label="定制主控形象与档案"
                  onClick={() => onOpenBotProfile(masterBot)}
                >
                  形象
                </button>
              )}
              {!narrow && activeCrew.length > 0 && (
                <div className="sa-crew-strip" aria-label="正在工作的子 Bot">
                  <span className="sa-crew-label">{activeCrew.length} 位在岗</span>
                  <div className="sa-crew-avatars">
                    {activeCrew.slice(0, 5).map((member) => (
                      <button
                        key={member.id}
                        type="button"
                        className="sa-crew-chip"
                        title={`${member.name}：${globalSignals[member.id]?.text ?? "工作中"}`}
                        onClick={() => onOpenBotChat(member)}
                      >
                        <BotMascot
                          id={member.id}
                          name={member.name}
                          isMaster={member.is_master}
                          shape={member.avatar_shape}
                          color={member.avatar_color}
                          image={member.avatar_image}
                          state={globalSignals[member.id]?.mascotState ?? "working"}
                          activeTool={globalSignals[member.id]?.activeTool}
                          size={22}
                          showBadge={false}
                        />
                      </button>
                    ))}
                    {activeCrew.length > 5 && (
                      <span className="sa-crew-more">+{activeCrew.length - 5}</span>
                    )}
                  </div>
                </div>
              )}
            </div>
          </div>
        )}

        {/* 2. 两个明确的一级动作：先建 Bot，再按需组建多 Bot 协作房间 */}
        <div className="sa-side-actions">
          <button
            type="button"
            className="sa-side-action-btn"
            data-primary="true"
            title="新建独立 Bot（可定制动态角色形象）"
            onClick={onNewBot}
          >
            <Icon name="plus" size={14} />
            <span className="sa-nav-label">新建 Bot</span>
          </button>
          <button
            type="button"
            className="sa-side-action-btn"
            title="挑选多个 Bot 组建协作房间"
            onClick={onNewRoom}
          >
            <Icon name="users" size={14} />
            <span className="sa-nav-label">组建房间</span>
          </button>
        </div>

        {/* 3. 我的 Bot（1v1 独立列表 + 支持动态状态分组切换） */}
        <section className="sa-section" aria-label="我的 Bot">
          <div className="sa-section-head">
            <h2 className="sa-section-title">我的 Bot</h2>
            <span className="sa-section-head-right sa-nav-label">
              {subBots.length > 1 && (
                <button
                  type="button"
                  className="sa-sort-toggle"
                  data-active={orderMode === "activity"}
                  title={
                    orderMode === "activity"
                      ? "当前按动态分组（待处理/进行中/刚完成/空闲），点此切回默认顺序"
                      : "点此按动态分组排序（待处理优先）"
                  }
                  onClick={toggleOrderMode}
                >
                  {orderMode === "activity" ? "动态分组" : "默认"}
                </button>
              )}
              <span className="sa-section-count">{subBots.length}</span>
            </span>
          </div>
          {subBots.length === 0 ? (
            <p className="sa-section-empty sa-nav-label">
              点「新建 Bot」定制专属形象，或直接让主控帮你组建专职小队。
            </p>
          ) : (
            groupedSubBots.map((group) => (
              <div key={group.id} className="sa-bot-group">
                {group.label && !narrow && (
                  <div className="sa-group-heading">
                    <span>{group.label}</span>
                    <span>{group.items.length}</span>
                  </div>
                )}
                <ul className="sa-list">{group.items.map(renderBotRow)}</ul>
              </div>
            ))
          )}
        </section>

        {/* 4. 协作房间（多 Bot 群聊，拼图头像） */}
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
              {groupRooms.map((room) => {
                const roomMembers = room.member_bot_ids.flatMap((id) =>
                  bots.filter((b) => b.id === id),
                );
                return (
                  <li key={room.id}>
                    <button
                      type="button"
                      className="sa-row"
                      aria-current={room.id === selectedRoomId ? "page" : undefined}
                      title={`${room.name}（${room.member_bot_ids.length} 位成员）`}
                      onClick={() => onSelectRoom(room.id)}
                    >
                      {roomMembers.length >= 2 ? (
                        <span className="sa-room-collage-wrap">
                          <RoomMascotCollage members={roomMembers} size={narrow ? 32 : 30} />
                          <span className="sa-dot" data-state={roomStates[room.id] ?? "idle"} />
                        </span>
                      ) : (
                        <span className="sa-room-mark" aria-hidden="true">
                          {[...room.name.trim()][0] ?? "·"}
                          <span className="sa-dot" data-state={roomStates[room.id] ?? "idle"} />
                        </span>
                      )}
                      <span className="sa-row-label">{room.name}</span>
                      <span className="sa-row-meta">{room.member_bot_ids.length} 人</span>
                    </button>
                  </li>
                );
              })}
            </ul>
          )}
        </section>

        {/* 5. 工作台工具导航 */}
        <section className="sa-section" aria-label="工作台">
          <h2 className="sa-section-title">工作台</h2>
          <ul className="sa-list">
            {NAV.map((item) => (
              <li key={item.tab}>
                <button
                  type="button"
                  className="sa-row sa-nav-row"
                  aria-current={activeTab === item.tab ? "page" : undefined}
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

      <footer className="sa-sidebar-foot">
        <span className="sa-conn-group">
          <span className="sa-conn" data-connected={connected} title={connected ? "已连接" : "正在重连"}>
            <span className="sa-conn-dot" aria-hidden="true" />
            <span className="sa-nav-label">{connected ? "已连接" : "重连中"}</span>
          </span>
          {version && (
            <button
              type="button"
              className="sa-version"
              data-update={updateAvailable || undefined}
              title={updateAvailable ? `当前 v${version}，有新版本可以更新` : `当前 v${version}，点一下检查更新`}
              onClick={onUpdate}
            >
              <span className="sa-nav-label">v{version}</span>
              {updateAvailable && <span className="sa-update-dot" aria-hidden="true" />}
            </button>
          )}
        </span>
        {desktop && desktop.pinned !== "workdir" && (
          <button
            type="button"
            className="sa-conn-button"
            title={`现在连的是${connectionLabel(desktop)}，点一下切换`}
            onClick={onConnect}
          >
            <Icon name="plug" size={14} />
            <span className="sa-nav-label">{connectionLabel(desktop)}</span>
          </button>
        )}
      </footer>
    </nav>
  );
}

/** 左上角的小标志：一只向右游的小鲸鱼（Myriad 的意思）。 */
function BrandMark() {
  return (
    <svg className="sa-brand-logo" width="24" height="24" viewBox="0 0 24 24" aria-hidden="true">
      <path
        fill="currentColor"
        d="M5.4 13.6c-.8-1.8-.4-4.2 1.1-5.8 1.6-1.7 4-2.4 6.5-2.1 2.4.3 4.3 1.7 5.2 3.7.7 1.6.7 3.6-.2 5-1.1 1.7-3.2 2.8-5.7 2.8H8.6c-1.4 0-2.6-.9-3.2-2.2l-1.9.8c-.4.2-.8-.2-.6-.6l1.3-2.4-1.5-1.4c-.3-.3-.1-.9.4-.9l2.3.1z"
      />
      <circle cx="14.6" cy="10.4" r="1.25" fill="var(--sa-surface)" />
    </svg>
  );
}
