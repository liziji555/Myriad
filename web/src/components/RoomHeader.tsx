import type { Room } from "../api/types";

interface Props {
  room: Room;
  memberCount: number;
  panelOpen: boolean;
  onShowMembers(): void;
  onTogglePanel(): void;
}

export function RoomHeader({ room, memberCount, panelOpen, onShowMembers, onTogglePanel }: Props) {
  return (
    <header className="sa-room-head">
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
