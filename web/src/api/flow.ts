import { handoffsOf } from "./handoffs";
import { taskDecision } from "./taskNotes";
import type {
  Bot,
  BotId,
  Discussion,
  Message,
  MessageId,
  StepState,
  ToolStep,
} from "./types";

/** 协作图上的一条竖线：你、主 Agent（房间的入口 Bot），或者下面干活的一个 Bot。 */
export interface FlowLane {
  key: string;
  botId: BotId | null;
  name: string;
  role: "user" | "main" | "bot";
}

/**
 * 竖线上的一个节点。
 * - ask：你说的一句话；
 * - start：Bot 接下一件事；
 * - step：做完（或正在做）的一步；
 * - handoff：交给别的 Bot；
 * - task：开了一个任务；
 * - done：这一轮说完了，带着结果；
 * - inbox：结果交回来，落到这条线上。
 */
export type FlowNodeKind =
  "ask" | "start" | "step" | "handoff" | "task" | "done" | "inbox";
export type FlowTone = "ok" | "live" | "waiting" | "error" | "muted";

export interface FlowNode {
  id: string;
  lane: string;
  row: number;
  kind: FlowNodeKind;
  tone: FlowTone;
  label: string;
  /** 点一下回到对话里的哪条消息。 */
  messageId: MessageId;
  /** 同一段连续工作的节点共用一个 run，竖线上这一段是亮的。 */
  run: string;
}

/**
 * 两个节点之间的一次传递。
 * - ask：你交给某个 Bot；
 * - dispatch：主 Agent 派活给下面的 Bot（包括开任务）；
 * - peer：下面的 Bot 之间互相交接；
 * - return：结果交回给派活的那一方。
 */
export type FlowEdgeKind = "ask" | "dispatch" | "peer" | "return";

export interface FlowEdge {
  id: string;
  from: string;
  to: string;
  kind: FlowEdgeKind;
  /** 接手的一方还在做：线上有光在流。 */
  live: boolean;
  /** 这次传递是什么时候发生的。 */
  at: number;
}

export interface Flow {
  lanes: FlowLane[];
  nodes: FlowNode[];
  edges: FlowEdge[];
  rows: number;
  /** 每一轮（你说一句话）从第几行开始。 */
  rounds: number[];
}

export const USER_LANE = "user";
const laneOf = (botId: BotId) => `bot:${botId}`;

const STEP_TONE: Record<StepState, FlowTone> = {
  waiting: "waiting",
  running: "live",
  ok: "ok",
  error: "error",
  cancelled: "muted",
  denied: "muted",
};

function firstLine(text: string, max = 40): string {
  const line =
    text
      .split("\n")
      .map((part) => part.trim())
      .find(Boolean) ?? "";
  const clean = line.replace(/^（Mock 模型）/, "");
  return clean.length > max ? `${clean.slice(0, max)}…` : clean;
}

function endTone(message: Message): FlowTone {
  if (message.state === "streaming") return "live";
  if (message.state === "failed" || message.error) return "error";
  if (message.state === "cancelled" || message.state === "interrupted")
    return "muted";
  return "ok";
}

/**
 * 把房间的消息排成协作图：每个 Bot 一条竖线，往下一个节点就是它做完了一步。
 *
 * 行号这样定：同一条竖线上一个接一个往下排；有线连进来的节点至少比来源低一行；
 * 你每说一句话另起一轮，排在前面所有节点的下面。这样线总是往下走，画面不乱。
 */
