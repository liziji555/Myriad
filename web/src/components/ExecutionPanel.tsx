import { useEffect, useState } from "react";
import { ago, EXEC_LABEL, elapsed, type ExecCard } from "../api/execution";
import { STEP_LABEL, stepIcon } from "../api/steps";
import type { ModelTier, TaskId } from "../api/types";
import { BotAvatar } from "./BotAvatar";
import { Icon } from "./Icon";

const TIER: Record<ModelTier, string> = { cheap: "便宜档", mid: "中档", strong: "强档" };
/** 卡片上只列最近这几步，全部的在主线上。 */
const RECENT = 4;

interface Props {
  cards: readonly ExecCard[];
  onJump(card: ExecCard): void;
  onShowTask(taskId: TaskId): void;
}

/** 每秒走一次的钟：有 Bot 在干活时才走。 */
function useNow(ticking: boolean): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    setNow(Date.now());
    if (!ticking) return;
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [ticking]);
  return now;
}

/** 右边的「执行」页：每个 Bot 一张卡片，写着它此刻在做什么、做到第几步、用了多久。 */
export function ExecutionPanel({ cards, onJump, onShowTask }: Props) {
  const now = useNow(cards.some((card) => card.since !== undefined));
  if (cards.length === 0) return <p className="sa-panel-empty">这个房间还没有 Bot。</p>;
  return (
    <ul className="sa-exec-list" aria-label="每个 Bot 在做什么">
      {cards.map((card) => (
        <ExecItem key={card.botId} card={card} now={now} onJump={onJump} onShowTask={onShowTask} />
      ))}
    </ul>
  );
}

function ExecItem({ card, now, onJump, onShowTask }: { card: ExecCard; now: number } & Omit<Props, "cards">) {
  const busy = card.since !== undefined;
  const recent = card.steps.slice(-RECENT);
  const hidden = card.steps.length - recent.length;
  return (
    <li className="sa-exec" data-state={card.state} data-busy={busy}>
      <div className="sa-exec-head">
        <BotAvatar id={card.botId} name={card.name} state={card.state} />
        <span className="sa-exec-name">{card.name}</span>
        <span className="sa-exec-tier">{TIER[card.tier]}</span>
        <span className="sa-exec-state">{EXEC_LABEL[card.state]}</span>
        <span className="sa-exec-time">
          {busy ? (
            <time title="这一次回复用了多久">{elapsed(now - card.since!)}</time>
          ) : card.lastAt !== undefined ? (
            <time title="上一次说完话">{ago(now - card.lastAt)}</time>
          ) : null}
        </span>
      </div>
      {card.state !== "idle" && card.state !== "off" && <p className="sa-exec-text">{card.text}</p>}
      {card.taskTitle && (
        <button type="button" className="sa-exec-task" onClick={() => card.taskId && onShowTask(card.taskId)}>
          <Icon name="tasks" size={12} />
          <span>{card.taskTitle}</span>
        </button>
      )}
      {card.steps.length > 0 && (
        <>
          <div className="sa-exec-progress">
            <ol className="sa-exec-bar" aria-hidden="true">
              {card.steps.map((step) => (
                <li key={step.id} data-state={step.state} />
              ))}
            </ol>
            <span>
              {card.steps.length} 步 · 做完 {card.done}
              {card.failed > 0 && <em> · {card.failed} 步没成</em>}
            </span>
          </div>
          <ul className="sa-steps sa-exec-steps" aria-label="最近几步">
            {hidden > 0 && <li className="sa-exec-more">前面还有 {hidden} 步</li>}
            {recent.map((step) => (
              <li key={step.id} className="sa-step" data-state={step.state}>
                <div className="sa-step-line">
                  <span className="sa-step-icon" aria-hidden="true">
                    <Icon name={stepIcon(step.tool)} size={13} />
                  </span>
                  <span className="sa-step-title" title={step.title}>
                    {step.title}
                  </span>
                  <span className="sa-step-state">
                    <span className="sa-step-dot" aria-hidden="true" />
                    {STEP_LABEL[step.state]}
                  </span>
                </div>
              </li>
            ))}
          </ul>
        </>
      )}
      {card.messageId && busy && (
        <div className="sa-exec-actions">
          <button
            type="button"
            className={card.state === "approval" ? "sa-button sa-exec-approve" : "sa-link"}
            onClick={() => onJump(card)}
          >
            {card.state === "approval" ? "去批准" : "在主线上看"}
          </button>
        </div>
      )}
    </li>
  );
}
