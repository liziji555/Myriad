import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { RoomChat } from "../api/chat";
import { shownTriggers, triggerTitle } from "../api/routines";
import { isLatestDelivery, type TaskBoard } from "../api/tasks";
import type { TaskActions } from "../api/useWorkspace";
import type { ApprovalDecision, ApprovalId, Bot, Message, Task, TaskId, ToolStep } from "../api/types";
import { Artifacts, StateBadge, TaskActionBar, deliveryHeading } from "./TaskCards";
import { splitMentions } from "./mentions";

type Decide = (id: ApprovalId, decision: ApprovalDecision) => Promise<void>;

interface Props {
  chat: RoomChat | undefined;
  bots: readonly Bot[];
  /** 这个房间的任务。任务卡、交付卡上的状态和操作跟着它变。 */
  tasks: readonly Task[];
  runs: TaskBoard["runs"];
  taskActions: TaskActions;
  /** 房间里启用着的 Bot。 */
  members: readonly Bot[];
  /** 在右栏的任务页里打开一个任务。 */
  onShowTask(taskId: TaskId): void;
  /** 最新一页没拉到时的原因。 */
  loadError: string | null;
  onRetry(): void;
  onLoadOlder(): Promise<void>;
  /** 答复一张审批卡。 */
  onDecide: Decide;
  onLightDemo(source: HTMLElement): void;
}

/** 离底部这么近就算「在看最新的」，新内容来了自动跟着滚。 */
const STICK_PX = 80;

export function Timeline({
  chat,
  bots,
  tasks,
  runs,
  taskActions,
  members,
  onShowTask,
  loadError,
  onRetry,
  onLoadOlder,
  onDecide,
  onLightDemo,
}: Props) {
  const scroller = useRef<HTMLElement>(null);
  const stick = useRef(true);
  /** 往前翻页之前的内容高度，用来在新内容插到上面之后保持视线位置。 */
  const heightBeforeOlder = useRef<number | null>(null);
  const [loadingOlder, setLoadingOlder] = useState(false);
  const [olderError, setOlderError] = useState<string | null>(null);
  const messages = chat?.messages ?? [];
  const last = messages.at(-1);

  useLayoutEffect(() => {
    const el = scroller.current;
    if (!el) return;
    if (heightBeforeOlder.current !== null) {
      el.scrollTop += el.scrollHeight - heightBeforeOlder.current;
      heightBeforeOlder.current = null;
    } else if (stick.current) {
      el.scrollTop = el.scrollHeight;
    }
  }, [messages.length, messages[0]?.id, last?.rev, last?.content.length]);

  // 最后一条消息里有没有在等批准的步骤：卡片出现时要滚到看得见。
  const asking = last?.steps.some((step) => step.state === "waiting") ?? false;
  useEffect(() => {
    if (asking) {
      stick.current = true;
      scroller.current?.scrollTo({ top: scroller.current.scrollHeight });
    }
  }, [asking]);

  // 自己刚发的消息总要看得到。
  useEffect(() => {
    if (last?.sender.kind === "user" && last.kind !== "routine") {
      stick.current = true;
      scroller.current?.scrollTo({ top: scroller.current.scrollHeight });
    }
  }, [last?.id, last?.sender.kind, last?.kind]);

  const triggers = useMemo(() => shownTriggers(messages), [messages]);

  const loadOlder = async () => {
    setLoadingOlder(true);
    setOlderError(null);
    heightBeforeOlder.current = scroller.current?.scrollHeight ?? null;
    try {
      await onLoadOlder();
    } catch (cause) {
      heightBeforeOlder.current = null;
      setOlderError(cause instanceof Error ? cause.message : String(cause));
    }
    setLoadingOlder(false);
  };

  let body;
  if (!chat?.loaded && loadError) {
    body = (
      <div className="sa-empty">
        <h2>消息没加载出来</h2>
        <p>{loadError}</p>
        <button type="button" className="sa-button" onClick={onRetry}>
          重试
        </button>
      </div>
    );
  } else if (!chat?.loaded && messages.length === 0) {
    body = (
      <div className="sa-empty">
        <p>正在加载消息…</p>
      </div>
    );
  } else if (messages.length === 0) {
    body = (
      <div className="sa-empty">
        <h2>这个房间还没有消息</h2>
        <p>在下面输入内容，回车发送。也可以先看看「过程连到结果」的光线效果：</p>
        <button type="button" className="sa-button" onClick={(event) => onLightDemo(event.currentTarget)}>
          演示光线
        </button>
      </div>
    );
  } else {
    body = (
      <ol className="sa-messages">
        {chat?.hasMore && (
          <li className="sa-older">
            <button type="button" className="sa-chip" disabled={loadingOlder} onClick={() => void loadOlder()}>
              {loadingOlder ? "正在加载…" : "加载更早的消息"}
            </button>
            {olderError && <span className="sa-msg-error">{olderError}</span>}
          </li>
        )}
        {messages.map((message) => {
          if (message.kind === "routine") {
            // Bot 没什么可说时什么都不留，这一行也不显示。
            return triggers.has(message.id) ? (
              <li key={message.id} className="sa-msg-trigger">
                定时任务「{triggerTitle(message.content)}」到点了 · {formatTime(message.created_at)}
              </li>
            ) : null;
          }
          const task = tasks.find((t) => t.id === message.task_id);
          if (message.kind === "task" || message.kind === "delivery") {
            return (
              <TaskMessage
                key={message.id}
                message={message}
                task={task}
                latest={message.kind === "delivery" && isLatestDelivery(messages, message)}
                bots={bots}
                members={members}
                actions={taskActions}
                onShowTask={onShowTask}
              />
            );
          }
          const run = message.run_id ? runs[message.run_id] : undefined;
          return (
            <MessageItem
              key={message.id}
              message={message}
              bots={bots}
              onDecide={onDecide}
              work={
                message.kind === "run"
                  ? {
                      title: task?.title,
                      attempt: run?.attempt,
                      onShow: () => message.task_id && onShowTask(message.task_id),
                      onStop: () => (message.task_id ? taskActions.stop(message.task_id) : Promise.resolve()),
                    }
                  : undefined
              }
            />
          );
        })}
      </ol>
    );
  }

  return (
    <section
      ref={scroller}
      className="sa-timeline"
      aria-label="时间线"
      onScroll={(event) => {
        const el = event.currentTarget;
        stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < STICK_PX;
      }}
    >
      {body}
    </section>
  );
}

