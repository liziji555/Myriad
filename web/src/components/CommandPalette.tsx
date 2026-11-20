import { useEffect, useMemo, useRef, useState } from "react";
import {
  GROUP_LABEL,
  PALETTE_TABS,
  loadRecents,
  pushRecent,
  saveRecents,
  searchPalette,
  type PaletteItem,
  type PaletteTab,
} from "../api/palette";

export interface PaletteEntry extends PaletteItem {
  run(): void;
}

interface Props {
  entries: readonly PaletteEntry[];
  onClose(): void;
}

/** 命令面板：搜房间、Bot、任务和动作，回车直达。用 Ctrl/Cmd+K 打开。 */
export function CommandPalette({ entries, onClose }: Props) {
  const [query, setQuery] = useState("");
  const [tab, setTab] = useState<PaletteTab>("all");
  const [cursor, setCursor] = useState(0);
  const [recents] = useState(loadRecents);
  const list = useRef<HTMLUListElement>(null);

  const found = useMemo(() => searchPalette(entries, query, tab, recents), [entries, query, tab, recents]);
  const active = Math.min(cursor, Math.max(found.length - 1, 0));
  const showRecents = query.trim() === "";

  useEffect(() => {
    list.current?.querySelector('[aria-selected="true"]')?.scrollIntoView({ block: "nearest" });
  }, [active, found]);

  const choose = (item: PaletteItem | undefined) => {
    const entry = entries.find((candidate) => candidate.id === item?.id);
    if (!entry) return;
    saveRecents(pushRecent(recents, entry.id));
    onClose();
    entry.run();
  };

  const moveTab = (step: number) => {
    const at = PALETTE_TABS.findIndex(({ id }) => id === tab);
    const next = PALETTE_TABS[(at + step + PALETTE_TABS.length) % PALETTE_TABS.length];
    if (next) setTab(next.id);
    setCursor(0);
  };

  return (
    <div className="sa-palette-backdrop" onMouseDown={(event) => event.target === event.currentTarget && onClose()}>
      <div
        className="sa-palette"
        role="dialog"
        aria-modal="true"
        aria-label="命令面板"
        onKeyDown={(event) => {
          // 输入法正在选字时的回车和方向键不是给我们的。
          if (event.nativeEvent.isComposing) return;
          if (event.key === "Escape") onClose();
          else if (event.key === "ArrowDown") setCursor(found.length === 0 ? 0 : (active + 1) % found.length);
          else if (event.key === "ArrowUp")
            setCursor(found.length === 0 ? 0 : (active - 1 + found.length) % found.length);
          else if (event.key === "Enter") choose(found[active]);
          else if (event.key === "Tab") moveTab(event.shiftKey ? -1 : 1);
          else return;
          event.preventDefault();
        }}
      >
        <input
          className="sa-palette-input"
          autoFocus
          value={query}
          placeholder="搜房间、Bot、任务，或者要做的事"
          aria-label="搜索"
          role="combobox"
          aria-expanded="true"
          aria-controls="sa-palette-list"
          aria-activedescendant={found[active] ? `sa-palette-${active}` : undefined}
          onChange={(event) => {
            setQuery(event.target.value);
            setCursor(0);
          }}
        />
        <div className="sa-palette-tabs" role="tablist" aria-label="分类">
          {PALETTE_TABS.map(({ id, label }) => (
            <button
              key={id}
              type="button"
              role="tab"
              tabIndex={-1}
              aria-selected={tab === id}
              className="sa-tab"
              onClick={() => {
                setTab(id);
                setCursor(0);
              }}
            >
              {label}
            </button>
          ))}
        </div>
        {found.length === 0 ? (
          <p className="sa-panel-empty">没有找到。</p>
        ) : (
          <ul className="sa-palette-list" id="sa-palette-list" role="listbox" ref={list}>
            {found.map((item, index) => (
              <li
                key={item.id}
                id={`sa-palette-${index}`}
                role="option"
                aria-selected={index === active}
                className="sa-palette-item"
                onMouseMove={() => setCursor(index)}
                onClick={() => choose(item)}
              >
                <span className="sa-tag">
                  {showRecents && recents.includes(item.id) ? "最近" : GROUP_LABEL[item.group]}
                </span>
                <span className="sa-row-label">{item.title}</span>
                {item.hint && <span className="sa-row-meta">{item.hint}</span>}
                {item.shortcut && <kbd className="sa-kbd">{item.shortcut}</kbd>}
              </li>
            ))}
          </ul>
        )}
        <p className="sa-palette-foot">
          <kbd className="sa-kbd">↑↓</kbd> 选择 <kbd className="sa-kbd">回车</kbd> 打开{" "}
          <kbd className="sa-kbd">Tab</kbd> 换分类 <kbd className="sa-kbd">Esc</kbd> 关闭
        </p>
      </div>
    </div>
  );
}
