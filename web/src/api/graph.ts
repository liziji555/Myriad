import { STATE_LABEL } from "./tasks";
import type { Run, RunId, RunState, Task, TaskId } from "./types";

/** 节点的样子：做成了、没做成、正在做、等人、还没开始、不了了之。 */
export type NodeTone = "ok" | "danger" | "live" | "attention" | "idle" | "muted";

export interface GraphNode {
  /** `run:<运行 id>` 或 `state:<任务 id>:<状态>`。任务状态一变，就是一个新节点。 */
  id: string;
  taskId: TaskId;
  label: string;
  tone: NodeTone;
  /** 这个节点是什么时候出现的。用来区分「刚发生的」和「翻出来的历史」。 */
  at: number;
  /** 运行节点：这次运行的过程记在哪条消息里，光线从那里出发。 */
  runId?: RunId;
  /** 鼠标停上去时的说明。 */
  detail: string;
}

export interface GraphLane {
  task: Task;
  nodes: GraphNode[];
}

const RUN_TONE: Record<RunState, NodeTone> = {
  running: "live",
  succeeded: "ok",
  failed: "danger",
  cancelled: "muted",
  interrupted: "muted",
};
const RUN_WORD: Record<RunState, string> = {
  running: "正在做",
  succeeded: "做完了",
  failed: "没做成",
  cancelled: "被停下了",
  interrupted: "被中断了",
};
const STATE_TONE: Record<Task["state"], NodeTone> = {
  draft: "idle",
  open: "idle",
  working: "live",
  waiting: "attention",
  review: "attention",
  blocked: "danger",
  done: "ok",
  dropped: "muted",
  cancelled: "muted",
};

/**
 * 任务图：一个任务一行，行里依次是它的每次运行，最后是它现在的状态。
 * `focus` 给了就只画那一个任务。新的任务在上面。
 */
export function buildGraph(
  tasks: readonly Task[],
  runs: Readonly<Record<RunId, Run>>,
  focus: TaskId | null = null,
): GraphLane[] {
  const all = Object.values(runs);
  return tasks
    .filter((task) => focus === null || task.id === focus)
    .map((task) => {
      const mine = all.filter((run) => run.task_id === task.id).sort((a, b) => a.attempt - b.attempt);
      const nodes: GraphNode[] = mine.map((run) => ({
        id: `run:${run.id}`,
        taskId: task.id,
        label: `第 ${run.attempt} 次`,
        tone: RUN_TONE[run.state],
        at: run.started_at,
        runId: run.id,
        detail: `第 ${run.attempt} 次运行：${RUN_WORD[run.state]}${run.error ? `（${run.error}）` : ""}`,
      }));
      // 正在做、而且那次运行已经画出来了：不再重复画一个「正在做」。
      const running = mine.some((run) => run.state === "running");
      if (!(task.state === "working" && running)) {
        nodes.push({
          id: `state:${task.id}:${task.state}`,
          taskId: task.id,
          label: STATE_LABEL[task.state],
          tone: STATE_TONE[task.state],
          at: task.updated_at,
          detail: task.note ? `${STATE_LABEL[task.state]}：${task.note}` : STATE_LABEL[task.state],
        });
      }
      return { task, nodes };
    })
    .reverse();
}

export type Density = "normal" | "overview";

/** 两种疏密：正常的看得清每个节点，总览的一屏放得下更多任务。单位是像素。 */
export const SPACING: Record<Density, { column: number; row: number; node: number }> = {
  normal: { column: 112, row: 32, node: 24 },
  overview: { column: 76, row: 18, node: 14 },
};

export interface PlacedNode extends GraphNode {
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface GraphLayout {
  width: number;
  height: number;
  /** 每一行最左边那格：任务的名字。 */
  labels: { task: Task; x: number; y: number; width: number; height: number }[];
  nodes: PlacedNode[];
  /** 相邻两个节点之间的连线，坐标是线的两头。 */
  edges: { id: string; x1: number; x2: number; y: number }[];
}

const GAP = 8;

/** 把任务图摆到格子上：第 0 列是任务名，往右依次是节点。 */
export function layoutGraph(lanes: readonly GraphLane[], density: Density): GraphLayout {
  const { column, row, node } = SPACING[density];
  const pad = (row - node) / 2;
  const layout: GraphLayout = { width: column, height: lanes.length * row, labels: [], nodes: [], edges: [] };
  lanes.forEach((lane, index) => {
    const y = index * row + pad;
    layout.labels.push({ task: lane.task, x: 0, y, width: column - GAP, height: node });
    lane.nodes.forEach((item, at) => {
      const x = (at + 1) * column;
      layout.nodes.push({ ...item, x, y, width: column - GAP, height: node });
      if (at > 0) {
        layout.edges.push({ id: item.id, x1: x - GAP, x2: x, y: index * row + row / 2 });
      }
    });
    layout.width = Math.max(layout.width, (lane.nodes.length + 1) * column);
  });
  return layout;
}

/** 这么久以内出现的节点才算「刚发生的」。 */
export const FRESH_MS = 15_000;

/**
 * 哪些节点值得飞一道光：之前没见过，而且是刚刚发生的。
 * 翻出来的历史（打开页面、展开任务时拉到的旧运行）虽然也是「没见过」，但不是刚发生的，不飞。
 */
export function freshNodes(
  seen: ReadonlySet<string>,
  lanes: readonly GraphLane[],
  now: number,
): GraphNode[] {
  return lanes
    .flatMap((lane) => lane.nodes)
    .filter((node) => !seen.has(node.id) && now - node.at < FRESH_MS);
}