/** 任务的运行过程那条消息多出来的东西：是哪个任务的、能停。 */
interface Work {
  title: string | undefined;
  attempt: number | undefined;
  onShow(): void;
  onStop(): Promise<void>;
}

/** 时间线上的任务卡（开了一个任务）和交付卡（任务交了结果）。 */
function TaskMessage({
  message,
  task,
  latest,
  bots,
  members,
  actions,
  onShowTask,
}: {
  message: Message;
  task: Task | undefined;
  latest: boolean;
  bots: readonly Bot[];
  members: readonly Bot[];
  actions: TaskActions;
  onShowTask(taskId: TaskId): void;
}) {
  const { sender } = message;
  const name =
    sender.kind === "user" ? "你" : (bots.find((bot) => bot.id === sender.bot_id)?.name ?? "Bot");
  const owner = bots.find((bot) => bot.id === task?.owner_bot_id)?.name;
  const delivery = message.kind === "delivery";
  // 这张卡上要不要放操作：最新的交付卡，而且任务正等着人（验收、回答、卡住）。
  const actionable = delivery && latest && task && ["review", "waiting", "blocked"].includes(task.state);
  return (
    <li className="sa-msg" data-sender={sender.kind} data-kind={message.kind}>
      <div className="sa-msg-head">
        <span className="sa-msg-name">{name}</span>
        <time className="sa-msg-time" dateTime={new Date(message.created_at).toISOString()}>
          {formatTime(message.created_at)}
        </time>
      </div>
      <div className="sa-card" data-kind={message.kind} data-latest={latest}>
        <div className="sa-card-head">
          <span className="sa-card-kicker">{delivery ? deliveryHeading(task, latest) : "开了一个任务"}</span>
          {task && (!delivery || latest) && <StateBadge state={task.state} />}
        </div>
        <p className="sa-card-title">{task?.title ?? "任务"}</p>
        {delivery ? (
          <div className="sa-msg-body">{message.content}</div>
        ) : (
          <p className="sa-card-scope">{task?.scope ?? message.content}</p>
        )}
        {delivery && latest && task && <Artifacts artifacts={task.artifacts} />}
        {actionable && <TaskActionBar key={task.state} task={task} actions={actions} members={members} />}
        <div className="sa-card-foot">
          {!delivery && <span className="sa-row-meta">负责：{owner ?? "还没有负责人"}</span>}
          {message.task_id && (
            <button type="button" className="sa-link" onClick={() => message.task_id && onShowTask(message.task_id)}>
              查看任务
            </button>
          )}
        </div>
      </div>
    </li>
  );
}

