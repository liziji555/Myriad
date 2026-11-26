import { useEffect, useState } from "react";
import { api } from "../api/client";
import type { SpendRow, UsageSummary } from "../api/types";
import { dailyBudget, money, spendDetail, tokens } from "../api/usage";

const REFRESH_MS = 15_000;

type BreakdownKey = "models" | "bots" | "tasks";

interface Props {
  /** 变了就重新拉一次（比如来了新的提醒）。 */
  refreshKey: number;
}

/** 右栏的「用量与预算」页：紧凑卡片概览 + 7 日迷你趋势柱图 + 分段明细。 */
export function UsagePanel({ refreshKey }: Props) {
  const [summary, setSummary] = useState<UsageSummary | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [breakdown, setBreakdown] = useState<BreakdownKey>("models");

  useEffect(() => {
    let stale = false;
    const load = () =>
      api.usage().then(
        (next) => {
          if (stale) return;
          setSummary(next);
          setError(null);
        },
        (cause: unknown) => {
          if (!stale) setError(cause instanceof Error ? cause.message : String(cause));
        },
      );
    void load();
    const timer = setInterval(load, REFRESH_MS);
    return () => {
      stale = true;
      clearInterval(timer);
    };
  }, [refreshKey]);

  if (!summary) {
    return <p className="sa-panel-empty">{error ? `用量没拉到：${error}` : "正在加载用量…"}</p>;
  }

  const budget = dailyBudget(summary);
  const { currency, today } = summary;
  const peak = Math.max(...summary.days.map((day) => day.spend.cost), 0);

  const tabs: readonly { id: BreakdownKey; label: string; rows: readonly SpendRow[]; empty: string }[] = [
    { id: "models", label: "按模型", rows: summary.models, empty: "今天还没有调用过模型。" },
    { id: "bots", label: "按队员", rows: summary.bots, empty: "今天还没有队员产生调用。" },
    { id: "tasks", label: "按任务", rows: summary.tasks, empty: "今天还没有任务产生花费。" },
  ];
  const activeTab = tabs.find((t) => t.id === breakdown) ?? tabs[0]!;

  return (
    <div className="sa-usage">
      {error && (
        <p className="sa-usage-stale" role="status">
          刷新失败，下面是之前的数：{error}
        </p>
      )}

      {/* 1. 今日预算紧凑主卡 */}
      <section className="sa-usage-card sa-usage-today" data-level={budget.level}>
        <div className="sa-usage-card-top">
          <span className="sa-usage-kicker">今日花费</span>
          <span className="sa-usage-limit-pill">
            单任务上限 {summary.task_limit > 0 ? money(summary.task_limit, currency) : "不限"}
          </span>
        </div>

        <div className="sa-usage-amount-hero">
          <strong>{today.cost.toFixed(2)}</strong>
          <span className="sa-usage-currency">{currency}</span>
          {summary.daily_limit > 0 && (
            <span className="sa-usage-cap">
              / {summary.daily_limit.toFixed(2)} {currency}
            </span>
          )}
          {budget.percent !== null && <span className="sa-usage-pct">{budget.percent}%</span>}
        </div>

        {budget.percent !== null && (
          <div
            className="sa-usage-bar"
            role="progressbar"
            aria-label="今天的花费占每日上限的比例"
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={budget.percent}
          >
            <span style={{ width: `${budget.percent}%` }} />
          </div>
        )}

        <div className="sa-usage-kpis">
          <div className="sa-usage-kpi">
            <span>调用</span>
            <strong>{today.calls} 次</strong>
          </div>
          <div className="sa-usage-kpi">
            <span>输入</span>
            <strong>
              {tokens(today.input_tokens)}
              {today.cached_tokens > 0 ? ` (${tokens(today.cached_tokens)} 缓存)` : ""}
            </strong>
          </div>
          <div className="sa-usage-kpi">
            <span>输出</span>
            <strong>{tokens(today.output_tokens)}</strong>
          </div>
        </div>
      </section>

      {/* 2. 最近 7 天迷你竖柱趋势（紧凑不占屏） */}
      <section className="sa-usage-card">
        <div className="sa-usage-card-top">
          <h3 className="sa-usage-title">最近 7 天趋势</h3>
          <span className="sa-usage-peak">
            峰值 {money(peak, currency)}
          </span>
        </div>
        <ol className="sa-usage-bars7" aria-label="最近 7 天花费">
          {summary.days.map((day, idx) => {
            const pct = peak > 0 ? Math.max((day.spend.cost / peak) * 100, day.spend.cost > 0 ? 8 : 0) : 0;
            const shortDate = day.label.length >= 5 ? day.label.slice(-5) : day.label;
            const isToday = idx === summary.days.length - 1;
            return (
              <li
                key={day.key}
                className="sa-usage-bar7-col"
                data-today={isToday || undefined}
                title={`${day.label}：${money(day.spend.cost, currency)}（${spendDetail(day.spend)}）`}
              >
                <div className="sa-usage-bar7-track" aria-hidden="true">
                  <span style={{ height: `${pct}%` }} />
                </div>
                <span className="sa-usage-bar7-date">{isToday ? "今天" : shortDate}</span>
              </li>
            );
          })}
        </ol>
      </section>

      {/* 3. 今日分类明细（三合一紧凑分段卡） */}
      <section className="sa-usage-card">
        <div className="sa-usage-card-top">
          <h3 className="sa-usage-title">今日明细</h3>
          <div className="sa-panel-subswitch" role="tablist" aria-label="明细维度">
            {tabs.map((item) => (
              <button
                key={item.id}
                type="button"
                role="tab"
                aria-selected={breakdown === item.id}
                onClick={() => setBreakdown(item.id)}
              >
                {item.label}
              </button>
            ))}
          </div>
        </div>

        {activeTab.rows.length === 0 ? (
          <p className="sa-usage-empty">{activeTab.empty}</p>
        ) : (
          <ul className="sa-usage-rows">
            {activeTab.rows.map((row) => (
              <li key={row.key} className="sa-usage-row">
                <span className="sa-row-label">{row.label}</span>
                <span className="sa-usage-amount">{money(row.spend.cost, currency)}</span>
                <span className="sa-usage-detail">{spendDetail(row.spend)}</span>
              </li>
            ))}
          </ul>
        )}
      </section>

      <p className="sa-usage-footnote">
        按 config.toml 中 [prices] 单价估算，与模型服务实际账单可能有细微出入。
      </p>
    </div>
  );
}
