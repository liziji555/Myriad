import {
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
} from "react";
import { HUE_COLORS, hueOf } from "../api/avatar";
import { USER_LANE, type Flow, type FlowNode } from "../api/flow";
import type { MessageId } from "../api/types";
import { useLight } from "../light/LightProvider";

/** 刚发生的传递才飞一道光；打开时已经有的只画线。 */
const ROW = 34;
const TOP = 18;
const COL_MIN = 120;
const USER_COLOR = "#8b857c";

/** 节点样子沿用 Lancer 工作树的事件分类。 */
type Glyph =
  | "checkpoint"
  | "submit"
  | "handoff"
  | "fork"
  | "done"
  | "xref"
  | "wait"
  | "error"
  | "live";

const GLYPH_LABEL: Record<Glyph, string> = {
  checkpoint: "接手",
  submit: "一步",
  handoff: "交接",
  fork: "开任务",
  done: "说完",
  xref: "收到结果",
  wait: "等你批准",
  error: "出错",
  live: "在做",
};

export function glyphOf(node: FlowNode): Glyph {
  if (node.tone === "waiting") return "wait";
  if (node.tone === "error") return "error";
  switch (node.kind) {
    case "ask":
    case "start":
      return "checkpoint";
    case "handoff":
      return "handoff";
    case "task":
      return "fork";
    case "done":
      return "done";
    case "inbox":
      return "xref";
    default:
      return node.tone === "live" ? "live" : "submit";
  }
}

export function laneColors(flow: Flow): Map<string, string> {
  const map = new Map<string, string>();
  for (const lane of flow.lanes)
    map.set(lane.key, lane.botId ? HUE_COLORS[hueOf(lane.botId)] : USER_COLOR);
  return map;
}

/** 点中一个节点时，和它连着的节点、车道。 */
export function relatedTo(flow: Flow, id: string | null): Set<string> | null {
  if (!id) return null;
  const set = new Set([id]);
  for (const edge of flow.edges)
    if (edge.from === id || edge.to === id) set.add(edge.from).add(edge.to);
  return set;
}

interface TreeProps {
  flow: Flow;
  selected: string | null;
  onSelect(node: FlowNode | null): void;
}

