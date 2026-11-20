//! `sa usage`：在终端里看花了多少。
//! 和 `sa chat` 一样直接用数据目录，所以不能和 `sa serve` 同时运行；服务开着时请在界面的「用量」里看。

use crate::app::{init_logging, App, RunArgs};
use crate::paths::DataDir;
use anyhow::Result;
use sa_protocol::{SpendRow, UsageSummary};
use std::io::Write;
use std::process::ExitCode;

pub fn run(data_dir: &DataDir) -> Result<ExitCode> {
    init_logging(tracing::Level::WARN);
    // 只是看账，不调用模型：用假模型打开，没配密钥也能看。
    let args = RunArgs {
        workdir: None,
        mock: true,
    };
    let app = App::open(data_dir, &args)?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            let summary = app.orchestrator.usage_summary().await;
            let notices = app.engine.list_notices();
            app.orchestrator.shutdown().await;
            let mut out = std::io::stdout();
            print(&summary?, &mut out)?;
            for notice in notices? {
                writeln!(out, "提醒：{}", notice.text)?;
            }
            Ok(ExitCode::SUCCESS)
        })
}

fn limit(limit: f64, currency: &str) -> String {
    if limit > 0.0 {
        format!("{limit:.2} {currency}")
    } else {
        "不限".to_string()
    }
}

pub fn print(summary: &UsageSummary, out: &mut impl Write) -> Result<()> {
    let currency = &summary.currency;
    let today = &summary.today;
    writeln!(
        out,
        "今天花了 {:.2} {currency}（每日上限 {}，每个任务上限 {}）",
        today.cost,
        limit(summary.daily_limit, currency),
        limit(summary.task_limit, currency)
    )?;
    writeln!(
        out,
        "调用模型 {} 次，输入 {} token（其中命中缓存 {}），输出 {} token",
        today.calls, today.input_tokens, today.cached_tokens, today.output_tokens
    )?;
    let sections: [(&str, &[SpendRow]); 4] = [
        ("最近 7 天", &summary.days),
        ("今天按模型", &summary.models),
        ("今天按 Bot", &summary.bots),
        ("今天花得最多的任务", &summary.tasks),
    ];
    for (title, rows) in sections {
        if rows.is_empty() {
            continue;
        }
        writeln!(out, "\n{title}")?;
        for row in rows {
            writeln!(
                out,
                "  {}：{:.2} {currency}，{} 次调用",
                row.label, row.spend.cost, row.spend.calls
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sa_protocol::Spend;

    #[test]
    fn prints_limits_totals_and_non_empty_sections() {
        let spend = Spend {
            cost: 1.5,
            input_tokens: 1000,
            output_tokens: 200,
            cached_tokens: 400,
            calls: 3,
        };
        let summary = UsageSummary {
            currency: "CNY".into(),
            daily_limit: 50.0,
            task_limit: 0.0,
            today: spend.clone(),
            days: vec![SpendRow {
                key: "2026-11-20".into(),
                label: "2026-11-20".into(),
                spend: spend.clone(),
            }],
            models: vec![],
            bots: vec![SpendRow {
                key: "bot_1".into(),
                label: "助手".into(),
                spend,
            }],
            tasks: vec![],
        };
        let mut out = Vec::new();
        print(&summary, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("今天花了 1.50 CNY（每日上限 50.00 CNY，每个任务上限 不限）"));
        assert!(text.contains("输入 1000 token（其中命中缓存 400），输出 200 token"));
        assert!(text.contains("最近 7 天\n  2026-11-20：1.50 CNY，3 次调用"));
        assert!(text.contains("今天按 Bot\n  助手：1.50 CNY，3 次调用"));
        assert!(!text.contains("今天按模型"));
        assert!(!text.contains("今天花得最多的任务"));
    }
}
