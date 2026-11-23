import { describe, expect, it } from "vitest";
import { clampPanelWidth, panelWidthAt, parseLayout, PANEL_DEFAULT, PANEL_MAX, PANEL_MIN } from "./layout";

describe("layout", () => {
  it("面板的宽度不出范围", () => {
    expect(clampPanelWidth(100)).toBe(PANEL_MIN);
    expect(clampPanelWidth(9000)).toBe(PANEL_MAX);
    expect(clampPanelWidth(371.6)).toBe(372);
    expect(clampPanelWidth(Number.NaN)).toBe(PANEL_DEFAULT);
  });

  it("拖到哪里就是哪里，拖过头停在两端", () => {
    expect(panelWidthAt(1480 - 400, 1480)).toBe(400);
    expect(panelWidthAt(0, 1480)).toBe(PANEL_MAX);
    expect(panelWidthAt(1479, 1480)).toBe(PANEL_MIN);
  });

  it("记下来的摆法能读回来", () => {
    const stored = JSON.stringify({ panelOpen: true, panelWidth: 400, sidebarCollapsed: true });
    expect(parseLayout(stored)).toEqual({ panelOpen: true, panelWidth: 400, sidebarCollapsed: true });
    // 旧版本存的没有左栏这一项：左栏展开着。
    expect(parseLayout('{"panelOpen":true,"panelWidth":400}')).toEqual({
      panelOpen: true,
      panelWidth: 400,
      sidebarCollapsed: false,
    });
  });

  it("读到看不懂的东西时用默认的", () => {
    const fallback = { panelOpen: false, panelWidth: PANEL_DEFAULT, sidebarCollapsed: false };
    for (const stored of [null, "", "不是 JSON", "null", "[1]", '"x"']) {
      expect(parseLayout(stored)).toEqual(fallback);
    }
    expect(parseLayout('{"panelOpen":"yes","panelWidth":"宽"}')).toEqual(fallback);
    expect(parseLayout('{"panelWidth":5000}')).toEqual({ panelOpen: false, panelWidth: PANEL_MAX, sidebarCollapsed: false });
  });
});
