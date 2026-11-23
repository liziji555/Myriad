/** Bot 头像有几种颜色。同一个 Bot 的颜色不变，不同的 Bot 尽量不一样。 */
export const AVATAR_HUES = 8;

/** 和样式表里 `[data-hue]` 的颜色一一对应，画 SVG 渐变时用。 */
export const HUE_COLORS = ["#d6534a", "#dd8430", "#b8960f", "#43a765", "#21a19b", "#3d83df", "#7a6be0", "#c65cb0"] as const;

/** 按 Bot 的 id 定一种颜色（0 到 AVATAR_HUES-1）。 */
export function hueOf(id: string): number {
  let hash = 0;
  for (const char of id) hash = (hash * 31 + (char.codePointAt(0) ?? 0)) >>> 0;
  return hash % AVATAR_HUES;
}

/** 头像里写的那个字：名字的第一个字，英文大写；名字是空的给一个点。 */
export function initialOf(name: string): string {
  const first = [...name.trim()][0];
  return first ? first.toUpperCase() : "·";
}
