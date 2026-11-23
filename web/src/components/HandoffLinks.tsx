import { useEffect, useLayoutEffect, useRef, useState } from "react";
import type { Handoff } from "../api/handoffs";
import { useLight } from "../light/LightProvider";

/** 刚发生的交接才飞一道光；打开页面时已经有的只画线。 */
const FRESH_MS = 20_000;

interface Drawn {
  id: string;
  d: string;
  a: { x: number; y: number };
  b: { x: number; y: number };
  from: string;
  to: string;
  live: boolean;
}

const nodeOf = (root: Element | null, id: string) =>
  root?.querySelector(`[data-message-id="${CSS.escape(id)}"] > .sa-line-node`) ?? null;

const hueOf = (node: Element) => getComputedStyle(node).getPropertyValue("--hue").trim() || "currentColor";

/**
 * 主线左边的交接连线：从交出去的那个 Bot 的头像弯到接手的 Bot 的头像。
 * 对方还在做时线上有光在流动，刚交接的那一下飞一道光过去。
 */
export function HandoffLinks({ handoffs }: { handoffs: readonly Handoff[] }) {
  const self = useRef<HTMLLIElement>(null);
  const [drawn, setDrawn] = useState<Drawn[]>([]);
  const [tick, setTick] = useState(0);
  const emit = useLight();

  useEffect(() => {
    const list = self.current?.parentElement;
    if (!list || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() => setTick((n) => n + 1));
    observer.observe(list);
    return () => observer.disconnect();
  }, []);

  useLayoutEffect(() => {
    const list = self.current?.parentElement;
    if (!list) return;
    const box = list.getBoundingClientRect();
    const scroller = list.parentElement?.getBoundingClientRect();
    const next: Drawn[] = [];
    handoffs.forEach((handoff, index) => {
      const from = nodeOf(list, handoff.from);
      const to = nodeOf(list, handoff.to);
      if (!from || !to) return;
      const ra = from.getBoundingClientRect();
      const rb = to.getBoundingClientRect();
      const a = { x: ra.x + ra.width / 2 - box.x, y: ra.y + ra.height / 2 - box.y };
      const b = { x: rb.x + rb.width / 2 - box.x, y: rb.y + rb.height / 2 - box.y };
      // 往左弯出主线，弯多少看左边还有多少地方；叠在一起的几根错开一点。
      const room = a.x + (scroller ? box.x - scroller.x : 0) - 6;
      const bend = Math.max(10, Math.min(room, 18 + (index % 3) * 9));
      const pull = Math.min(48, Math.abs(b.y - a.y) / 3);
      const d = `M ${a.x} ${a.y} C ${a.x - bend} ${a.y + pull} ${b.x - bend} ${b.y - pull} ${b.x} ${b.y}`;
      next.push({ id: handoff.id, d, a, b, from: hueOf(from), to: hueOf(to), live: handoff.live });
    });
    setDrawn((old) => (JSON.stringify(old) === JSON.stringify(next) ? old : next));
  });

  // 第一次画出来的都当作见过；之后新出现、而且是刚说的交接才飞光。
  const seen = useRef<Set<string> | null>(null);
  useEffect(() => {
    const list = self.current?.parentElement ?? null;
    const fresh = seen.current
      ? handoffs.filter((h) => !seen.current!.has(h.id) && Date.now() - h.at < FRESH_MS)
      : [];
    seen.current = new Set([...(seen.current ?? []), ...handoffs.map((h) => h.id)]);
    for (const handoff of fresh) {
      const source = nodeOf(list, handoff.from);
      if (!source) continue;
      void emit({
        id: `handoff-${handoff.id}`,
        source: () => nodeOf(list, handoff.from),
        target: () => nodeOf(list, handoff.to),
        color: hueOf(source),
        sourceKind: "message",
      });
    }
  }, [handoffs, emit]);

  return (
    <li ref={self} className="sa-links" aria-hidden="true" data-tick={tick}>
      <svg>
        {drawn.map((link, index) => (
          <g key={link.id} className="sa-link" data-live={link.live}>
            <defs>
              <linearGradient
                id={`sa-link-${index}`}
                gradientUnits="userSpaceOnUse"
                x1={link.a.x}
                y1={link.a.y}
                x2={link.b.x}
                y2={link.b.y}
              >
                <stop offset="0" stopColor={link.from} />
                <stop offset="1" stopColor={link.to} />
              </linearGradient>
            </defs>
            <path className="sa-link-line" d={link.d} stroke={`url(#sa-link-${index})`} />
            {link.live && (
              <>
                <path className="sa-link-glow" d={link.d} stroke={link.to} />
                <path className="sa-link-flow" d={link.d} stroke={`url(#sa-link-${index})`} />
                <circle className="sa-link-head" r="2.6" fill={link.to}>
                  <animateMotion dur="1.6s" repeatCount="indefinite" path={link.d} />
                </circle>
              </>
            )}
          </g>
        ))}
      </svg>
    </li>
  );
}
