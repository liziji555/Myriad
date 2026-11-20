import { useEffect, useMemo, useRef, useState } from "react";
import { buildGraph, freshNodes, layoutGraph, type Density, type GraphNode } from "../api/graph";
import { STATE_LABEL, type TaskBoard } from "../api/tasks";
import type { Task, TaskId } from "../api/types";
import { useLight } from "../light/LightProvider";

/** 打开这一页时最多给多少个任务补拉执行历史。 */
const MAX_BACKFILL = 20;

interface Props {
  /** 这个房间的任务，按创建的先后。 */
  tasks: readonly Task[];
  runs: TaskBoard["runs"];
  /** 任务页里展开着的任务：可以只看它。 */
  focus: TaskId | null;
  /** 在任务页里打开一个任务。 */
  onShowTask(taskId: TaskId): void;
  loadRuns(taskId: TaskId): Promise<void>;
}

/** 任务图：一个任务一行，从左到右是它的每次运行和现在的状态。刚发生的事有一道光从过程飞到节点上。 */
export function TaskGraph({ tasks, runs, focus, onShowTask, loadRuns }: Props) {
  const emit = useLight();
  const [density, setDensity] = useState<Density>("normal");
  const [onlyFocus, setOnlyFocus] = useState(false);
  const focused = onlyFocus && focus !== null && tasks.some((task) => task.id === focus);
  const lanes = useMemo(() => buildGraph(tasks, runs, focused ? focus : null), [tasks, runs, focused, focus]);
  const layout = useMemo(() => layoutGraph(lanes, density), [lanes, density]);
  const root = useRef<HTMLDivElement>(null);

  // 开过工的任务补拉一次执行历史：事件流只送来打开页面之后的运行。
  const asked = useRef(new Set<TaskId>());
  useEffect(() => {
    const started = tasks.filter((task) => task.state !== "draft" && task.state !== "open");
    for (const task of started.slice(-MAX_BACKFILL)) {
      if (asked.current.has(task.id)) continue;
      asked.current.add(task.id);
      // 拉不到就只画现在的状态，不打扰人。
      loadRuns(task.id).catch(() => {});
    }
  }, [tasks, loadRuns]);

  // 光线只给刚发生的事：第一次画出来的节点都当作见过，之后新出现而且是刚发生的才飞。
  const seen = useRef<Set<string> | null>(null);
  useEffect(() => {
    const all = lanes.flatMap((lane) => lane.nodes);
    const fresh = seen.current ? freshNodes(seen.current, lanes, Date.now()) : [];
    seen.current = new Set([...(seen.current ?? []), ...all.map((node) => node.id)]);
    const color = getComputedStyle(document.documentElement).getPropertyValue("--sa-accent").trim();
    for (const node of fresh) {
      const target = () => root.current?.querySelector(`[data-node-id="${CSS.escape(node.id)}"]`) ?? null;
      const source = () => sourceOf(node, all, runs, root.current);
      void emit({ id: node.id, source, target, color, sourceKind: "message" });
    }
  }, [lanes, runs, emit]);

  if (tasks.length === 0) {
    return <p className="sa-panel-empty">还没有任务。Bot 开了任务之后，这里画出每个任务走到哪一步。</p>;
  }
  const overview = density === "overview";
  return (
    <div className="sa-graph" data-density={density} ref={root}>
      <div className="sa-graph-controls">
        <button
          type="button"
          className="sa-chip"
          aria-pressed={focused}
          disabled={focus === null}
          title={focus === null ? "先在任务页里展开一个任务" : undefined}
          onClick={() => setOnlyFocus((only) => !only)}
        >
          只看当前任务
        </button>
        <button
          type="button"
          className="sa-chip"
          aria-pressed={overview}
          onClick={() => setDensity(overview ? "normal" : "overview")}
        >
          总览
        </button>
      </div>
      <div className="sa-graph-scroll">
        <div className="sa-graph-canvas" style={{ width: layout.width, height: layout.height }}>
          <svg className="sa-graph-edges" width={layout.width} height={layout.height} aria-hidden="true">
            {layout.edges.map((edge) => (
              <line key={edge.id} x1={edge.x1} x2={edge.x2} y1={edge.y} y2={edge.y} />
            ))}
          </svg>
          {layout.labels.map(({ task, x, y, width, height }) => (
            <button
              key={task.id}
              type="button"
              className="sa-graph-label"
              data-task-label={task.id}
              style={{ left: x, top: y, width, height }}
              title={`${task.title}（${STATE_LABEL[task.state]}）`}
              aria-current={task.id === focus ? "true" : undefined}
              onClick={() => onShowTask(task.id)}
            >
              {task.title}
            </button>
          ))}
          {layout.nodes.map((node) => (
            <button
              key={node.id}
              type="button"
              className="sa-graph-node"
              data-node-id={node.id}
              data-tone={node.tone}
              style={{ left: node.x, top: node.y, width: node.width, height: node.height }}
              title={node.detail}
              onClick={() => onShowTask(node.taskId)}
            >
              {node.tone === "live" && (
                <svg className="light-running-border" aria-hidden="true">
                  <rect x="1" y="1" width="calc(100% - 2px)" height="calc(100% - 2px)" rx="8" pathLength="100" />
                </svg>
              )}
              {node.label}
            </button>
          ))}
        </div>
      </div>
    </div>
  );
}

/**
 * 光线从哪里出发：这次运行的过程在时间线上的那条消息；状态节点从它前面那个节点出发。
 * 找不到（消息没加载、被滚出了屏幕）就从同一行的任务名出发。
 */
function sourceOf(
  node: GraphNode,
  all: readonly GraphNode[],
  runs: TaskBoard["runs"],
  root: HTMLElement | null,
): Element | null {
  const mine = all.filter((other) => other.taskId === node.taskId);
  const before = mine[mine.findIndex((other) => other.id === node.id) - 1];
  const runId = node.runId ?? before?.runId;
  const run = runId ? runs[runId] : undefined;
  const message = run && document.querySelector(`[data-message-id="${CSS.escape(run.message_id)}"]`);
  if (message) {
    const box = message.getBoundingClientRect();
    if (box.bottom > 0 && box.top < window.innerHeight) return message;
  }
  if (before) return root?.querySelector(`[data-node-id="${CSS.escape(before.id)}"]`) ?? null;
  return root?.querySelector(`[data-task-label="${CSS.escape(node.taskId)}"]`) ?? null;
}
