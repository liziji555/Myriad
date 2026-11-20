import { useEffect, useState, type FormEvent } from "react";
import type { Bot, Room, RoomId } from "../api/types";

interface Props {
  rooms: readonly Room[];
  bots: readonly Bot[];
  selectedRoomId: RoomId | null;
  connected: boolean;
  version: string;
  onSelect(id: RoomId): void;
  onCreateRoom(name: string): Promise<void>;
  /** 每加一就打开「新建房间」的输入框（命令面板里点的）。 */
  createSignal: number;
  onOpenPalette(): void;
}

export function Sidebar({
  rooms,
  bots,
  selectedRoomId,
  connected,
  version,
  onSelect,
  onCreateRoom,
  createSignal,
  onOpenPalette,
}: Props) {
  const [creating, setCreating] = useState(false);
  useEffect(() => {
    if (createSignal > 0) setCreating(true);
  }, [createSignal]);
  const [name, setName] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  const close = () => {
    setCreating(false);
    setName("");
    setError("");
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
    <nav className="sa-sidebar" aria-label="导航">
      <header className="sa-sidebar-head">
        <span className="sa-brand">SuperAgent</span>
        <button
          type="button"
          className="sa-icon-button"
          aria-label="新建房间"
          title="新建房间"
          onClick={() => setCreating(true)}
        >
          +
        </button>
      </header>
      <button type="button" className="sa-search" onClick={onOpenPalette}>
        <span>搜索、跳转…</span>
        <kbd className="sa-kbd">Ctrl K</kbd>
      </button>

      <section className="sa-section">
        <h2 className="sa-section-title">房间</h2>
        {creating && (
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
                onClick={() => onSelect(room.id)}
              >
                <span className="sa-dot" data-state="idle" aria-hidden="true" />
                <span className="sa-row-label">{room.name}</span>
              </button>
            </li>
          ))}
        </ul>
      </section>

      <section className="sa-section">
        <h2 className="sa-section-title">Bot</h2>
        <ul className="sa-list">
          {bots.map((bot) => (
            <li key={bot.id} className="sa-row sa-row-static">
              <span className="sa-dot" data-state={bot.enabled ? "idle" : "off"} aria-hidden="true" />
              <span className="sa-row-label">{bot.name}</span>
              <span className="sa-row-meta">{bot.enabled ? "空闲" : "已停用"}</span>
            </li>
          ))}
        </ul>
      </section>

      <footer className="sa-sidebar-foot" role="status">
        <span className="sa-dot" data-state={connected ? "idle" : "warn"} aria-hidden="true" />
        <span>{connected ? "已连接" : "连接中断，正在重连…"}</span>
        {version && <span className="sa-row-meta">v{version}</span>}
      </footer>
    </nav>
  );
}
