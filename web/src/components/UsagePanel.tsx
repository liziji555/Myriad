import { useEffect, useState } from "react";
import { api } from "../api/client";
import type { SpendRow, UsageSummary } from "../api/types";
import { dailyBudget, money, spendDetail } from "../api/usage";

const REFRESH_MS = 15_000;

interface Props {
  /** 变了就重新拉一次（比如来了新的提醒）。 */
  refreshKey: number;
}

/** 右栏的「用量」页：今天花了多少、离上限多远，最近几天和今天按模型、Bot、任务的明细。 */
export function UsagePanel({ refreshKey }: Props) {
  const [summary, setSummary] = useState<UsageSummary | null>(null);
  const [error, setError] = useState<string | null>(null);

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
  const { currency } = summary;
  const sections: { title: string; rows: readonly SpendRow[]; empty: string }[] = [
    { title: "今天按模型", rows: summary.models, empty: "今天还没有调用过模型。" },
    { title: "今天按 Bot", rows: summary.bots, empty: "" },
    { title: "今天花得最多的任务", rows: summary.tasks, empty: "" },
  ];
  const peak = Math.max(...summary.days.map((day) => day.spend.cost), 0);
  return (
    <div className="sa-usage">
      {error && (
        <p className="sa-usage-stale" role="status">
          刷新失败，下面是之前的数：{error}
        </p>
      )}
      <section className="sa-usage-today" data-level={budget.level}>
        <p className="sa-usage-headline">{budget.text}</p>
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
        <p className="sa-usage-note">{spendDetail(summary.today)}</p>
        <p className="sa-usage-note">
          每个任务上限：{summary.task_limit > 0 ? money(summary.task_limit, currency) : "不限"}
          。花费按 config.toml 里 [prices] 的价格估算，和模型服务实际扣的钱可能有出入。
        </p>
      </section>

      <section>
        <h3 className="sa-usage-title">最近 7 天</h3>
        <ol className="sa-usage-days">
          {summary.days.map((day) => (
            <li key={day.key} className="sa-usage-day">
              <span className="sa-row-meta">{day.label}</span>
              <span className="sa-usage-day-bar" aria-hidden="true">
                <span style={{ width: peak > 0 ? `${(day.spend.cost / peak) * 100}%` : "0" }} />
              </span>
              <span className="sa-usage-amount">{money(day.spend.cost, currency)}</span>
            </li>
          ))}
        </ol>
      </section>

      {sections.map(
        ({ title, rows, empty }) =>
          (rows.length > 0 || empty) && (
            <section key={title}>
              <h3 className="sa-usage-title">{title}</h3>
              {rows.length === 0 ? (
                <p className="sa-panel-empty">{empty}</p>
              ) : (
                <ul className="sa-usage-rows">
                  {rows.map((row) => (
                    <li key={row.key} className="sa-usage-row">
                      <span className="sa-row-label">{row.label}</span>
                      <span className="sa-usage-amount">{money(row.spend.cost, currency)}</span>
                      <span className="sa-usage-detail">{spendDetail(row.spend)}</span>
                    </li>
                  ))}
                </ul>
              )}
            </section>
          ),
      )}
    </div>
  );
}
