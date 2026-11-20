//! 五段式 cron（分 时 日 月 周），按本机时间算下一次到点。
//!
//! 只支持数字：`*`、`a`、`a-b`、`a,b`、`*/n`、`a-b/n`、`a/n`。周几 0 和 7 都是周日。
//! 「日」和「周」都写了具体值时，满足其中一个就算（和常见的 cron 一样）。

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, TimeZone, Timelike};
use sa_protocol::Millis;

/// 最多往后找这么多天：`0 0 29 2 *` 这种四年一次的也找得到。
const HORIZON_DAYS: i64 = 366 * 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    minutes: u64,
    hours: u64,
    days: u64,
    months: u64,
    weekdays: u64,
    any_day: bool,
    any_weekday: bool,
}

impl Schedule {
    pub fn parse(text: &str) -> Result<Self, String> {
        let fields: Vec<&str> = text.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = fields[..] else {
            return Err(format!(
                "时间要写成五段：分 时 日 月 周，比如每天早上九点是 `0 9 * * *`（现在是 {} 段）",
                fields.len()
            ));
        };
        // 周几的 7 也是周日。
        let weekdays = field(weekday, "周", 0, 7)?;
        let weekdays = (weekdays | (weekdays >> 7)) & 0x7f;
        Ok(Self {
            minutes: field(minute, "分", 0, 59)?,
            hours: field(hour, "时", 0, 23)?,
            days: field(day, "日", 1, 31)?,
            months: field(month, "月", 1, 12)?,
            weekdays,
            any_day: day == "*",
            any_weekday: weekday == "*",
        })
    }

    /// `after` 之后（不含）第一次到点，按本机时间。找不到（比如 2 月 30 日）返回 `None`。
    pub fn next_after(&self, after: Millis) -> Option<Millis> {
        let after = Local.timestamp_millis_opt(after).single()?;
        Some(self.next_in(after)?.timestamp_millis())
    }

    pub fn next_in<Tz: TimeZone>(&self, after: DateTime<Tz>) -> Option<DateTime<Tz>> {
        let zone = after.timezone();
        let start = after.naive_local();
        let mut date = start.date();
        for _ in 0..HORIZON_DAYS {
            if self.on(date) {
                for hour in (0..24).filter(|hour| has(self.hours, *hour)) {
                    for minute in (0..60).filter(|minute| has(self.minutes, *minute)) {
                        let Some(naive) = date.and_hms_opt(hour, minute, 0) else {
                            continue;
                        };
                        if date == start.date()
                            && (naive.hour(), naive.minute()) < (start.hour(), start.minute())
                        {
                            continue;
                        }
                        // 换夏令时：不存在的时刻跳过，出现两次的取早的那次。
                        let Some(at) = zone.from_local_datetime(&naive).earliest() else {
                            continue;
                        };
                        if at > after {
                            return Some(at);
                        }
                    }
                }
            }
            date += Duration::days(1);
        }
        None
    }

    fn on(&self, date: NaiveDate) -> bool {
        if !has(self.months, date.month()) {
            return false;
        }
        let day = has(self.days, date.day());
        let weekday = has(self.weekdays, date.weekday().num_days_from_sunday());
        match (self.any_day, self.any_weekday) {
            (false, false) => day || weekday,
            _ => day && weekday,
        }
    }
}

fn has(set: u64, value: u32) -> bool {
    set & (1 << value) != 0
}

/// 一段里的所有取值，按位记。
fn field(text: &str, name: &str, min: u32, max: u32) -> Result<u64, String> {
    let bad = || {
        format!(
            "「{name}」这一段看不懂：`{text}`（只能用数字、`*`、`-`、`,`、`/`，范围 {min}-{max}）"
        )
    };
    let number = |part: &str| -> Result<u32, String> {
        let value: u32 = part.parse().map_err(|_| bad())?;
        if value < min || value > max {
            return Err(bad());
        }
        Ok(value)
    };
    let mut set = 0u64;
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => (range, step.parse::<u32>().map_err(|_| bad())?),
            None => (part, 1),
        };
        if step == 0 {
            return Err(bad());
        }
        let (from, to) = match range.split_once('-') {
            _ if range == "*" => (min, max),
            Some((from, to)) => (number(from)?, number(to)?),
            // `5/15`：从 5 开始每 15 一次。
            None if part.contains('/') => (number(range)?, max),
            None => {
                let value = number(range)?;
                (value, value)
            }
        };
        if from > to {
            return Err(bad());
        }
        for value in (from..=to).step_by(step as usize) {
            set |= 1 << value;
        }
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, NaiveDateTime};

    fn at(text: &str) -> DateTime<FixedOffset> {
        let naive = NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").unwrap();
        FixedOffset::east_opt(8 * 3600)
            .unwrap()
            .from_local_datetime(&naive)
            .unwrap()
    }

    fn next(schedule: &str, after: &str) -> String {
        Schedule::parse(schedule)
            .unwrap()
            .next_in(at(after))
            .map(|time| time.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_default()
    }

    #[test]
    fn finds_the_next_time_strictly_after() {
        assert_eq!(next("0 9 * * *", "2026-03-01 08:59:59"), "2026-03-01 09:00");
        // 正好到点的那一刻不算，找下一次。
        assert_eq!(next("0 9 * * *", "2026-03-01 09:00:00"), "2026-03-02 09:00");
        assert_eq!(next("0 9 * * *", "2026-03-01 09:00:30"), "2026-03-02 09:00");
        assert_eq!(
            next("*/15 * * * *", "2026-03-01 10:07:00"),
            "2026-03-01 10:15"
        );
        assert_eq!(
            next("5/20 8-9 * * *", "2026-03-01 09:46:00"),
            "2026-03-02 08:05"
        );
        assert_eq!(
            next("30 18 * * 1-5", "2026-03-06 19:00:00"),
            "2026-03-09 18:30"
        );
        // 7 和 0 都是周日。
        assert_eq!(next("0 0 * * 7", "2026-03-02 00:00:00"), "2026-03-08 00:00");
        assert_eq!(next("0 0 * * 0", "2026-03-02 00:00:00"), "2026-03-08 00:00");
        assert_eq!(
            next("0 0 1 1,7 *", "2026-03-02 00:00:00"),
            "2026-07-01 00:00"
        );
        assert_eq!(
            next("0 0 29 2 *", "2026-03-01 00:00:00"),
            "2028-02-29 00:00"
        );
        // 日和周都写了：满足一个就算。
        assert_eq!(
            next("0 0 15 * 1", "2026-03-03 00:00:00"),
            "2026-03-09 00:00"
        );
        assert_eq!(
            next("0 0 15 * 1", "2026-03-10 00:00:00"),
            "2026-03-15 00:00"
        );
        // 永远到不了的日子。
        assert_eq!(next("0 0 30 2 *", "2026-03-01 00:00:00"), "");
    }

    #[test]
    fn mistakes_are_explained() {
        for (text, needle) in [
            ("0 9 * *", "五段"),
            ("0 9 * * * *", "五段"),
            ("60 9 * * *", "「分」"),
            ("0 24 * * *", "「时」"),
            ("0 9 0 * *", "「日」"),
            ("0 9 * 13 *", "「月」"),
            ("0 9 * * 8", "「周」"),
            ("0 9 * * mon", "「周」"),
            ("*/0 9 * * *", "「分」"),
            ("9-5 9 * * *", "「分」"),
            ("1,,2 9 * * *", "「分」"),
        ] {
            let error = Schedule::parse(text).unwrap_err();
            assert!(error.contains(needle), "{text}: {error}");
        }
    }
}
