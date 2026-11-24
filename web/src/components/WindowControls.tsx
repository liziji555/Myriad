import { useEffect, useState } from "react";
import { desktopInvoke } from "../api/desktop";

/** 顶栏、左栏顶上的空白处就是标题栏：按住拖窗口，双击放大。按钮、输入框上不算。还没进主界面时整页都能拖。 */
const BAR = ".sa-stage, .sa-sidebar-head, .sa-gate";
const CONTROL = "button, a, input, textarea, select";

const BUTTONS = [
  { action: "minimize", label: "最小化", path: "M3.5 8h9" },
  { action: "maximize", label: "最大化、还原", path: "M4 4h8v8H4z" },
  { action: "close", label: "关闭", path: "m4 4 8 8M12 4l-8 8" },
] as const;

/**
 * 桌面版的窗口按钮：主窗口没有系统的标题栏，最小化、最大化、关闭画在顶栏最右边。浏览器里不出现。
 * 外壳答应把系统的标题栏收走了才画：老版本的外壳不认这个命令，标题栏还在，这里就什么都不画。
 */
export function WindowControls() {
  const [invoke] = useState(() => desktopInvoke());
  const [owned, setOwned] = useState(false);

  useEffect(() => {
    if (!invoke) return;
    const chrome = (action: string) => void invoke("window_chrome", { action }).catch(() => {});
    void invoke("window_chrome", { action: "own" }).then(
      () => setOwned(true),
      () => {},
    );
    const onBar = (event: MouseEvent) => {
      const target = event.target;
      return target instanceof Element && target.closest(BAR) && !target.closest(CONTROL);
    };
    // 按下之后动了才算拖：一按下就交给系统的话，双击就收不到了。
    let pressed = false;
    const onDown = (event: MouseEvent) => {
      pressed = event.button === 0 && Boolean(onBar(event));
    };
    const onMove = (event: MouseEvent) => {
      if (!pressed) return;
      pressed = false;
      if (event.buttons === 1) chrome("drag");
    };
    const onDouble = (event: MouseEvent) => {
      if (onBar(event)) chrome("maximize");
    };
    document.addEventListener("mousedown", onDown);
    document.addEventListener("mousemove", onMove);
    document.addEventListener("dblclick", onDouble);
    return () => {
      document.removeEventListener("mousedown", onDown);
      document.removeEventListener("mousemove", onMove);
      document.removeEventListener("dblclick", onDouble);
    };
  }, [invoke]);

  if (!invoke || !owned) return null;
  return (
    <div className="sa-window-controls">
      {BUTTONS.map(({ action, label, path }) => (
        <button
          key={action}
          type="button"
          className="sa-window-button"
          data-action={action}
          title={label}
          aria-label={label}
          onClick={() => void invoke("window_chrome", { action }).catch(() => {})}
        >
          <svg viewBox="0 0 16 16" width="16" height="16" fill="none" stroke="currentColor" strokeWidth="1.2" aria-hidden="true">
            <path d={path} />
          </svg>
        </button>
      ))}
    </div>
  );
}
