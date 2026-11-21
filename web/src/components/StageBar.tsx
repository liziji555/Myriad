import type { Presence } from "../api/presence";
import { presenceSummary } from "../api/presence";
import type { Room } from "../api/types";

interface Props {
  room: Room | undefined;
  presence: readonly Presence[];
  connected: boolean;
  /** 等人处理的任务有几个。 */
  pendingTasks: number;
  roomsOpen: boolean;
  drawerOpen: boolean;
  onToggleRooms(): void;
  onToggleDrawer(): void;
  onShowTasks(): void;
  onOpenPalette(): void;
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
  roomsOpen,
  drawerOpen,
  onToggleRooms,
  onToggleDrawer,
  onShowTasks,
  onOpenPalette,
  onPick,
}: Props) {
  return (
    <header className="sa-stage">
      <button
        type="button"
        className="sa-stage-room"
        aria-expanded={roomsOpen}
        title="换房间、新建房间"
        onClick={onToggleRooms}
      >
        <span className="sa-brand">苦乐</span>
        <span className="sa-stage-sep" aria-hidden="true">
          /
        </span>
        <h1 className="sa-room-title">{room?.name ?? "还没有房间"}</h1>
        <span className="sa-stage-caret" aria-hidden="true" />
      </button>

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
                onClick={() => onPick(row)}
              >
                <span className="sa-presence-dot" aria-hidden="true" />
                <span className="sa-presence-name">{row.name}</span>
                <span className="sa-presence-text">{row.text}</span>
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
        <button type="button" className="sa-chip" onClick={onOpenPalette} title="搜索、跳转">
          搜索 <kbd className="sa-kbd">{/Mac|iPhone|iPad/.test(navigator.platform) ? "⌘ K" : "Ctrl K"}</kbd>
        </button>
        {room && (
          <>
            <button type="button" className="sa-chip" data-alert={pendingTasks > 0} onClick={onShowTasks}>
              任务{pendingTasks > 0 ? ` · ${pendingTasks} 个等你` : ""}
            </button>
            <button type="button" className="sa-chip" aria-pressed={drawerOpen} onClick={onToggleDrawer}>
              {drawerOpen ? "收起面板" : "面板"}
            </button>
          </>
        )}
      </div>
    </header>
  );
}
