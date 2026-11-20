/** 命令面板里的一项属于哪一类。 */
export type PaletteGroup = "room" | "bot" | "task" | "action";
export type PaletteTab = "all" | PaletteGroup;

export interface PaletteItem {
  /** 全局唯一，带类别前缀（`room:…`、`action:…`），「最近用过」靠它记。 */
  id: string;
  group: PaletteGroup;
  title: string;
  /** 标题右边的小字：在哪个房间、什么状态。也参与搜索，但排在标题后面。 */
  hint?: string;
  /** 快捷键提示，只是显示。 */
  shortcut?: string;
}

export const PALETTE_TABS: readonly { id: PaletteTab; label: string }[] = [
  { id: "all", label: "全部" },
  { id: "room", label: "房间" },
  { id: "bot", label: "Bot" },
  { id: "task", label: "任务" },
  { id: "action", label: "动作" },
];

export const GROUP_LABEL: Record<PaletteGroup, string> = {
  room: "房间",
  bot: "Bot",
  task: "任务",
  action: "动作",
};

/** 最多记几个「最近用过」。 */
export const MAX_RECENTS = 6;
/** 一次最多列多少项。 */
export const MAX_RESULTS = 50;

/**
 * 模糊匹配：`query` 的每个字按顺序出现在 `text` 里就算匹配上，不要求挨着。
 * 分数越高越像：挨着的、在开头的、在词开头的加分，中间跳过的字越多越扣分。匹配不上返回 `null`。
 */
export function fuzzyScore(query: string, text: string): number | null {
  const needle = [...query.toLowerCase()].filter((char) => char.trim() !== "");
  if (needle.length === 0) return 0;
  const hay = [...text.toLowerCase()];
  let score = 0;
  let from = 0;
  let previous = -2;
  for (const char of needle) {
    const at = hay.indexOf(char, from);
    if (at < 0) return null;
    score += 10;
    if (at === previous + 1) score += 8;
    if (at === 0) score += 6;
    else if (/[\s\-_/.「」：:]/.test(hay[at - 1] ?? "")) score += 4;
    score -= Math.min(at - from, 5);
    previous = at;
    from = at + 1;
  }
  // 同样匹配上的，短的更像。
  return score - Math.min(hay.length - needle.length, 20) / 4;
}

/**
 * 要列出来的项。没输入时「最近用过」的在最前面，其余按原来的顺序；
 * 有输入时按像不像排，一样像的保持原来的顺序。
 */
export function searchPalette(
  items: readonly PaletteItem[],
  query: string,
  tab: PaletteTab,
  recents: readonly string[],
): PaletteItem[] {
  const pool = tab === "all" ? items : items.filter((item) => item.group === tab);
  if (query.trim() === "") {
    const recent = recents.flatMap((id) => pool.find((item) => item.id === id) ?? []);
    const rest = pool.filter((item) => !recents.includes(item.id));
    return [...recent, ...rest].slice(0, MAX_RESULTS);
  }
  const scored = pool.flatMap((item, index) => {
    const title = fuzzyScore(query, item.title);
    // 小字里匹配上的排在标题匹配上的后面。
    const hint = item.hint ? fuzzyScore(query, item.hint) : null;
    const score = Math.max(title ?? -Infinity, hint === null ? -Infinity : hint - 30);
    return Number.isFinite(score) ? [{ item, score, index }] : [];
  });
  scored.sort((a, b) => b.score - a.score || a.index - b.index);
  return scored.slice(0, MAX_RESULTS).map(({ item }) => item);
}

/** 用过一项之后的「最近用过」：它排到最前面，最多留 [`MAX_RECENTS`] 个。 */
export function pushRecent(recents: readonly string[], id: string): string[] {
  return [id, ...recents.filter((other) => other !== id)].slice(0, MAX_RECENTS);
}

const RECENTS_KEY = "sa.palette.recents";

export function loadRecents(): string[] {
  try {
    const stored: unknown = JSON.parse(localStorage.getItem(RECENTS_KEY) ?? "[]");
    return Array.isArray(stored) ? stored.filter((id): id is string => typeof id === "string") : [];
  } catch {
    return [];
  }
}

export function saveRecents(recents: readonly string[]): void {
  try {
    localStorage.setItem(RECENTS_KEY, JSON.stringify(recents));
  } catch {
    // 存不了（隐私模式）就算了，只是下次不记得。
  }
}