/** 右边的工作树：一个 Bot 一列，往下一个节点就是做完了一步；交接是实线，交回结果是虚线。 */
export function WorkTree({ flow, selected, onSelect }: TreeProps) {
  const scroller = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(0);
  const [focused, setFocused] = useState(false);
  const [hover, setHover] = useState<string | null>(null);
  const follow = useRef(true);
  const emit = useLight();
  const seen = useRef<Set<string> | null>(null);

  useLayoutEffect(() => {
    const el = scroller.current;
    if (!el) return;
    const observer = new ResizeObserver(() => setWidth(el.clientWidth));
    observer.observe(el);
    setWidth(el.clientWidth);
    return () => observer.disconnect();
  }, []);

  const colorOf = useMemo(() => laneColors(flow), [flow]);
  const used = useMemo(
    () => new Set(flow.nodes.map((node) => node.lane)),
    [flow.nodes],
  );
  const selectedNode = flow.nodes.find((node) => node.id === selected);
  const lanes = flow.lanes.filter(
    (lane) =>
      lane.key !== USER_LANE &&
      (!focused || used.has(lane.key) || lane.role === "main"),
  );
  const nodes = flow.nodes.filter((node) =>
    lanes.some((lane) => lane.key === node.lane),
  );
  // 行号是全房间的时间顺序；只留有节点的行，空行不占地方。
  const rows = [...new Set(nodes.map((node) => node.row))].sort(
    (a, b) => a - b,
  );
  const rowOf = new Map(rows.map((row, index) => [row, index]));
  const col = Math.max(COL_MIN, width / Math.max(1, lanes.length));
  const total = col * lanes.length;
  const height = TOP * 2 + Math.max(1, rows.length) * ROW;
  const pos = new Map<string, { x: number; y: number }>();
  for (const node of nodes) {
    const index = lanes.findIndex((lane) => lane.key === node.lane);
    pos.set(node.id, {
      x: index * col + 18,
      y: TOP + (rowOf.get(node.row) ?? 0) * ROW,
    });
  }
  const active = hover ?? selected;
  const related = relatedTo(flow, active);
  const focusLane = active
    ? flow.nodes.find((node) => node.id === active)?.lane
    : undefined;

  // 新节点出现时跟到最下面；自己往上翻了就不跟。
  useEffect(() => {
    const el = scroller.current;
    if (el && follow.current) el.scrollTop = el.scrollHeight;
  }, [nodes.length]);

  // 新的派活、交接：从会话里那条消息飞一道光到这个节点。
  useEffect(() => {
    const ids = flow.edges.map((edge) => edge.id);
    if (!seen.current) {
      seen.current = new Set(ids);
      return;
    }
    for (const edge of flow.edges) {
      if (seen.current.has(edge.id)) continue;
      seen.current.add(edge.id);
      const to = flow.nodes.find((node) => node.id === edge.to);
      const from = flow.nodes.find((node) => node.id === edge.from);
      if (!to || !from) continue;
      const glyph = () =>
        scroller.current?.querySelector(
          `[data-tree-node="${CSS.escape(to.id)}"] > i`,
        ) ?? null;
      const chat = () =>
        document.querySelector(
          `[data-message-id="${CSS.escape(from.messageId)}"]`,
        ) ??
        scroller.current?.querySelector(
          `[data-tree-node="${CSS.escape(from.id)}"] > i`,
        ) ??
        null;
      void emit({
        id: `tree-${edge.id}`,
        source: chat,
        target: glyph,
        color: colorOf.get(from.lane) ?? USER_COLOR,
        sourceKind: "message",
      });
    }
  }, [flow.edges, flow.nodes, colorOf, emit]);

  // 从 Bot 链或会话里点中的节点不在视野里时，把工作树滚过去。
  useEffect(() => {
    if (!selected) return;
    scroller.current
      ?.querySelector(`[data-tree-node="${CSS.escape(selected)}"]`)
      ?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }, [selected]);

  useEffect(() => {
    if (!selected) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onSelect(null);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [selected, onSelect]);

  const dimLane = (key: string) =>
    focusLane !== undefined && related !== null && key !== focusLane;
  const dimNode = (id: string) => related !== null && !related.has(id);

  return (
    <aside className="sa-tree" aria-label="工作树">
      <header className="sa-tree-top">
        <strong>工作树 · 泳道</strong>
        <span>
          {lanes.length} 条车道 · {nodes.length} 个节点
        </span>
        <div className="sa-tree-switch" role="group" aria-label="显示哪些车道">
          <button
            type="button"
            aria-pressed={focused}
            onClick={() => setFocused(true)}
          >
            聚焦
          </button>
          <button
            type="button"
            aria-pressed={!focused}
            onClick={() => setFocused(false)}
          >
            全部车道
          </button>
        </div>
      </header>
      <div
        className="sa-tree-scroll"
        ref={scroller}
        onScroll={(event) => {
          const el = event.currentTarget;
          follow.current =
            el.scrollTop + el.clientHeight >= el.scrollHeight - 24;
        }}
        onClick={(event) => {
          if (event.target === event.currentTarget) onSelect(null);
        }}
      >
        <div className="sa-tree-heads" style={{ width: total }}>
          {lanes.map((lane) => {
            const mine = nodes.filter((node) => node.lane === lane.key);
            const live = mine.some(
              (node) => node.tone === "live" || node.tone === "waiting",
            );
            const failed = mine.some((node) => node.tone === "error");
              return (
              <div
                key={lane.key}
                className="sa-tree-head"
                data-dim={dimLane(lane.key) || undefined}
                style={
                  {
                    width: col,
                    "--lane": colorOf.get(lane.key),
                  } as CSSProperties
                }
              >
                <span className="sa-tree-head-name">
                  <i aria-hidden="true" />
                  {lane.name}
                  {lane.role === "main" && <em>主 Agent</em>}
                </span>
                <span
                  className="sa-tree-head-bar"
                  data-live={live || undefined}
                />
                <small>
                  {mine.length} 个节点 ·{" "}
                  {failed && !live
                    ? "出错"
                    : live
                      ? "在做"
                      : mine.length
                        ? "空闲"
                        : "还没开始"}
                </small>
              </div>
            );
          })}
        </div>
        <div className="sa-tree-canvas" style={{ width: total, height }}>
          <svg width={total} height={height} aria-hidden="true">
            {lanes.map((lane, index) => {
              const mine = nodes
                .filter((node) => node.lane === lane.key)
                .map((node) => pos.get(node.id)!);
              const color = colorOf.get(lane.key);
              const x = index * col + 18;
              if (mine.length === 0)
                return (
                  <path
                    key={lane.key}
                    className="sa-tree-rail"
                    data-empty="true"
                    d={`M ${x} ${TOP} V ${height - TOP}`}
                    stroke={color}
                  />
                );
              const last = mine[mine.length - 1];
              return (
                <path
                  key={lane.key}
                  className="sa-tree-rail"
                  data-dim={dimLane(lane.key) || undefined}
                  d={`M ${x} ${mine[0].y} V ${last.y}`}
                  stroke={color}
                />
              );
            })}
            {flow.edges.map((edge) => {
              const a = pos.get(edge.from);
              const b = pos.get(edge.to);
              if (!a || !b || edge.kind === "ask") return null;
              const from = flow.nodes.find((node) => node.id === edge.from)!;
              const to = flow.nodes.find((node) => node.id === edge.to)!;
              const cx = (a.x + b.x) / 2;
              const d =
                Math.abs(a.x - b.x) < 1
                  ? `M ${a.x} ${a.y} C ${a.x + 28} ${a.y}, ${b.x + 28} ${b.y}, ${b.x} ${b.y}`
                  : `M ${a.x} ${a.y} C ${cx} ${a.y}, ${cx} ${b.y}, ${b.x} ${b.y}`;
              const dim =
                related !== null &&
                !(related.has(edge.from) && related.has(edge.to));
              return (
                <path
                  key={edge.id}
                  className="sa-tree-edge"
                  data-kind={edge.kind}
                  data-live={edge.live || undefined}
                  data-dim={dim || undefined}
                  d={d}
                  stroke={
                    edge.kind === "return"
                      ? colorOf.get(from.lane)
                      : colorOf.get(to.lane)
                  }
                />
              );
            })}
          </svg>
          {nodes.map((node) => {
            const p = pos.get(node.id)!;
            const glyph = glyphOf(node);
            return (
              <button
                key={node.id}
                type="button"
                className="sa-tree-node"
                data-tree-node={node.id}
                data-glyph={glyph}
                data-dim={dimNode(node.id) || undefined}
                data-on={node.id === selected || undefined}
                style={
                  {
                    left: p.x - 7,
                    top: p.y - 9,
                    maxWidth: col - 18,
                    "--lane": colorOf.get(node.lane),
                  } as CSSProperties
                }
                title={node.label}
                onMouseEnter={() => setHover(node.id)}
                onMouseLeave={() => setHover(null)}
                onClick={() => onSelect(node.id === selected ? null : node)}
              >
                <i aria-hidden="true" />
                <span>
                  <b>{node.label}</b>
                  <small>{GLYPH_LABEL[glyph]}</small>
                </span>
              </button>
            );
          })}
          {nodes.length === 0 && (
            <p className="sa-tree-empty">
              还没有人开始干活。说一句话，这里会按 Bot 一列一列画出每一步。
            </p>
          )}
        </div>
      </div>
      <footer className="sa-tree-legend" aria-label="图例">
        {(Object.keys(GLYPH_LABEL) as Glyph[]).map((glyph) => (
          <span key={glyph} data-glyph={glyph}>
            <i aria-hidden="true" />
            {GLYPH_LABEL[glyph]}
          </span>
        ))}
        <span data-line="solid">
          <i aria-hidden="true" />
          派活、交接
        </span>
        <span data-line="dash">
          <i aria-hidden="true" />
          交回结果
        </span>
      </footer>
      {selectedNode && (
        <p className="sa-tree-hint">已聚焦「{selectedNode.label}」，按 Esc 或再点一下取消</p>
      )}
    </aside>
  );
}

interface StripProps {
  flow: Flow;
  selected: string | null;
  onSelect(node: FlowNode): void;
}

/** 会话上方的 Bot 链：你每问一句一行，按接手的先后串起来。 */
export function RelayStrip({ flow, selected, onSelect }: StripProps) {
  const colorOf = useMemo(() => laneColors(flow), [flow]);
  const related = relatedTo(flow, selected);
  const starts = flow.rounds.length ? flow.rounds : [0];
  const chains = starts
    .map((start, index) => {
      const end = starts[index + 1] ?? Number.POSITIVE_INFINITY;
      const inRound = flow.nodes
        .filter(
          (node) =>
            node.row >= start && node.row < end && node.lane !== USER_LANE,
        )
        .sort((a, b) => a.row - b.row);
      const order: string[] = [];
      for (const node of inRound)
        if (!order.includes(node.lane)) order.push(node.lane);
      const ask = flow.nodes.find(
        (node) => node.lane === USER_LANE && node.row >= start && node.row < end,
      );
      return { index, ask, order, inRound };
    })
    .filter((chain) => chain.order.length > 0)
    .slice(-4);
  if (chains.length === 0) return null;
  return (
    <div className="sa-relay" aria-label="Bot 链">
      {chains.map((chain) => (
        <div key={chain.index} className="sa-relay-row">
          <span className="sa-relay-zone" title={chain.ask?.label}>
            第 {chain.index + 1} 句
          </span>
          {chain.order.map((key, at) => {
            const lane = flow.lanes.find((l) => l.key === key)!;
            const mine = chain.inRound.filter((node) => node.lane === key);
            const state = mine.some((node) => node.tone === "waiting")
              ? "wait"
              : mine.some((node) => node.tone === "live")
                ? "live"
                : mine.some((node) => node.tone === "error")
                  ? "error"
                  : "ok";
            const on = mine.some((node) => related?.has(node.id));
            return (
              <span key={key} className="sa-relay-step">
                {at > 0 && <span className="sa-relay-arrow">→</span>}
                <button
                  type="button"
                  className="sa-relay-chip"
                  data-state={state}
                  data-on={on || undefined}
                  data-dim={(related !== null && !on) || undefined}
                  style={{ "--lane": colorOf.get(key) } as CSSProperties}
                  onClick={() => onSelect(mine[0])}
                >
                  <i aria-hidden="true" />
                  {lane.name}
                  {state === "ok" && <small>✓</small>}
                </button>
              </span>
            );
          })}
        </div>
      ))}
    </div>
  );
}


/** 点中节点时，从会话里那条消息到工作树节点画一根曲线；两头都看得见才画。 */
export function FocusCurve({
  nodeId,
  messageId,
  color,
}: {
  nodeId: string | null;
  messageId: MessageId | null;
  color: string;
}) {
  const [d, setD] = useState<string | null>(null);
  useEffect(() => {
    if (!nodeId || !messageId) {
      setD(null);
      return;
    }
    const message = () =>
      document.querySelector<HTMLElement>(
        `[data-message-id="${CSS.escape(messageId)}"]`,
      );
    message()?.setAttribute("data-picked", "true");
    let frame = 0;
    let last = "";
    const measure = () => {
      frame = requestAnimationFrame(measure);
      const a = message();
      const b = document.querySelector(
        `[data-tree-node="${CSS.escape(nodeId)}"] > i`,
      );
      const pane = a?.closest(".sa-timeline, .sa-main");
      const scroll = b?.closest(".sa-tree-scroll");
      let next = "";
      if (a && b && pane && scroll) {
        const ra = a.getBoundingClientRect();
        const rb = b.getBoundingClientRect();
        const pa = pane.getBoundingClientRect();
        const pb = scroll.getBoundingClientRect();
        const y1 = Math.min(Math.max(ra.top + 18, ra.top), ra.bottom);
        const y2 = rb.top + rb.height / 2;
        const x2 = rb.left + rb.width / 2;
        const visible =
          y1 > pa.top && y1 < pa.bottom && y2 > pb.top && y2 < pb.bottom &&
          x2 > pb.left && x2 < pb.right;
        if (visible) {
          const x1 = Math.min(ra.right, pa.right) - 4;
          const bend = Math.max(60, (x2 - x1) * 0.55);
          next = `M ${x1} ${y1} C ${x1 + bend} ${y1}, ${x2 - bend} ${y2}, ${x2} ${y2}`;
        }
      }
      if (next !== last) {
        last = next;
        setD(next || null);
      }
    };
    measure();
    return () => {
      cancelAnimationFrame(frame);
      message()?.removeAttribute("data-picked");
    };
  }, [nodeId, messageId]);
  if (!d) return null;
  return (
    <svg className="sa-focus-curve" aria-hidden="true">
      <path d={d} stroke={color} />
    </svg>
  );
}
