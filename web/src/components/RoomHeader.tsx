import type { Room } from "../api/types";

interface Props {
  room: Room;
  memberCount: number;
  panelOpen: boolean;
  sidebarCollapsed: boolean;
  onToggleSidebar(): void;
  onShowMembers(): void;
  onTogglePanel(): void;
}

export function RoomHeader({
  room,
  memberCount,
  panelOpen,
  sidebarCollapsed,
  onToggleSidebar,
  onShowMembers,
  onTogglePanel,
}: Props) {
  return (
    <header className="sa-room-head">
      <button
        type="button"
        className="sa-chip"
        aria-pressed={!sidebarCollapsed}
        title={sidebarCollapsed ? "展开左边的房间列表" : "收起左边的房间列表"}
        onClick={onToggleSidebar}
      >
        {sidebarCollapsed ? "房间 ›" : "‹ 房间"}
      </button>
      <h1 className="sa-room-title">{room.name}</h1>
      <button type="button" className="sa-chip" onClick={onShowMembers}>
        成员 {memberCount}
      </button>
      <button type="button" className="sa-chip" aria-pressed={panelOpen} onClick={onTogglePanel}>
        {panelOpen ? "收起侧栏" : "展开侧栏"}
      </button>
    </header>
  );
}