function MessageItem({
  message,
  bots,
  onDecide,
  work,
}: {
  message: Message;
  bots: readonly Bot[];
  onDecide: Decide;
  work?: Work;
}) {
  const [stopping, setStopping] = useState(false);
  const { sender, state } = message;
  const name =
    sender.kind === "user" ? "你" : (bots.find((bot) => bot.id === sender.bot_id)?.name ?? "Bot");
  const waiting = state === "streaming" && message.content === "" && message.steps.length === 0;
  const mentioned = message.mentions.flatMap((id) => bots.find((bot) => bot.id === id)?.name ?? []);
  return (
    <li
      className="sa-msg"
      data-message-id={message.id}
      data-sender={sender.kind}
      data-state={state}
      data-kind={message.kind}
    >
      <div className="sa-msg-head">
        <span className="sa-msg-name">{name}</span>
        <time className="sa-msg-time" dateTime={new Date(message.created_at).toISOString()}>
          {formatTime(message.created_at)}
        </time>
      </div>
      {work && (
        <div className="sa-work-head">
          <button type="button" className="sa-link" onClick={work.onShow}>
            {work.title ? `任务「${work.title}」` : "任务"}
          </button>
          <span className="sa-row-meta">
            {work.attempt ? `第 ${work.attempt} 次运行` : "运行过程"}
          </span>
          {state === "streaming" && (
            <button
              type="button"
              className="sa-chip"
              disabled={stopping}
              onClick={() => {
                setStopping(true);
                void work.onStop().finally(() => setStopping(false));
              }}
            >
              {stopping ? "正在停…" : "停止"}
            </button>
          )}
        </div>
      )}
      {message.steps.length > 0 && <Steps steps={message.steps} onDecide={onDecide} />}
      {message.content !== "" && (
        <div className="sa-msg-body">
          {splitMentions(message.content, mentioned).map((piece, index) =>
            piece.mention ? (
              <span key={index} className="sa-mention">
                {piece.text}
              </span>
            ) : (
              piece.text
            ),
          )}
          {state === "streaming" && <span className="sa-caret" aria-hidden="true" />}
        </div>
      )}
      {waiting && <p className="sa-msg-note">{work ? "正在开工…" : "正在回复…"}</p>}
      {state === "cancelled" && <p className="sa-msg-note">已停止</p>}
      {state === "interrupted" && (
        <p className="sa-msg-note">
          {work
            ? "这次运行被中断（服务重启或关闭），服务再启动时会接着做。"
            : "回复被中断（服务重启或关闭），可以再发一条让它继续。"}
        </p>
      )}
      {state === "failed" && (
        <p className="sa-msg-error" role="alert">
          出错了：{message.error ?? "原因不明"}
        </p>
      )}
    </li>
  );
}

const STEP_LABEL: Record<ToolStep["state"], string> = {
  waiting: "等你批准",
  running: "进行中",
  ok: "完成",
  error: "失败",
  cancelled: "已停止",
  denied: "没有批准",
};

/** 需要人看一眼的步骤：还在进行、等批准，或者没做成。 */
const NEEDS_ATTENTION: readonly ToolStep["state"][] = ["waiting", "running", "error", "denied"];

/**
 * 一条消息的工具步骤。都顺利做完时折成一行，点开才看细节；
 * 有步骤在进行、等批准或者失败时自动展开。人手动点过之后以人的选择为准。
 */
