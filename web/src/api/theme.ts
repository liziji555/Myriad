/** 界面的明暗：跟着系统，或者人自己定。记在浏览器里。 */
export type Theme = "system" | "light" | "dark";

const KEY = "sa.theme";
const ORDER: readonly Theme[] = ["system", "light", "dark"];

export const THEME_LABEL: Record<Theme, string> = {
  system: "明暗跟随系统",
  light: "浅色",
  dark: "深色",
};

/** 存着的东西不是这三样之一时当作跟着系统。 */
export function parseTheme(stored: string | null): Theme {
  return ORDER.find((theme) => theme === stored) ?? "system";
}

/** 点一下换到下一种：跟随系统 → 浅色 → 深色 → 跟随系统。 */
export function nextTheme(theme: Theme): Theme {
  return ORDER[(ORDER.indexOf(theme) + 1) % ORDER.length]!;
}

export function loadTheme(): Theme {
  try {
    return parseTheme(localStorage.getItem(KEY));
  } catch {
    return "system";
  }
}

/** 记下来并立刻用上。存不进去（隐私模式）时只管这一次。 */
export function applyTheme(theme: Theme): void {
  if (theme === "system") delete document.documentElement.dataset.theme;
  else document.documentElement.dataset.theme = theme;
  try {
    localStorage.setItem(KEY, theme);
  } catch {
    // 存不了不影响这一次。
  }
}
