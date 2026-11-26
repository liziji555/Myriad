import type { Presence } from "../api/presence";
import { presenceSummary } from "../api/presence";
import { THEME_LABEL, type Theme } from "../api/theme";
import type { Room } from "../api/types";
import { BotAvatar } from "./BotAvatar";
import { Icon } from "./Icon";
import { WindowControls } from "./WindowControls";

const THEME_ICON = { system: "auto", light: "sun", dark: "moon" } as const;

interface Props {
  room: Room | undefined;
  /** 当前会话的性质标签（如「主控」「1v1」「3 人协作」）。 */
  roomBadge?: string | null;
  presence: readonly Presence[];
  connected: boolean;
  /** 等人处理的任务有几个。 */
  pendingTasks: number;
  /** 工作树轨迹当前是否可见。 */
  trajectoryShown: boolean;
  /** 宽窗口：左栏常驻，这里只写房间名；窄窗口：点房间名开关抽屉。 */
  docked: boolean;
  roomsOpen: boolean;
  onToggleRooms(): void;
  onToggleTrajectory(): void;
  onShowTasks(): void;
  onOpenPalette(): void;
  theme: Theme;
  /** 换一种明暗：跟随系统 → 浅色 → 深色。 */
  onCycleTheme(): void;
  /** 点了一个 Bot 的牌子：跳到它正在做的那件事。 */
  onPick(row: Presence): void;
}

/**
 * 顶上的现场栏：当前对话标题 + 每个 Bot 实时现场胶囊（跨栏飞线锚点）+ 全局快捷操作。
 */
export function StageBar({
  room,
  roomBadge,
  presence,
  connected,
  pendingTasks,
  trajectoryShown,
  docked,
  roomsOpen,
  onToggleRooms,
  onToggleTrajectory,
  onShowTasks,
  onOpenPalette,
  theme,
  onCycleTheme,
  onPick,
}: Props) {
  const mac = /Mac|iPhone|iPad/.test(navigator.platform);
  return (
    <header className="sa-stage">
      {docked ? (
        <div className="sa-stage-room" data-docked="true">
          <h1 className="sa-room-title">{room?.name ?? "主控对话"}</h1>
          {roomBadge && <span className="sa-tag">{roomBadge}</span>}
        </div>
      ) : (
        <button
          type="button"
          className="sa-stage-room"
          aria-expanded={roomsOpen}
          title="展开侧栏"
          onClick={onToggleRooms}
        >
          <span className="sa-brand-mark" aria-hidden="true" />
          <span className="sa-brand">Myriad</span>
          <span className="sa-stage-sep" aria-hidden="true">
            /
          </span>
          <h1 className="sa-room-title">{room?.name ?? "主控对话"}</h1>
          <span className="sa-stage-caret" aria-hidden="true" />
        </button>
      )}

      <ul className="sa-presence" aria-label={`现场：${presenceSummary(presence)}`}>
        {presence.map((row) => {
          const jumps = Boolean(row.messageId || row.taskId);
          const running =
            row.state === "tool" || row.state === "writing" || row.state === "working";
          return (
            <li key={row.botId}>
              <button
                type="button"
                className="sa-presence-chip"
                data-presence-bot={row.botId}
                data-state={row.state}
                disabled={!jumps}
                title={jumps ? `${row.name}：${row.text}（点一下去看）` : `${row.name}：${row.text}`}
                aria-label={`${row.name}：${row.text}`}
                onClick={() => onPick(row)}
              >
                {running && (
                  <svg className="light-running-border" aria-hidden="true">
                    <rect x="1" y="1" width="calc(100% - 2px)" height="calc(100% - 2px)" rx="999" pathLength="100" />
                  </svg>
                )}
                <BotAvatar id={row.botId} name={row.name} state={row.state} />
                <span className="sa-presence-name">{row.name}</span>
                <span className="sa-presence-text">{row.text}</span>
                {(row.state === "approval" || row.state === "asking") && (
                  <span className="sa-presence-short" aria-hidden="true">
                    等你
                  </span>
                )}
              </button>
            </li>
          );
        })}
      </ul>

      <div className="sa-stage-actions">
        {!connected && (
          <span className="sa-stage-offline" role="status">
            连接中断，正在重连…
          </span>
        )}
        <button type="button" className="sa-stage-search" onClick={onOpenPalette} title="搜索、跳转">
          <Icon name="search" />
          <span className="sa-stage-search-text">搜索、跳转</span>
          <kbd className="sa-kbd">{mac ? "⌘ K" : "Ctrl K"}</kbd>
        </button>
        {room && (
          <button
            type="button"
            className="sa-stage-button"
            data-alert={pendingTasks > 0}
            title="这个房间的任务"
            onClick={onShowTasks}
          >
            <Icon name="tasks" />
            <span>任务</span>
            {pendingTasks > 0 && <span className="sa-count">{pendingTasks} 待处理</span>}
          </button>
        )}
        <button
          type="button"
          className="sa-icon-button"
          title={`${THEME_LABEL[theme]}（点一下换）`}
          aria-label={`明暗：${THEME_LABEL[theme]}，点一下换`}
          onClick={onCycleTheme}
        >
          <Icon name={THEME_ICON[theme]} />
        </button>
        {room && (
          <button
            type="button"
            className="sa-icon-button"
            aria-label={trajectoryShown ? "收起右侧工作树轨迹" : "显示右侧工作树轨迹"}
            aria-pressed={trajectoryShown}
            title={trajectoryShown ? "收起右侧工作树轨迹" : "显示右侧工作树轨迹"}
            onClick={onToggleTrajectory}
          >
            <Icon name="split" />
          </button>
        )}
        <WindowControls />
      </div>
    </header>
  );
}
