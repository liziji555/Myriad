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
  presence: readonly Presence[];
  connected: boolean;
  /** 等人处理的任务有几个。 */
  pendingTasks: number;
  /** 宽窗口：左栏常驻，这里只写房间名；窄窗口：点房间名开关抽屉。 */
  docked: boolean;
  roomsOpen: boolean;
  drawerOpen: boolean;
  onToggleRooms(): void;
  onToggleDrawer(): void;
  onShowTasks(): void;
  onOpenPalette(): void;
  theme: Theme;
  /** 换一种明暗：跟随系统 → 浅色 → 深色。 */
  onCycleTheme(): void;
  /** 点了一个 Bot 的牌子：跳到它正在做的那件事。 */
  onPick(row: Presence): void;
}

/**
 * 顶上的现场栏：现在在哪个房间、每个 Bot 此刻在做什么。
 * 牌子上的话会变，点一下跳到主线上对应的地方。
 */
export function StageBar({
  room,
  presence,
  connected,
  pendingTasks,
  docked,
  roomsOpen,
  drawerOpen,
  onToggleRooms,
  onToggleDrawer,
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
          <h1 className="sa-room-title">{room?.name ?? "还没有房间"}</h1>
        </div>
      ) : (
        <button
          type="button"
          className="sa-stage-room"
          aria-expanded={roomsOpen}
          title="换房间、新建房间"
          onClick={onToggleRooms}
        >
          <span className="sa-brand-mark" aria-hidden="true" />
          <span className="sa-brand">Myriad</span>
          <span className="sa-stage-sep" aria-hidden="true">
            /
          </span>
          <h1 className="sa-room-title">{room?.name ?? "还没有房间"}</h1>
          <span className="sa-stage-caret" aria-hidden="true" />
        </button>
      )}

      <ul className="sa-presence" aria-label={`现场：${presenceSummary(presence)}`}>
        {presence.map((row) => {
          const jumps = Boolean(row.messageId || row.taskId);
          return (
            <li key={row.botId}>
              <button
                type="button"
                className="sa-presence-chip"
                data-state={row.state}
                disabled={!jumps}
                title={jumps ? `${row.name}：${row.text}（点一下去看）` : `${row.name}：${row.text}`}
                aria-label={`${row.name}：${row.text}`}
                onClick={() => onPick(row)}
              >
                <BotAvatar id={row.botId} name={row.name} state={row.state} />
                <span className="sa-presence-name">{row.name}</span>
                <span className="sa-presence-text">{row.text}</span>
                {/* 窗口窄的时候只剩头像；有事等人时多留两个字。 */}
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
            {pendingTasks > 0 && <span className="sa-count">{pendingTasks} 个等你</span>}
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
            aria-pressed={drawerOpen}
            title={drawerOpen ? "收起面板" : "打开面板：任务、成员、用量、审计…"}
            aria-label={drawerOpen ? "收起面板" : "打开面板"}
            onClick={onToggleDrawer}
          >
            <Icon name="panel" />
          </button>
        )}
        <WindowControls />
      </div>
    </header>
  );
}
