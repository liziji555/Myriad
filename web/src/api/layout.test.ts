import { describe, expect, it } from "vitest";
import { clampPanelWidth, panelWidthAt, parseLayout, PANEL_DEFAULT, PANEL_MAX, PANEL_MIN } from "./layout";

describe("layout", () => {
  it("右栏的宽度不出范围", () => {
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
    const stored = JSON.stringify({ sidebarCollapsed: true, panelWidth: 400 });
    expect(parseLayout(stored)).toEqual({ sidebarCollapsed: true, panelWidth: 400 });
  });

  it("读到看不懂的东西时用默认的", () => {
    const fallback = { sidebarCollapsed: false, panelWidth: PANEL_DEFAULT };
    for (const stored of [null, "", "不是 JSON", "null", "[1]", '"x"']) {
      expect(parseLayout(stored)).toEqual(fallback);
    }
    expect(parseLayout('{"sidebarCollapsed":"yes","panelWidth":"宽"}')).toEqual(fallback);
    expect(parseLayout('{"panelWidth":5000}')).toEqual({ sidebarCollapsed: false, panelWidth: PANEL_MAX });
  });
});
