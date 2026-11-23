/** 界面的摆法：左栏收没收、右边的面板开没开、多宽。记在浏览器里，刷新之后还是这样。 */
export interface Layout {
  panelOpen: boolean;
  panelWidth: number;
  /** 左栏收成一条只有图标的窄条。 */
  sidebarCollapsed: boolean;
}

export const PANEL_MIN = 320;
export const PANEL_MAX = 420;
export const PANEL_DEFAULT = 360;
/** 用方向键调面板宽度时一次挪多少。 */
export const PANEL_STEP = 16;

const KEY = "sa.layout";

export function clampPanelWidth(width: number): number {
  if (!Number.isFinite(width)) return PANEL_DEFAULT;
  return Math.min(PANEL_MAX, Math.max(PANEL_MIN, Math.round(width)));
}

/** 存着的东西不是自己写的那样（被别的版本、别的程序改过）时用默认的，不报错。 */
export function parseLayout(stored: string | null): Layout {
  const fallback: Layout = { panelOpen: false, panelWidth: PANEL_DEFAULT, sidebarCollapsed: false };
  if (!stored) return fallback;
  try {
    const value: unknown = JSON.parse(stored);
    if (typeof value !== "object" || value === null) return fallback;
    const { panelOpen, panelWidth, sidebarCollapsed } = value as Record<string, unknown>;
    return {
      panelOpen: panelOpen === true,
      panelWidth: typeof panelWidth === "number" ? clampPanelWidth(panelWidth) : PANEL_DEFAULT,
      sidebarCollapsed: sidebarCollapsed === true,
    };
  } catch {
    return fallback;
  }
}

export function loadLayout(): Layout {
  try {
    return parseLayout(localStorage.getItem(KEY));
  } catch {
    return parseLayout(null);
  }
}

export function saveLayout(layout: Layout): void {
  try {
    localStorage.setItem(KEY, JSON.stringify(layout));
  } catch {
    // 浏览器不让存（隐私模式、满了）：这次照常用，只是刷新后不记得。
  }
}

/** 拖动面板左边的那条线：指针在窗口里的横坐标对应面板多宽。 */
export function panelWidthAt(pointerX: number, windowWidth: number): number {
  return clampPanelWidth(windowWidth - pointerX);
}
