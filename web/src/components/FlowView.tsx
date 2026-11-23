import {
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
} from "react";
import { HUE_COLORS, hueOf } from "../api/avatar";
import {
  USER_LANE,
  type Flow,
  type FlowEdge,
  type FlowNode,
} from "../api/flow";
import type { Presence } from "../api/presence";
import type { MessageId } from "../api/types";
import { useLight } from "../light/LightProvider";
import { BotAvatar } from "./BotAvatar";

/** 刚发生的传递才飞一道光；打开时已经有的只画线。 */
const FRESH_MS = 20_000;
const HEAD = 76;
const ROW = 46;
const USER_W = 176;
/** 还没干过活的 Bot 窄一点，把地方让给在干活的。 */
const IDLE_W = 156;
const LANE_MIN = 184;
const LANE_MAX = 248;
const USER_COLOR = "#8b857c";

interface Props {
  flow: Flow;
  presence: readonly Presence[];
  /** 回到对话里看这条消息。 */
  onOpen(messageId: MessageId): void;
}

interface Point {
  x: number;
  y: number;
}

const ROLE: Record<string, string> = { main: "主 Agent", bot: "成员" };

/**
 * 线之间的传递走「轨道」：先顺着来源的竖线往下，在接手节点上面半行的空隙里横过去，再落到节点上。
 * 横的那一段总在两行之间，不压字，几条线也不会缠在一起。
 */
function channel(a: Point, b: Point): { d: string; mid: Point } {
  const ym = b.y - ROW / 2;
  const dx = b.x - a.x;
  if (Math.abs(dx) < 1)
    return {
      d: `M ${a.x} ${a.y} L ${b.x} ${b.y}`,
      mid: { x: a.x, y: (a.y + b.y) / 2 },
    };
  const s = Math.sign(dx);
  const r = Math.min(12, Math.abs(dx) / 2, Math.max(2, ym - a.y));
  const d = [
    `M ${a.x} ${a.y}`,
    `L ${a.x} ${ym - r}`,
    `Q ${a.x} ${ym} ${a.x + s * r} ${ym}`,
    `L ${b.x - s * r} ${ym}`,
    `Q ${b.x} ${ym} ${b.x} ${ym + r}`,
    `L ${b.x} ${b.y}`,
  ].join(" ");
  return { d, mid: { x: (a.x + b.x) / 2, y: ym } };
}

/**
 * 协作视图：每个 Bot 一条竖线，主 Agent 在你旁边，下面干活的 Bot 依次排开。
 * 往下一个节点就是做完一步；派活、交接、交回结果是线之间的光。
 */
