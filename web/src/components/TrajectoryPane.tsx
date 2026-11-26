import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
} from "react";
import { HUE_COLORS, hueOf } from "../api/avatar";
import { api } from "../api/client";
import { USER_LANE, type Flow, type FlowNode } from "../api/flow";
import type { Presence } from "../api/presence";
import type { Bot, Harness, HarnessDesk, MessageId, RoomId } from "../api/types";
import { useLight } from "../light/LightProvider";
import { Icon } from "./Icon";

const FRESH_MS = 20_000;
const USER_COLOR = "#78716c";

const SHORT_HARNESS: Record<Harness, string> = {
  builtin: "Built-in",
  claude_code: "Claude",
  codex: "Codex",
  open_code: "OpenCode",
  gemini: "Gemini",
  kimi: "Kimi",
  acp: "ACP",
  command: "CLI",
};

interface Props {
  roomId: RoomId;
  flow: Flow;
  bots: readonly Bot[];
  presence: readonly Presence[];
  onOpen(messageId: MessageId): void;
  onShowDiff(): void;
  onExpandFlow(): void;
  onClose(): void;
}

interface Arc {
  id: string;
  d: string;
  live: boolean;
}

/**
 * Lancer 风格的常驻右侧「多泳道工作树轨迹（Worktree Trajectory）」：
 * - 与左侧对话同屏并排展示，每个 Agent 一列纵向工作树泳道；
 * - 新消息、派活、分叉或交回结果时，调用 `light-ui` 从左侧消息头或顶栏胶囊飞出一道三次贝塞尔弧形渐变流光落到右侧工作树节点上；
 * - 鼠标悬停在任意工作树节点上时，画出跨栏关联弧线（Focus Bridge Curve）连回左侧对应消息。
 */
