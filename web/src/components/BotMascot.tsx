import { useEffect, useId, useRef, useState, type CSSProperties } from "react";
import {
  mascotStateFromPresence,
  mascotTimingFor,
  resolveMascotColor,
  resolveMascotShape,
  type MascotState,
} from "../api/mascotShapes";
import type { PresenceState } from "../api/presence";
import type { Bot } from "../api/types";
import "../styles/mascot.css";

export interface BotMascotProps {
  id: string;
  name: string;
  isMaster?: boolean;
  shape?: string;
  color?: string;
  image?: string;
  state?: PresenceState | "streaming" | MascotState;
  activeTool?: string;
  size?: number;
  /** 是否开启实时鼠标眼神追踪（适合大尺寸中枢卡、档案页与形象预览）。 */
  gaze?: boolean;
  /** 是否允许点击戳一戳 360° 弹跳与悬停抚摸。 */
  interactive?: boolean;
  /** 是否在右下角显示状态角标或右上角显示主控金星。 */
  showBadge?: boolean;
  className?: string;
}

/**
 * Myriad 自建参数化动态角色模型（Grok Bot 风格 12 种立体造型 × 12 种渐变 + 完整生命态动效）。
 * 支持：空闲呼吸/随机眨眼/眼神追踪、思考歪头眯眼、搜索扫视、按造型工作编舞、派活前冲、
 * 等待审批跳动、完成欢呼 ^ ^、报错摇头 > <、停止/停用平静闭眼休眠。
 */
