import { describe, expect, it } from "vitest";
import type { Spend, UsageSummary } from "./types";
import { dailyBudget, money, spendDetail, tokens } from "./usage";

const spend = (cost: number): Spend => ({
  cost,
  input_tokens: 12_345,
  output_tokens: 678,
  cached_tokens: 0,
  calls: 3,
});
const summary = (cost: number, daily_limit: number): UsageSummary => ({
  currency: "CNY",
  daily_limit,
  task_limit: 10,
  today: spend(cost),
  days: [],
  models: [],
  bots: [],
  tasks: [],
});

describe("dailyBudget", () => {
  it("is calm below 80%, warns from 80%, and says calls are paused at the limit", () => {
    expect(dailyBudget(summary(10, 50))).toEqual({
      percent: 20,
      level: "ok",
      text: "今天花了 10.00 CNY / 上限 50.00 CNY",
    });
    expect(dailyBudget(summary(40, 50)).level).toBe("near");
    const over = dailyBudget(summary(62.5, 50));
    expect(over.level).toBe("over");
    expect(over.percent).toBe(100);
    expect(over.text).toContain("模型调用暂停到明天");
  });

  it("has no bar when there is no daily limit", () => {
    const budget = dailyBudget(summary(3, 0));
    expect(budget.percent).toBeNull();
    expect(budget.level).toBe("ok");
    expect(budget.text).toBe("今天花了 3.00 CNY，没有设每日上限");
  });
});

describe("formatting", () => {
  it("writes money with two decimals and large token counts in 万", () => {
    expect(money(1.005, "USD")).toBe("1.00 USD");
    expect(tokens(9_999)).toBe("9999");
    expect(tokens(12_345)).toBe("1.2 万");
  });

  it("mentions cached tokens only when there are some", () => {
    expect(spendDetail(spend(1))).toBe("3 次调用 · 输入 1.2 万 · 输出 678");
    expect(spendDetail({ ...spend(1), cached_tokens: 500 })).toBe(
      "3 次调用 · 输入 1.2 万（缓存 500） · 输出 678",
    );
  });
});