export function TrajectoryPane({
  roomId,
  flow,
  bots,
  presence,
  onOpen,
  onShowDiff,
  onExpandFlow,
  onClose,
}: Props) {
  const emit = useLight();
  const rootRef = useRef<HTMLElement>(null);
  const scrollRef = useRef<HTMLDivElement>(null);
  const [arcs, setArcs] = useState<Arc[]>([]);
  const [bridgePath, setBridgePath] = useState<string | null>(null);
  const [focusedNode, setFocusedNode] = useState<string | null>(null);
  const [desks, setDesks] = useState<readonly HarnessDesk[]>([]);
  const seenNodes = useRef<Set<string> | null>(null);
  const seenEdges = useRef<Set<string> | null>(null);
  const stickBottom = useRef(true);

  const hasCliBots = useMemo(
    () =>
      flow.lanes.some((lane) => {
        const bot = lane.botId ? bots.find((b) => b.id === lane.botId) : null;
        return bot && bot.harness !== "builtin";
      }),
    [flow.lanes, bots],
  );

  // 拉取各 CLI Agent 的独立工位（Worktree / 分支改动文件数）。
  useEffect(() => {
    if (!hasCliBots) {
      setDesks([]);
      return;
    }
    let alive = true;
    api.listDesks(roomId).then(
      (found) => alive && setDesks(found),
      () => {},
    );
    return () => {
      alive = false;
    };
  }, [roomId, hasCliBots, flow.nodes.length]);

  const colorOf = useMemo(() => {
    const map = new Map<string, string>();
    for (const lane of flow.lanes) {
      map.set(lane.key, lane.botId ? HUE_COLORS[hueOf(lane.botId)] : USER_COLOR);
    }
    return map;
  }, [flow.lanes]);

  const nodeById = useMemo(() => new Map(flow.nodes.map((n) => [n.id, n])), [flow.nodes]);
  const presenceByBot = useMemo(() => new Map(presence.map((p) => [p.botId, p])), [presence]);
  const deskByBot = useMemo(() => new Map(desks.map((d) => [d.bot_id, d])), [desks]);

  // 当房间里有 Bot 时，右侧泳道优先展示各 Bot 工作树列（用户发言通过跨栏飞线射入对应主/子泳道）；没有 Bot 时展示全部列。
  const visibleLanes = useMemo(() => {
    const botLanes = flow.lanes.filter((l) => l.key !== USER_LANE);
    return botLanes.length > 0 ? botLanes : flow.lanes;
  }, [flow.lanes]);

  const nodesByLane = useMemo(() => {
    const map = new Map<string, FlowNode[]>();
    for (const lane of visibleLanes) map.set(lane.key, []);
    for (const node of flow.nodes) {
      const list = map.get(node.lane);
      if (list) list.push(node);
    }
    return map;
  }, [visibleLanes, flow.nodes]);

  const findVisibleChatAnchor = useCallback((messageId: MessageId, botId: string | null): Element | null => {
    const msgEl = document.querySelector(`[data-message-id="${CSS.escape(messageId)}"]`);
    if (msgEl) {
      const anchor = msgEl.querySelector(".sa-msg-head") ?? msgEl.querySelector(".sa-line-node") ?? msgEl;
      const rect = anchor.getBoundingClientRect();
      if (rect.width > 0 && rect.bottom > 44 && rect.top < window.innerHeight - 24) {
        return anchor;
      }
    }
    if (botId) {
      const chip = document.querySelector(`[data-presence-bot="${CSS.escape(botId)}"]`);
      if (chip) {
        const rect = chip.getBoundingClientRect();
        if (rect.width > 0 && rect.height > 0) return chip;
      }
    }
    return null;
  }, []);

  const findTreeGlyph = useCallback((nodeId: string): Element | null => {
    return rootRef.current?.querySelector(`[data-traj-node="${CSS.escape(nodeId)}"] .sa-traj-glyph`) ?? null;
  }, []);

  // 测量并重绘泳道之间的交接弧线（handoffSvg）。
  const measureArcs = useCallback(() => {
    const scroller = scrollRef.current;
    if (!scroller) return;
    const sr = scroller.getBoundingClientRect();
    if (!sr.width) return;
    const next: Arc[] = [];
    for (const edge of flow.edges) {
      const a = nodeById.get(edge.from);
      const b = nodeById.get(edge.to);
      if (!a || !b || a.lane === b.lane) continue;
      const g1 = findTreeGlyph(edge.from);
      const g2 = findTreeGlyph(edge.to);
      if (!g1 || !g2) continue;
      const r1 = g1.getBoundingClientRect();
      const r2 = g2.getBoundingClientRect();
      if (!r1.width || !r2.width) continue;
      const x1 = r1.left + r1.width / 2 - sr.left + scroller.scrollLeft;
      const y1 = r1.top + r1.height / 2 - sr.top + scroller.scrollTop;
      const x2 = r2.left + r2.width / 2 - sr.left + scroller.scrollLeft;
      const y2 = r2.top + r2.height / 2 - sr.top + scroller.scrollTop;
      const my = (y1 + y2) / 2;
      next.push({
        id: edge.id,
        d: `M ${x1} ${y1} C ${x1} ${my}, ${x2} ${my}, ${x2} ${y2}`,
        live: edge.live,
      });
    }
    setArcs(next);
  }, [flow.edges, nodeById, findTreeGlyph]);

  useLayoutEffect(() => {
    measureArcs();
    const scroller = scrollRef.current;
    if (!scroller) return;
    if (stickBottom.current) {
      scroller.scrollTop = scroller.scrollHeight;
    }
    const observer = new ResizeObserver(() => measureArcs());
    observer.observe(scroller);
    return () => observer.disconnect();
  }, [flow.nodes, flow.edges, visibleLanes.length, measureArcs]);

  // 新事件实时发射跨栏弧形流光（Hydration 静默建基线，刷新历史不乱飞）。
  useEffect(() => {
    if (!seenNodes.current || !seenEdges.current) {
      seenNodes.current = new Set(flow.nodes.map((n) => n.id));
      seenEdges.current = new Set(flow.edges.map((e) => e.id));
      return;
    }
    const now = Date.now();
    for (const node of flow.nodes) {
      if (seenNodes.current.has(node.id)) continue;
      seenNodes.current.add(node.id);
      if (node.lane === USER_LANE) continue;
      const lane = flow.lanes.find((l) => l.key === node.lane);
      const botId = lane?.botId ?? null;
      const color = colorOf.get(node.lane) ?? USER_COLOR;
      void emit({
        id: `traj-node-${node.id}-${now}`,
        source: () => findVisibleChatAnchor(node.messageId, botId),
        target: () => findTreeGlyph(node.id),
        color,
        duration: 1150,
        sourceKind: "message",
      });
    }
    for (const edge of flow.edges) {
      if (seenEdges.current.has(edge.id)) continue;
      seenEdges.current.add(edge.id);
      if (now - edge.at > FRESH_MS) continue;
      const a = nodeById.get(edge.from);
      const b = nodeById.get(edge.to);
      if (!a || !b || a.lane === b.lane) continue;
      const color = colorOf.get(b.lane) ?? USER_COLOR;
      void emit({
        id: `traj-edge-${edge.id}-${now}`,
        source: () =>
          a.lane === USER_LANE
            ? findVisibleChatAnchor(a.messageId, null)
            : findTreeGlyph(edge.from),
        target: () => findTreeGlyph(edge.to),
        color,
        duration: 1100,
        sourceKind: "message",
      });
    }
  }, [flow.nodes, flow.edges, flow.lanes, nodeById, colorOf, emit, findVisibleChatAnchor, findTreeGlyph]);

  // 手动点击「重播飞线」：按时间顺序重演最近几步从对话/顶栏飞入工作树的流线。
  const replayFlights = () => {
    const targetNodes = flow.nodes.filter((n) => n.lane !== USER_LANE).slice(-6);
    targetNodes.forEach((node, index) => {
      const lane = flow.lanes.find((l) => l.key === node.lane);
      const botId = lane?.botId ?? null;
      const color = colorOf.get(node.lane) ?? USER_COLOR;
      window.setTimeout(() => {
        void emit({
          id: `replay-${node.id}-${Date.now()}-${index}`,
          source: () => findVisibleChatAnchor(node.messageId, botId),
          target: () => findTreeGlyph(node.id),
          color,
          duration: 1150,
          sourceKind: index % 2 === 1 && botId ? "model" : "message",
        });
      }, index * 260);
    });
  };

  // 悬停右侧工作树节点：画出跨栏关联青色弧线连回左侧对应消息。
  const updateFocusBridge = (node: FlowNode | null) => {
    if (!node) {
      setFocusedNode(null);
      setBridgePath(null);
      return;
    }
    setFocusedNode(node.id);
    const lane = flow.lanes.find((l) => l.key === node.lane);
    const source = findVisibleChatAnchor(node.messageId, lane?.botId ?? null);
    const target = findTreeGlyph(node.id);
    if (!source || !target) {
      setBridgePath(null);
      return;
    }
    const r1 = source.getBoundingClientRect();
    const r2 = target.getBoundingClientRect();
    const x1 = r1.left + r1.width / 2;
    const y1 = r1.top + r1.height / 2;
    const x2 = r2.left + r2.width / 2;
    const y2 = r2.top + r2.height / 2;
    const mx = (x1 + x2) / 2;
    const lift = Math.min(68, Math.abs(x2 - x1) * 0.14);
    setBridgePath(`M ${x1} ${y1} C ${mx} ${y1 - lift}, ${mx} ${y2 - lift}, ${x2} ${y2}`);
  };

  const totalDeskChanges = desks.reduce((sum, d) => sum + d.changes.length, 0);

  return (
    <aside ref={rootRef} className="sa-trajectory" aria-label="工作树轨迹">
      {bridgePath && (
        <svg className="sa-traj-bridge" aria-hidden="true">
          <path d={bridgePath} />
        </svg>
      )}

      <header className="sa-traj-head">
        <span className="sa-traj-title">工作树轨迹</span>
        {totalDeskChanges > 0 && (
          <button type="button" className="sa-traj-diff-pill" onClick={onShowDiff} title="查看工位改动 Diff">
            +{totalDeskChanges} 改动
          </button>
        )}
        <div className="sa-traj-actions">
          <button
            type="button"
            className="sa-traj-replay"
            disabled={flow.nodes.every((n) => n.lane === USER_LANE)}
            onClick={replayFlights}
            title="重播跨栏飞线流光"
          >
            <Icon name="play" size={11} />
            <span>飞线</span>
          </button>
          <button
            type="button"
            className="sa-traj-replay"
            onClick={onExpandFlow}
            title="展开全屏协作流线大图"
          >
            <Icon name="graph" size={11} />
            <span>大图</span>
          </button>
          <button
            type="button"
            className="sa-traj-close"
            onClick={onClose}
            aria-label="收起工作树"
            title="收起工作树"
          >
            ×
          </button>
        </div>
      </header>

      <div
        ref={scrollRef}
        className="sa-traj-scroll"
        onScroll={(e) => {
          const el = e.currentTarget;
          stickBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < 64;
          if (focusedNode) setBridgePath(null);
        }}
      >
        {visibleLanes.length === 0 || flow.nodes.length === 0 ? (
          <div className="sa-traj-empty">
            <p>发送消息或点名多个队员协作后，流光会从左侧对话飞入各自的工作树泳道。</p>
          </div>
        ) : (
          <div
            className="sa-traj-grid"
            style={{ "--traj-cols": Math.max(1, visibleLanes.length) } as CSSProperties}
          >
            <svg className="sa-traj-arcs" aria-hidden="true">
              {arcs.map((arc) => (
                <path key={arc.id} d={arc.d} data-live={arc.live || undefined} />
              ))}
            </svg>

            {visibleLanes.map((lane) => {
              const bot = lane.botId ? bots.find((b) => b.id === lane.botId) : null;
              const now = lane.botId ? presenceByBot.get(lane.botId) : undefined;
              const desk = lane.botId ? deskByBot.get(lane.botId) : undefined;
              const color = colorOf.get(lane.key) ?? USER_COLOR;
              const laneNodes = nodesByLane.get(lane.key) ?? [];
              const isBusy =
                now && now.state !== "idle" && now.state !== "off";

              return (
                <div
                  key={lane.key}
                  className="sa-traj-col"
                  style={{ "--lane-color": color } as CSSProperties}
                >
                  <div className="sa-traj-lane-head" data-busy={isBusy || undefined}>
                    <div className="sa-traj-lane-row">
                      <i className="sa-traj-dot" aria-hidden="true" />
                      <strong className="sa-traj-name" title={lane.name}>
                        {lane.name}
                      </strong>
                      {bot && (
                        <span className="sa-traj-harness">
                          {SHORT_HARNESS[bot.harness]}
                        </span>
                      )}
                    </div>
                    <div className="sa-traj-lane-sub" title={desk?.path ?? undefined}>
                      {desk && (desk.branch || desk.path)
                        ? `${desk.branch ?? "worktree"}${desk.changes.length > 0 ? ` · +${desk.changes.length}` : ""}`
                        : lane.role === "main"
                          ? "主泳道 · main"
                          : isBusy
                            ? now.text
                            : laneNodes.length > 0
                              ? `${laneNodes.length} 步`
                              : "待命"}
                    </div>
                    <div className="sa-traj-bar" data-active={isBusy || laneNodes.length > 0 || undefined} />
                  </div>

                  <div className="sa-traj-nodes">
                    {laneNodes.length > 0 && <div className="sa-traj-spine" aria-hidden="true" />}
                    {laneNodes.map((node) => (
                      <button
                        key={node.id}
                        type="button"
                        data-traj-node={node.id}
                        data-traj-bot={lane.botId ?? undefined}
                        data-traj-msg={node.messageId}
                        data-kind={node.kind}
                        data-tone={node.tone}
                        data-focused={focusedNode === node.id || undefined}
                        className="sa-traj-node"
                        title={`${node.label}（点击定位到左侧消息）`}
                        onMouseEnter={() => updateFocusBridge(node)}
                        onMouseLeave={() => updateFocusBridge(null)}
                        onClick={() => {
                          onOpen(node.messageId);
                          void emit({
                            id: `back-${node.id}-${Date.now()}`,
                            source: () => findTreeGlyph(node.id),
                            target: () => findVisibleChatAnchor(node.messageId, lane.botId),
                            color,
                            duration: 750,
                            sourceKind: "message",
                          });
                        }}
                      >
                        <span className="sa-traj-glyph" aria-hidden="true">
                          {node.tone === "ok" && (node.kind === "done" || node.kind === "inbox") ? "✓" : null}
                          {node.tone === "error" ? "!" : null}
                        </span>
                        <span className="sa-traj-node-text">
                          <span className="sa-traj-node-title">{node.label}</span>
                          <span className="sa-traj-node-meta">
                            {KIND_META[node.kind]}
                          </span>
                        </span>
                      </button>
                    ))}
                  </div>
                </div>
              );
            })}
          </div>
        )}
      </div>

      <footer className="sa-traj-legend" aria-hidden="true">
        <span>● 步骤</span>
        <span>◆ 派发</span>
        <span>○ 分叉</span>
        <span>✓ 完成</span>
      </footer>
    </aside>
  );
}

const KIND_META: Record<FlowNode["kind"], string> = {
  ask: "提问",
  start: "接手分支",
  step: "执行步骤",
  handoff: "跨工位派发",
  task: "创建任务",
  done: "阶段完成",
  inbox: "结果汇流",
};