export function FlowView({ flow, presence, onOpen }: Props) {
  const scroller = useRef<HTMLDivElement>(null);
  const canvas = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(0);
  const [focus, setFocus] = useState<string | null>(null);
  const stick = useRef(true);
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

  const used = useMemo(
    () => new Set(flow.nodes.map((node) => node.lane)),
    [flow.nodes],
  );
  const bots = flow.lanes.filter((lane) => lane.key !== USER_LANE);
  const working = bots.filter((lane) => used.has(lane.key)).length;
  const idle = bots.length - working;
  const laneW = Math.min(
    LANE_MAX,
    Math.max(
      LANE_MIN,
      (width - USER_W - idle * IDLE_W - 32) / Math.max(1, working),
    ),
  );
  const widthOf = (key: string) =>
    key === USER_LANE ? USER_W : used.has(key) ? laneW : IDLE_W;
  const total = USER_W + working * laneW + idle * IDLE_W;
  const left = Math.max(16, (width - total) / 2);
  const xOf = useMemo(() => {
    const map = new Map<string, number>();
    let x = left;
    for (const lane of flow.lanes) {
      map.set(lane.key, x + 22);
      x +=
        lane.key === USER_LANE ? USER_W : used.has(lane.key) ? laneW : IDLE_W;
    }
    return map;
  }, [flow.lanes, left, laneW, used]);
  const colorOf = useMemo(() => {
    const map = new Map<string, string>();
    for (const lane of flow.lanes)
      map.set(
        lane.key,
        lane.botId ? HUE_COLORS[hueOf(lane.botId)] : USER_COLOR,
      );
    return map;
  }, [flow.lanes]);
  const height = HEAD + Math.max(1, flow.rows) * ROW + 40;
  const at = (node: FlowNode): Point => ({
    x: xOf.get(node.lane) ?? 0,
    y: HEAD + 26 + node.row * ROW,
  });
  const nodeById = useMemo(
    () => new Map(flow.nodes.map((node) => [node.id, node])),
    [flow.nodes],
  );

  // 鼠标停在一个节点上：和它相连的线、节点亮着，其他的退到后面。
  const related = useMemo(() => {
    if (!focus) return null;
    const ids = new Set<string>([focus]);
    const lane = focus.startsWith("lane:") ? focus.slice(5) : null;
    for (const edge of flow.edges) {
      const hit = lane
        ? nodeById.get(edge.from)?.lane === lane ||
          nodeById.get(edge.to)?.lane === lane
        : edge.from === focus || edge.to === focus;
      if (hit) ids.add(edge.id).add(edge.from).add(edge.to);
    }
    if (lane)
      for (const node of flow.nodes) if (node.lane === lane) ids.add(node.id);
    return ids;
  }, [focus, flow.edges, flow.nodes, nodeById]);

  // 新的传递：沿着线飞一道光，落到接手的节点上。
  useEffect(() => {
    const root = canvas.current;
    if (!seen.current) {
      seen.current = new Set(flow.edges.map((edge) => edge.id));
      return;
    }
    for (const edge of flow.edges) {
      if (seen.current.has(edge.id)) continue;
      seen.current.add(edge.id);
      if (Date.now() - edge.at > FRESH_MS) continue;
      const find = (id: string) => () =>
        root?.querySelector(`[data-flow-node="${CSS.escape(id)}"] > i`) ?? null;
      const source = nodeById.get(edge.from);
      void emit({
        id: `flow-${edge.id}-${Date.now()}`,
        source: find(edge.from),
        target: find(edge.to),
        color: (source && colorOf.get(source.lane)) ?? USER_COLOR,
        duration: 1000,
        sourceKind: "message",
      });
    }
  }, [flow.edges, nodeById, colorOf, emit]);

  // 新节点出现时跟着往下滚，人往上翻走了就不跟。
  useLayoutEffect(() => {
    const el = scroller.current;
    if (el && stick.current) el.scrollTop = el.scrollHeight;
  }, [flow.rows]);

  const busy = new Map(presence.map((row) => [row.botId, row]));
  const runs = new Map<string, FlowNode[]>();
  for (const node of flow.nodes) {
    const key = `${node.lane}|${node.run}`;
    runs.set(key, [...(runs.get(key) ?? []), node]);
  }
  const dim = (id: string) =>
    related && !related.has(id) ? "true" : undefined;

  return (
    <div
      ref={scroller}
      className="sa-flow"
      aria-label="协作视图"
      onScroll={(event) => {
        const el = event.currentTarget;
        stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
      }}
    >
      <div
        ref={canvas}
        className="sa-flow-canvas"
        data-focus={related ? "true" : undefined}
        style={{ width: Math.max(width, total + 32), height }}
      >
        <svg
          className="sa-flow-svg"
          width="100%"
          height={height}
          aria-hidden="true"
        >
          <defs>
            {flow.lanes.map((lane) => (
              <linearGradient
                key={lane.key}
                id={`band-${lane.key.replace(/[^a-zA-Z0-9_-]/g, "_")}`}
                x1="0"
                y1="0"
                x2="0"
                y2="1"
              >
                <stop
                  offset="0"
                  stopColor={colorOf.get(lane.key)}
                  stopOpacity="0"
                />
                <stop
                  offset="1"
                  stopColor={colorOf.get(lane.key)}
                  stopOpacity="0.08"
                />
              </linearGradient>
            ))}
            {flow.edges.map((edge) => {
              const a = nodeById.get(edge.from);
              const b = nodeById.get(edge.to);
              if (!a || !b) return null;
              const p = at(a);
              const q = at(b);
              return (
                <linearGradient
                  key={edge.id}
                  id={gradientId(edge)}
                  gradientUnits="userSpaceOnUse"
                  x1={p.x}
                  y1={p.y}
                  x2={q.x}
                  y2={q.y}
                >
                  <stop offset="0" stopColor={colorOf.get(a.lane)} />
                  <stop offset="1" stopColor={colorOf.get(b.lane)} />
                </linearGradient>
              );
            })}
          </defs>

          {flow.rounds.map((row, index) => (
            <g key={`round-${row}`} className="sa-flow-round">
              <line
                x1={8}
                x2={Math.max(width, total + 32) - 8}
                y1={HEAD + 4 + row * ROW}
                y2={HEAD + 4 + row * ROW}
              />
              <text x={12} y={HEAD + 16 + row * ROW}>
                {`第 ${index + 1} 轮`}
              </text>
            </g>
          ))}

          {flow.lanes.map((lane) => {
            const live = flow.nodes.find(
              (node) =>
                node.lane === lane.key &&
                (node.tone === "live" || node.tone === "waiting"),
            );
            if (!live) return null;
            const top =
              Math.min(
                ...flow.nodes
                  .filter(
                    (node) => node.lane === lane.key && node.run === live.run,
                  )
                  .map((node) => at(node).y),
              ) -
              ROW / 2;
            const x = xOf.get(lane.key) ?? 0;
            return (
              <rect
                key={`band-${lane.key}`}
                className="sa-flow-band"
                x={x - 22}
                y={top}
                width={widthOf(lane.key) - 12}
                height={height - top - 8}
                rx={14}
                fill={`url(#band-${lane.key.replace(/[^a-zA-Z0-9_-]/g, "_")})`}
              />
            );
          })}

          {flow.lanes.map((lane) => {
            const x = xOf.get(lane.key) ?? 0;
            return (
              <line
                key={lane.key}
                className="sa-flow-track"
                data-dim={
                  dim(`lane:${lane.key}`) && !related?.has(`lane:${lane.key}`)
                    ? "true"
                    : undefined
                }
                x1={x}
                x2={x}
                y1={HEAD}
                y2={height - 8}
                stroke={colorOf.get(lane.key)}
              />
            );
          })}

          {[...runs.values()].map((group) => {
            if (group.length < 2) return null;
            const first = at(group[0]!);
            const last = at(group.at(-1)!);
            const live =
              group.at(-1)!.tone === "live" || group.at(-1)!.tone === "waiting";
            const color = colorOf.get(group[0]!.lane);
            const key = `${group[0]!.lane}|${group[0]!.run}`;
            const faded =
              related && !group.some((node) => related.has(node.id));
            return (
              <g
                key={key}
                className="sa-flow-run"
                data-live={live}
                data-dim={faded ? "true" : undefined}
              >
                <line
                  className="sa-flow-run-glow"
                  x1={first.x}
                  x2={last.x}
                  y1={first.y}
                  y2={last.y}
                  stroke={color}
                />
                <line
                  className="sa-flow-run-line"
                  x1={first.x}
                  x2={last.x}
                  y1={first.y}
                  y2={last.y}
                  stroke={color}
                />
                {live && (
                  <line
                    className="sa-flow-run-stream"
                    x1={first.x}
                    x2={last.x}
                    y1={first.y}
                    y2={last.y}
                    stroke={color}
                  />
                )}
              </g>
            );
          })}

          {flow.edges.map((edge) => {
            const a = nodeById.get(edge.from);
            const b = nodeById.get(edge.to);
            if (!a || !b) return null;
            const p = at(a);
            const q = at(b);
            const { d, mid: m } = channel(p, q);
            const pathId = `flow-path-${gradientId(edge)}`;
            return (
              <g
                key={edge.id}
                className="sa-flow-edge"
                data-kind={edge.kind}
                data-live={edge.live}
                data-dim={dim(edge.id)}
              >
                <path
                  id={pathId}
                  className="sa-flow-edge-glow"
                  d={d}
                  stroke={`url(#${gradientId(edge)})`}
                />
                <path
                  className="sa-flow-edge-line"
                  d={d}
                  stroke={`url(#${gradientId(edge)})`}
                />
                {edge.live && (
                  <path
                    className="sa-flow-edge-stream"
                    d={d}
                    stroke={`url(#${gradientId(edge)})`}
                  />
                )}
                {edge.kind === "peer" && (
                  <rect
                    className="sa-flow-edge-mark"
                    x={m.x - 3.5}
                    y={m.y - 3.5}
                    width={7}
                    height={7}
                    transform={`rotate(45 ${m.x} ${m.y})`}
                    stroke={`url(#${gradientId(edge)})`}
                  />
                )}
                {edge.live &&
                  [0, 0.6, 1.2].map((begin) => (
                    <circle
                      key={begin}
                      className="sa-flow-comet"
                      r={2.6}
                      fill={colorOf.get(b.lane)}
                    >
                      <animateMotion
                        dur="1.8s"
                        begin={`${begin}s`}
                        repeatCount="indefinite"
                        rotate="auto"
                      >
                        <mpath href={`#${pathId}`} />
                      </animateMotion>
                    </circle>
                  ))}
              </g>
            );
          })}
        </svg>

        <div
          className="sa-flow-head"
          style={{ width: Math.max(width, total + 32) }}
        >
          {flow.lanes.map((lane) => {
            const x = xOf.get(lane.key) ?? 0;
            const now = lane.botId ? busy.get(lane.botId) : undefined;
            const count = flow.nodes.filter(
              (node) => node.lane === lane.key && node.kind === "step",
            ).length;
            return (
              <div
                key={lane.key}
                className="sa-flow-lane"
                data-role={lane.role}
                data-state={now?.state}
                style={
                  {
                    left: x - 14,
                    width: widthOf(lane.key) - 16,
                  } as CSSProperties
                }
                onMouseEnter={() => setFocus(`lane:${lane.key}`)}
                onMouseLeave={() => setFocus(null)}
              >
                {lane.botId ? (
                  <BotAvatar
                    id={lane.botId}
                    name={lane.name}
                    state={now?.state}
                    className="sa-flow-avatar"
                  />
                ) : (
                  <span
                    className="sa-avatar sa-flow-avatar"
                    data-who="user"
                    aria-hidden="true"
                  >
                    你
                  </span>
                )}
                <span className="sa-flow-lane-text">
                  <strong>
                    {lane.name}
                    {ROLE[lane.role] && (
                      <em data-role={lane.role}>{ROLE[lane.role]}</em>
                    )}
                  </strong>
                  <small>
                    {lane.role === "user"
                      ? "提问、验收"
                      : now && now.state !== "idle" && now.state !== "off"
                        ? now.text
                        : count > 0
                          ? `做完 ${count} 步`
                          : "空闲"}
                  </small>
                </span>
              </div>
            );
          })}
        </div>

        {flow.nodes.map((node) => {
          const p = at(node);
          return (
            <button
              key={node.id}
              type="button"
              className="sa-flow-node"
              data-flow-node={node.id}
              data-kind={node.kind}
              data-tone={node.tone}
              data-dim={dim(node.id)}
              style={
                {
                  left: p.x,
                  top: p.y,
                  "--node": colorOf.get(node.lane),
                  maxWidth: widthOf(node.lane) - 12,
                } as CSSProperties
              }
              title={node.label}
              onClick={() => onOpen(node.messageId)}
              onMouseEnter={() => setFocus(node.id)}
              onMouseLeave={() => setFocus(null)}
            >
              <i aria-hidden="true" />
              <span>{node.label}</span>
            </button>
          );
        })}

        {flow.nodes.length === 0 && (
          <p className="sa-flow-empty">
            还没有人开始干活。说一句话，或者点名几个 Bot
            分头去做，这里会画出谁把活交给了谁。
          </p>
        )}
      </div>
    </div>
  );
}

function gradientId(edge: FlowEdge): string {
  return `flow-${edge.id.replace(/[^a-zA-Z0-9_-]/g, "_")}`;
}