function Steps({ steps, onDecide }: { steps: readonly ToolStep[]; onDecide: Decide }) {
  const [chosen, setChosen] = useState<boolean | null>(null);
  const attention = steps.some((step) => NEEDS_ATTENTION.includes(step.state));
  // 等批准的卡片必须看得到，不能被折起来。
  const mustShow = steps.some((step) => step.state === "waiting");
  const open = mustShow || (chosen ?? attention);
  const failed = steps.filter((step) => step.state === "error" || step.state === "denied").length;
  const summary = failed > 0 ? `${steps.length} 步操作，${failed} 步没做成` : `${steps.length} 步操作`;
  return (
    <div className="sa-steps-box" data-open={open}>
      <button
        type="button"
        className="sa-steps-toggle"
        aria-expanded={open}
        disabled={mustShow}
        onClick={() => setChosen(!open)}
      >
        <span className="sa-steps-arrow" aria-hidden="true" />
        {summary}
      </button>
      {open && (
        <ul className="sa-steps" aria-label="工具调用">
          {steps.map((step, index) => (
            <StepItem key={`${step.id}-${index}`} step={step} onDecide={onDecide} />
          ))}
        </ul>
      )}
    </div>
  );
}

function StepItem({ step, onDecide }: { step: ToolStep; onDecide: Decide }) {
  return (
    <li className="sa-step" data-state={step.state}>
      <div className="sa-step-line">
        <span className="sa-step-dot" aria-hidden="true" />
        <span className="sa-step-title" title={step.title}>
          {step.title}
        </span>
        <span className="sa-step-state">{STEP_LABEL[step.state]}</span>
      </div>
      {step.state === "waiting" && step.approval && (
        <ApprovalCard
          id={step.approval.id}
          reason={step.approval.reason}
          expiresAt={step.approval.expires_at}
          // 只有命令和写文件能记住「总是允许」。
          canRemember={["shell", "write", "edit"].includes(step.tool)}
          onDecide={onDecide}
        />
      )}
    </li>
  );
}

const DECISIONS: { decision: ApprovalDecision; label: string; hint: string }[] = [
  { decision: "once", label: "允许一次", hint: "只允许这一次" },
  { decision: "always", label: "总是允许", hint: "这个房间里以后同样的操作不再问" },
  { decision: "deny", label: "拒绝", hint: "不执行，Bot 会被告知" },
];

function ApprovalCard({
  id,
  reason,
  expiresAt,
  canRemember,
  onDecide,
}: {
  id: ApprovalId;
  reason: string;
  expiresAt: number;
  canRemember: boolean;
  onDecide: Decide;
}) {
  const [sending, setSending] = useState<ApprovalDecision | null>(null);
  const [error, setError] = useState<string | null>(null);

  const decide = async (decision: ApprovalDecision) => {
    setSending(decision);
    setError(null);
    try {
      await onDecide(id, decision);
      // 成功后保持按钮禁用：卡片会随事件流送来的新状态消失。
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
      setSending(null);
    }
  };

  return (
    <div className="sa-approval" role="group" aria-label="需要你批准">
      <p className="sa-approval-reason">{reason}</p>
      <div className="sa-approval-actions">
        {DECISIONS.filter(({ decision }) => canRemember || decision !== "always").map(
          ({ decision, label, hint }) => (
            <button
              key={decision}
              type="button"
              className="sa-button"
              data-decision={decision}
              title={hint}
              disabled={sending !== null}
              onClick={() => void decide(decision)}
            >
              {sending === decision ? "正在提交…" : label}
            </button>
          ),
        )}
        <span className="sa-approval-expiry">{formatTime(expiresAt)} 前不处理按拒绝算</span>
      </div>
      {error && (
        <p className="sa-msg-error" role="alert">
          {error}
        </p>
      )}
    </div>
  );
}

function formatTime(ms: number): string {
  const date = new Date(ms);
  const sameDay = date.toDateString() === new Date().toDateString();
  return date.toLocaleString("zh-CN", {
    ...(sameDay ? {} : { month: "numeric", day: "numeric" }),
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });
}
