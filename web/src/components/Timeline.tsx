import { useEffect, useLayoutEffect, useRef, useState } from "react";
import type { RoomChat } from "../api/chat";
import type { ApprovalDecision, ApprovalId, Bot, Message, ToolStep } from "../api/types";

type Decide = (id: ApprovalId, decision: ApprovalDecision) => Promise<void>;

interface Props {
  chat: RoomChat | undefined;
  bots: readonly Bot[];
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

export function Timeline({ chat, bots, loadError, onRetry, onLoadOlder, onDecide, onLightDemo }: Props) {
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
    if (last?.sender.kind === "user") {
      stick.current = true;
      scroller.current?.scrollTo({ top: scroller.current.scrollHeight });
    }
  }, [last?.id, last?.sender.kind]);

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
        {messages.map((message) => (
          <MessageItem key={message.id} message={message} bots={bots} onDecide={onDecide} />
        ))}
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

function MessageItem({
  message,
  bots,
  onDecide,
}: {
  message: Message;
  bots: readonly Bot[];
  onDecide: Decide;
}) {
  const { sender, state } = message;
  const name =
    sender.kind === "user" ? "你" : (bots.find((bot) => bot.id === sender.bot_id)?.name ?? "Bot");
  const waiting = state === "streaming" && message.content === "" && message.steps.length === 0;
  return (
    <li className="sa-msg" data-sender={sender.kind} data-state={state}>
      <div className="sa-msg-head">
        <span className="sa-msg-name">{name}</span>
        <time className="sa-msg-time" dateTime={new Date(message.created_at).toISOString()}>
          {formatTime(message.created_at)}
        </time>
      </div>
      {message.steps.length > 0 && (
        <ul className="sa-steps" aria-label="工具调用">
          {message.steps.map((step, index) => (
            <StepItem key={`${step.id}-${index}`} step={step} onDecide={onDecide} />
          ))}
        </ul>
      )}
      {message.content !== "" && (
        <div className="sa-msg-body">
          {message.content}
          {state === "streaming" && <span className="sa-caret" aria-hidden="true" />}
        </div>
      )}
      {waiting && <p className="sa-msg-note">正在回复…</p>}
      {state === "cancelled" && <p className="sa-msg-note">已停止</p>}
      {state === "interrupted" && <p className="sa-msg-note">回复被中断（服务重启或关闭），可以再发一条让它继续。</p>}
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
