import type { Spend, UsageSummary } from "./types";

/** 金额：两位小数，带货币。 */
export function money(amount: number, currency: string): string {
  return `${amount.toFixed(2)} ${currency}`;
}

/** token 数：一万以上写成「1.2 万」。 */
export function tokens(count: number): string {
  if (count < 10_000) return String(count);
  return `${(count / 10_000).toFixed(1)} 万`;
}

export type BudgetLevel = "ok" | "near" | "over";

export interface DailyBudget {
  /** 今天花了上限的百分之几，最多 100。没有上限时是 `null`。 */
  percent: number | null;
  level: BudgetLevel;
  text: string;
}

/** 今天的花费和每日上限的关系：80% 以上算「快到了」，到了上限模型调用会暂停。 */
export function dailyBudget(summary: UsageSummary): DailyBudget {
  const spent = summary.today.cost;
  const limit = summary.daily_limit;
  if (limit <= 0) {
    return { percent: null, level: "ok", text: `今天花了 ${money(spent, summary.currency)}，没有设每日上限` };
  }
  const ratio = spent / limit;
  const level: BudgetLevel = ratio >= 1 ? "over" : ratio >= 0.8 ? "near" : "ok";
  const tail =
    level === "over" ? "，已到上限，模型调用暂停到明天" : level === "near" ? "，快到上限了" : "";
  return {
    percent: Math.min(100, Math.round(ratio * 100)),
    level,
    text: `今天花了 ${money(spent, summary.currency)} / 上限 ${money(limit, summary.currency)}${tail}`,
  };
}

/** 一行用量的说明：调用几次、输入输出多少 token。 */
export function spendDetail(spend: Spend): string {
  const cached = spend.cached_tokens > 0 ? `（缓存 ${tokens(spend.cached_tokens)}）` : "";
  return `${spend.calls} 次调用 · 输入 ${tokens(spend.input_tokens)}${cached} · 输出 ${tokens(spend.output_tokens)}`;
}