export function flowOf(
  messages: readonly Message[],
  bots: readonly Bot[],
  hostBotId: BotId | null,
  memberIds: readonly BotId[],
  discussion: Discussion = "normal",
): Flow {
  const name = (id: BotId) =>
    bots.find((bot) => bot.id === id)?.name ?? "已删除的 Bot";
  const lanes: FlowLane[] = [
    { key: USER_LANE, botId: null, name: "你", role: "user" },
  ];
  const addLane = (botId: BotId) => {
    if (lanes.some((lane) => lane.botId === botId)) return;
    lanes.push({
      key: laneOf(botId),
      botId,
      name: name(botId),
      role: botId === hostBotId ? "main" : "bot",
    });
  };
  if (hostBotId) addLane(hostBotId);
  memberIds.forEach(addLane);

  const nodes: FlowNode[] = [];
  const edges: FlowEdge[] = [];
  const rounds: number[] = [];
  const byId = new Map<string, FlowNode>();
  const next = new Map<string, number>();
  let bottom = 0;

  const place = (
    node: Omit<FlowNode, "row">,
    after: readonly string[] = [],
    fresh = false,
  ): FlowNode => {
    let row = next.get(node.lane) ?? 0;
    for (const id of after) {
      const source = byId.get(id);
      if (source) row = Math.max(row, source.row + 1);
    }
    if (fresh) row = Math.max(row, bottom);
    const placed = { ...node, row };
    next.set(node.lane, row + 1);
    bottom = Math.max(bottom, row + 1);
    nodes.push(placed);
    byId.set(placed.id, placed);
    return placed;
  };
  const link = (
    from: string,
    to: string,
    kind: FlowEdgeKind,
    live: boolean,
    at: number,
  ) => {
    if (byId.has(from) && byId.has(to))
      edges.push({ id: `${from}>${to}`, from, to, kind, live, at });
  };
  // 交回结果另画（落到「收到…」节点）；连进一段工作的线只有找人和派活。
  const kindBetween = (fromLane: string): FlowEdgeKind => {
    if (fromLane === USER_LANE) return "ask";
    const fromMain =
      lanes.find((lane) => lane.key === fromLane)?.role === "main";
    return fromMain ? "dispatch" : "peer";
  };

  const shown = messages.filter(
    (message) => message.kind !== "routine" && message.kind !== "event",
  );
  const handoffs = handoffsOf(shown, discussion);
  /** Bot 用 `ask_bot` 发出去的那句话，和它回复里那一步对上：节点画在那一步的位置。 */
  const handoffAt = new Map<string, MessageId>();
  /** Bot 用 `create_task` 开的任务卡，同样画在那一步的位置。 */
  const taskAt = new Map<string, MessageId>();
  const claimed = new Set<MessageId>();
  for (const message of shown) {
    if (
      message.sender.kind !== "bot" ||
      message.kind !== "chat" ||
      message.mentions.length > 0
    )
      continue;
    const botId = message.sender.bot_id;
    for (const step of message.steps) {
      const want =
        step.tool === "ask_bot"
          ? "handoff"
          : step.tool === "create_task"
            ? "task"
            : null;
      if (!want || step.state !== "ok") continue;
      const match = shown.find(
        (later) =>
          later.seq > message.seq &&
          !claimed.has(later.id) &&
          later.sender.kind === "bot" &&
          later.sender.bot_id === botId &&
          (want === "handoff"
            ? later.kind === "chat" && later.mentions.length > 0
            : later.kind === "task"),
      );
      if (!match) continue;
      claimed.add(match.id);
      (want === "handoff" ? handoffAt : taskAt).set(
        `${message.id}:${step.id}`,
        match.id,
      );
    }
  }

  /** 任务卡节点 → 那个任务后面每一次运行、交付接在它下面。 */
  const taskNode = new Map<string, string>();
  /** 一件事从哪条竖线派来，做完把结果交回那里。 */
  const origin = new Map<string, string>();
  let lastAsk: string | null = null;
  let trigger = "接到你的话";

  const handoffNode = (message: Message, run: string) => {
    if (message.sender.kind !== "bot") return;
    const to = message.mentions.map(name).join("、");
    place({
      id: message.id,
      lane: laneOf(message.sender.bot_id),
      kind: "handoff",
      tone: "ok",
      label: `交给 ${to}`,
      messageId: message.id,
      run,
    });
  };
  const taskCard = (message: Message, lane: string, run: string) => {
    place({
      id: message.id,
      lane,
      kind: "task",
      tone: "ok",
      label: firstLine(message.content.replace(/^开了任务/, "开任务"), 28),
      messageId: message.id,
      run,
    });
    if (message.task_id) {
      taskNode.set(message.task_id, message.id);
      origin.set(message.task_id, lane);
    }
  };

  for (const message of messages) {
    const { sender } = message;
    // 定时任务、外部事件叫起来的回复，不是在回你上一句话。
    if (message.kind === "routine" || message.kind === "event") {
      lastAsk = null;
      trigger = message.kind === "routine" ? "接到定时任务" : "接到外部事件";
      continue;
    }
    if (sender.kind === "user") {
      if (message.kind === "task") {
        // 验收、打回、取消是人对任务的决定，不是新开的任务卡。
        if (taskDecision(message.content) === null)
          taskCard(message, USER_LANE, message.id);
        continue;
      }
      if (message.kind !== "chat") continue;
      rounds.push(bottom);
      place(
        {
          id: message.id,
          lane: USER_LANE,
          kind: "ask",
          tone: "ok",
          label:
            firstLine(
              message.content.replace(/^(@\S+\s*)+/, "").replace(/`/g, ""),
              26,
            ) || "（空）",
          messageId: message.id,
          run: message.id,
        },
        [],
        true,
      );
      lastAsk = message.id;
      trigger = "接到你的话";
      continue;
    }

    const botId = sender.bot_id;
    addLane(botId);
    const lane = laneOf(botId);
    if (claimed.has(message.id)) continue;
    if (message.kind === "chat" && message.mentions.length > 0) {
      handoffNode(message, message.id);
      continue;
    }
    if (message.kind === "task") {
      taskCard(message, lane, message.id);
      continue;
    }

    // 一段工作：接下 → 每一步 → 说完（或者正在做）。
    const incoming = handoffs.filter((handoff) => handoff.to === message.id);
    const sources: string[] = [];
    let label: string;
    if (message.kind === "run" || message.kind === "delivery") {
      const card = message.task_id ? taskNode.get(message.task_id) : undefined;
      if (card) sources.push(card);
      label = message.kind === "run" ? "开工做任务" : "交付";
    } else if (incoming.length > 0) {
      sources.push(...incoming.map((handoff) => handoff.from));
      label = `接手 ${incoming.map((handoff) => name(handoff.fromBot)).join("、")} 的活`;
    } else {
      if (lastAsk) sources.push(lastAsk);
      label = trigger;
    }

    if (message.kind === "delivery") {
      const node = place(
        {
          id: message.id,
          lane,
          kind: "done",
          tone: "ok",
          label: `交付：${firstLine(message.content, 24)}`,
          messageId: message.id,
          run: message.id,
        },
        sources,
      );
      const back = message.task_id ? origin.get(message.task_id) : undefined;
      if (back && back !== lane) {
        const inbox = place(
          {
            id: `${message.id}:inbox`,
            lane: back,
            kind: "inbox",
            tone: "ok",
            label: `收到 ${name(botId)} 的交付`,
            messageId: message.id,
            run: `${message.id}:inbox`,
          },
          [node.id],
        );
        link(node.id, inbox.id, "return", false, message.updated_at);
      }
      continue;
    }

    const run = message.id;
    const start = place(
      {
        id: `${message.id}:start`,
        lane,
        kind: "start",
        tone: "ok",
        label,
        messageId: message.id,
        run,
      },
      sources,
    );
    for (const source of sources) {
      const from = byId.get(source);
      if (!from) continue;
      const kind = message.kind === "run" ? "dispatch" : kindBetween(from.lane);
      link(
        source,
        start.id,
        kind,
        message.state === "streaming",
        message.created_at,
      );
    }

    message.steps.forEach((step: ToolStep) => {
      const handed = handoffAt.get(`${message.id}:${step.id}`);
      if (handed) {
        const handoff = shown.find((m) => m.id === handed);
        if (handoff) {
          handoffNode(handoff, run);
          return;
        }
      }
      const opened = taskAt.get(`${message.id}:${step.id}`);
      if (opened) {
        const card = shown.find((m) => m.id === opened);
        if (card) {
          taskCard(card, lane, run);
          return;
        }
      }
      place({
        id: `${message.id}:${step.id}`,
        lane,
        kind: "step",
        tone: STEP_TONE[step.state],
        label: step.title || step.tool,
        messageId: message.id,
        run,
      });
    });

    const tone = endTone(message);
    const end = place({
      id: `${message.id}:end`,
      lane,
      kind: "done",
      tone,
      label:
        tone === "live"
          ? message.steps.some((step) => step.state === "waiting")
            ? "等你批准"
            : "正在做…"
          : tone === "muted"
            ? "停下了"
            : tone === "error"
              ? "没做成"
              : firstLine(message.content, 24) || "说完了",
      messageId: message.id,
      run,
    });

    if (message.kind === "run" || tone !== "ok") continue;
    // 结果交回去：别的 Bot 派来的交回给它；你直接找的交回给你。
    const back = new Set(incoming.map((handoff) => laneOf(handoff.fromBot)));
    if (
      back.size === 0 &&
      sources.some((source) => byId.get(source)?.lane === USER_LANE)
    )
      back.add(USER_LANE);
    for (const target of back) {
      if (target === lane) continue;
      const inbox = place(
        {
          id: `${message.id}:back:${target}`,
          lane: target,
          kind: "inbox",
          tone: "ok",
          label:
            target === USER_LANE
              ? `${name(botId)} 答复了你`
              : `收到 ${name(botId)} 的结果`,
          messageId: message.id,
          run: `${message.id}:back:${target}`,
        },
        [end.id],
      );
      link(end.id, inbox.id, "return", false, message.updated_at);
    }
  }

  return { lanes, nodes, edges, rows: bottom, rounds };
}