export function BotMascot({
  id,
  name,
  isMaster = false,
  shape,
  color,
  image,
  state,
  activeTool,
  size = 28,
  gaze = false,
  interactive = false,
  showBadge = true,
  className,
}: BotMascotProps) {
  const uid = useId().replace(/:/g, "");
  const rootRef = useRef<HTMLSpanElement>(null);
  const [poked, setPoked] = useState(false);
  const [isDark, setIsDark] = useState(() =>
    typeof document !== "undefined" && document.documentElement.dataset.theme === "dark",
  );

  // 监听主题切换（中枢黑白角色在深色模式下自动切珍珠白，浅色模式下为黑曜石）
  useEffect(() => {
    if (typeof document === "undefined" || typeof MutationObserver === "undefined") return;
    const sync = () => setIsDark(document.documentElement.dataset.theme === "dark");
    sync();
    const obs = new MutationObserver(sync);
    obs.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
    return () => obs.disconnect();
  }, []);

  const canonicalState = mascotStateFromPresence(state, activeTool);
  const shapeDef = resolveMascotShape(id, shape, isMaster);
  const colorDef = resolveMascotColor(id, color, isMaster);
  const timing = mascotTimingFor(id);

  // 异步平滑眼球追踪（仅在 gaze=true 且未休眠时启用，避免全局无谓计算）
  useEffect(() => {
    if (!gaze || canonicalState === "sleeping") return;
    let raf = 0;
    let lastX = 0;
    let lastY = 0;
    const update = () => {
      raf = 0;
      const el = rootRef.current;
      if (!el) return;
      const rect = el.getBoundingClientRect();
      const cx = rect.left + rect.width / 2;
      const cy = rect.top + rect.height / 2;
      const dx = (lastX - cx) / Math.max(rect.width * 2.2, 160);
      const dy = (lastY - cy) / Math.max(rect.height * 2.2, 160);
      const clampedX = Math.max(-1, Math.min(1, dx));
      const clampedY = Math.max(-1, Math.min(1, dy));
      el.style.setProperty("--sa-gx", clampedX.toFixed(3));
      el.style.setProperty("--sa-gy", clampedY.toFixed(3));
    };
    const onMove = (event: PointerEvent) => {
      lastX = event.clientX;
      lastY = event.clientY;
      if (!raf) raf = window.requestAnimationFrame(update);
    };
    const onLeave = () => {
      const el = rootRef.current;
      if (!el) return;
      el.style.setProperty("--sa-gx", "0");
      el.style.setProperty("--sa-gy", "0");
    };
    window.addEventListener("pointermove", onMove, { passive: true });
    window.addEventListener("pointerleave", onLeave);
    return () => {
      if (raf) window.cancelAnimationFrame(raf);
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerleave", onLeave);
    };
  }, [gaze, canonicalState]);

  const triggerPoke = () => {
    if (!interactive || canonicalState === "sleeping") return;
    setPoked(false);
    window.requestAnimationFrame(() => setPoked(true));
    window.setTimeout(() => setPoked(false), 740);
  };

  const stops = isDark && colorDef.darkStops ? colorDef.darkStops : colorDef.stops;
  const eyeFill = isDark ? colorDef.eyeDark : colorDef.eyeLight;
  const eyeY = 104 + (shapeDef.eyeOffsetY ?? 0);
  const gradId = `sa-g-${uid}`;
  const sheenId = `sa-s-${uid}`;
  const shadowId = `sa-d-${uid}`;

  const style: CSSProperties = {
    "--sa-mascot-size": `${size}px`,
    "--sa-speed": String(timing.speed),
    "--sa-delay": `${timing.delaySec.toFixed(2)}s`,
    "--sa-gaze-ms": `${timing.gazeDelayMs}ms`,
  } as CSSProperties;

  const badge =
    showBadge && size >= 22
      ? canonicalState === "waiting"
        ? { cls: "sa-mascot-badge--waiting", text: "?" }
        : canonicalState === "error"
          ? { cls: "sa-mascot-badge--error", text: "" }
          : canonicalState === "working" ||
              canonicalState === "thinking" ||
              canonicalState === "searching" ||
              canonicalState === "sending"
            ? { cls: "sa-mascot-badge--working", text: "" }
            : canonicalState === "done"
              ? { cls: "sa-mascot-badge--done", text: "" }
              : null
      : null;

  return (
    <span
      ref={rootRef}
      className={className ? `sa-mascot ${className}` : "sa-mascot"}
      style={style}
      data-state={canonicalState}
      data-motion={shapeDef.motion}
      data-shape={shapeDef.id}
      data-color={colorDef.id}
      data-interactive={interactive ? "true" : undefined}
      data-poked={poked ? "true" : undefined}
      onClick={interactive ? triggerPoke : undefined}
      aria-hidden="true"
    >
      {image ? (
        <img className="sa-mascot-photo" src={image} alt={name} draggable={false} />
      ) : (
        <svg
          className="sa-mascot-svg"
          viewBox="-18 -18 260 260"
          role="presentation"
          focusable="false"
        >
          <defs>
            <linearGradient id={gradId} x1="18%" y1="10%" x2="86%" y2="92%">
              <stop offset="0%" stopColor={stops[0]} />
              <stop offset="52%" stopColor={stops[1]} />
              <stop offset="100%" stopColor={stops[2]} />
            </linearGradient>
            <radialGradient id={sheenId} cx="32%" cy="24%" r="58%">
              <stop offset="0%" stopColor="#ffffff" stopOpacity="0.42" />
              <stop offset="55%" stopColor="#ffffff" stopOpacity="0.06" />
              <stop offset="100%" stopColor="#ffffff" stopOpacity="0" />
            </radialGradient>
            <radialGradient id={shadowId} cx="68%" cy="78%" r="64%">
              <stop offset="0%" stopColor="#000000" stopOpacity="0.24" />
              <stop offset="60%" stopColor="#000000" stopOpacity="0.04" />
              <stop offset="100%" stopColor="#000000" stopOpacity="0" />
            </radialGradient>
          </defs>

          <g className="sa-mascot-body">
            {/* 立体造型本体 + 顶部高光 + 底部体积阴影 */}
            <path d={shapeDef.path} fill={`url(#${gradId})`} />
            <path d={shapeDef.path} fill={`url(#${sheenId})`} />
            <path d={shapeDef.path} fill={`url(#${shadowId})`} />

            {/* 完成态星光微粒 */}
            <g className="sa-mascot-sparkles" fill={stops[0]}>
              <circle cx="34" cy="24" r="6" />
              <circle cx="196" cy="30" r="5" />
              <circle cx="186" cy="12" r="3.5" />
            </g>

            {/* 眼睛与表情层 */}
            <g className="sa-mascot-gaze">
              <g className="sa-mascot-eyes">
                {/* 常规活力双眸（带斜角灵动感） */}
                <rect
                  className="sa-mascot-eye sa-mascot-eye--left"
                  x={78}
                  y={eyeY - 19}
                  width={18}
                  height={38}
                  rx={9}
                  fill={eyeFill}
                />
                <rect
                  className="sa-mascot-eye sa-mascot-eye--right"
                  x={128}
                  y={eyeY - 16}
                  width={18}
                  height={32}
                  rx={9}
                  fill={eyeFill}
                />

                {/* 开心/抚摸/完成态 ^ ^ 弯月笑眼 */}
                <g
                  className="sa-mascot-eye-happy"
                  fill="none"
                  stroke={eyeFill}
                  strokeWidth="10"
                  strokeLinecap="round"
                  strokeLinejoin="round"
                >
                  <path d={`M 73 ${eyeY + 5} Q 87 ${eyeY - 14} 101 ${eyeY + 5}`} />
                  <path d={`M 123 ${eyeY + 5} Q 137 ${eyeY - 14} 151 ${eyeY + 5}`} />
                </g>

                {/* 停止/休眠态平静闭眼弧线 */}
                <g
                  className="sa-mascot-eye-sleep"
                  fill="none"
                  stroke={eyeFill}
                  strokeWidth="8"
                  strokeLinecap="round"
                >
                  <path d={`M 74 ${eyeY + 2} Q 87 ${eyeY + 12} 100 ${eyeY + 2}`} />
                  <path d={`M 124 ${eyeY + 2} Q 137 ${eyeY + 12} 150 ${eyeY + 2}`} />
                </g>

                {/* 报错态 > < 眼睛 */}
                <g
                  className="sa-mascot-eye-error"
                  fill="none"
                  stroke={eyeFill}
                  strokeWidth="9"
                  strokeLinecap="round"
                  strokeLinejoin="round"
                >
                  <path d={`M 76 ${eyeY - 10} L 96 ${eyeY} L 76 ${eyeY + 10}`} />
                  <path d={`M 148 ${eyeY - 10} L 128 ${eyeY} L 148 ${eyeY + 10}`} />
                </g>

                {/* 开心微笑弧线 */}
                <path
                  className="sa-mascot-smile"
                  d={`M 98 ${eyeY + 26} Q 112 ${eyeY + 40} 126 ${eyeY + 26}`}
                  fill="none"
                  stroke={eyeFill}
                  strokeWidth="7"
                  strokeLinecap="round"
                />
              </g>
            </g>
          </g>
        </svg>
      )}

      {showBadge && isMaster && size >= 28 && (
        <span className="sa-mascot-badge sa-mascot-badge--master" title="中枢主控">
          ★
        </span>
      )}
      {badge && <span className={`sa-mascot-badge ${badge.cls}`}>{badge.text}</span>}
    </span>
  );
}

/** 协作房间（群聊）多 Bot 拼图头像：展示前 3~4 位成员的专属角色模型。 */
export function RoomMascotCollage({
  members,
  size = 28,
}: {
  members: readonly Bot[];
  size?: number;
}) {
  const shown = members.slice(0, members.length > 4 ? 3 : 4);
  const extra = members.length > 4 ? members.length - 3 : 0;
  const count = Math.min(Math.max(members.length, 2), 4);
  const cell = Math.max(10, Math.floor((size - 6) / 2));
  return (
    <span
      className="sa-room-collage"
      data-count={count}
      style={{ "--sa-collage-size": `${size}px` } as CSSProperties}
      aria-hidden="true"
    >
      {shown.map((bot) => (
        <BotMascot
          key={bot.id}
          id={bot.id}
          name={bot.name}
          isMaster={bot.is_master}
          shape={bot.avatar_shape}
          color={bot.avatar_color}
          image={bot.avatar_image}
          size={cell}
          showBadge={false}
        />
      ))}
      {extra > 0 && <span className="sa-room-collage-more">+{extra}</span>}
    </span>
  );
}
