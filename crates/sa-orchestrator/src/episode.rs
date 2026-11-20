//! 一轮交流：从人说一句话开始，到没有 Bot 再要说话为止。
//!
//! 这里只管记账和上限：这一轮里 Bot 说了几次、谁在排队、谁找过谁。
//! 上限写在代码里，不靠提示词：模型不听话时也停得下来。

use sa_protocol::BotId;
use std::collections::{HashMap, VecDeque};

/// 人说一句话，最多引出这么多次 Bot 发言。
pub const MAX_BOT_TURNS: usize = 6;
/// Bot 找 Bot 最多转这么多手。
pub const MAX_HOP: u32 = 3;
/// 同一对 Bot 在一轮交流里最多互相找这么多次；再找就是在客套了。
pub const MAX_PAIR_ASKS: usize = 2;
/// 和最近这么多条 Bot 的话比，几乎一样的算复读。
const RECENT: usize = 6;
const REPEAT_SIMILARITY: f64 = 0.9;

/// 排队等着说话的 Bot。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Job {
    pub bot_id: BotId,
    /// 这次发言是转到第几手：被人叫到是 0。
    pub hop: u32,
}

#[derive(Debug, Default)]
pub(crate) struct Episode {
    /// 这一轮里已经开始的 Bot 发言次数。
    turns: usize,
    queue: VecDeque<Job>,
    /// 有定时任务、外面来的事等着它看的 Bot。这些事只有它看得到，所以这一趟欠着它的：
    /// 人说了新的话、又来了别的事都不会把它挤掉，排着队的说完了就轮到它。
    owed: VecDeque<BotId>,
    /// 每一对 Bot 之间互相找过几次。
    pairs: HashMap<(BotId, BotId), usize>,
    /// 最近几条 Bot 说的话（归一化之后），用来发现复读。
    recent: VecDeque<String>,
}

impl Episode {
    /// 人说了新的话：之前排着的都不算了，重新记账，由 `first` 来接。欠着的还欠着。
    pub fn restart(&mut self, first: Option<Job>) {
        let mut owed = std::mem::take(&mut self.owed);
        owed.retain(|bot_id| first.as_ref().is_none_or(|job| &job.bot_id != bot_id));
        *self = Self {
            owed,
            ..Self::default()
        };
        self.queue.extend(first);
    }

    /// 有一件定时任务或者外面来的事是交给 `bot_id` 的：记着要让它说一次。已经记着、或者它本来就排着队的不重复记。
    pub fn owe(&mut self, bot_id: BotId) {
        let queued = self
            .queue
            .iter()
            .any(|job| job.hop == 0 && job.bot_id == bot_id);
        if !queued && !self.owed.contains(&bot_id) {
            self.owed.push_back(bot_id);
        }
    }

    /// 取下一个该说话的 Bot，并记一次发言。
    /// 正在说话的 Bot 读到了人说的新话：上限重新算（它这一次发言算新一轮的第一次），已经排上队的照旧。
    pub fn renew(&mut self) {
        self.turns = 1;
        self.pairs.clear();
        self.recent.clear();
    }

    pub fn next(&mut self) -> Option<Job> {
        if let Some(job) = self.queue.pop_front() {
            self.turns += 1;
            return Some(job);
        }
        // 排着队的都说完了，轮到欠着的。它看的是另一件事：算新的一轮。
        let bot_id = self.owed.pop_front()?;
        self.renew();
        Some(Job { bot_id, hop: 0 })
    }

    pub fn queued(&self) -> Vec<BotId> {
        let queue = self.queue.iter().map(|job| job.bot_id.clone());
        queue.chain(self.owed.iter().cloned()).collect()
    }

    /// 不让排着的 Bot 再说话（人点了停止）。
    pub fn clear_queue(&mut self) {
        self.owed.clear();
        self.queue.clear();
    }

    /// 撤回一次排队（话没发出去）。
    pub fn cancel(&mut self, job: &Job) {
        if let Some(index) = self.queue.iter().rposition(|queued| queued == job) {
            self.queue.remove(index);
        }
    }

    /// 记下一条 Bot 说过的话。
    pub fn remember(&mut self, text: &str) {
        let text = normalize(text);
        if text.is_empty() {
            return;
        }
        if self.recent.len() == RECENT {
            self.recent.pop_front();
        }
        self.recent.push_back(text);
    }

    /// `from`（它这次发言是第 `hop` 手）想找 `to` 说 `message`。可以就排上队；不可以就说明原因。
    pub fn ask(
        &mut self,
        from: &BotId,
        hop: u32,
        to: &BotId,
        message: &str,
    ) -> Result<Job, String> {
        if self.turns + self.queue.len() >= MAX_BOT_TURNS {
            return Err(format!(
                "这一轮里 Bot 已经发言 {MAX_BOT_TURNS} 次，到上限了，不能再找别的 Bot。请直接把目前的情况告诉用户。"
            ));
        }
        if hop + 1 > MAX_HOP {
            return Err(format!(
                "Bot 之间已经转了 {MAX_HOP} 手，不能再往下转。请直接把目前的情况告诉用户，由用户决定下一步。"
            ));
        }
        let pair = if from.as_str() <= to.as_str() {
            (from.clone(), to.clone())
        } else {
            (to.clone(), from.clone())
        };
        let asked = self.pairs.get(&pair).copied().unwrap_or(0);
        if asked >= MAX_PAIR_ASKS {
            return Err(format!(
                "你们两个在这一轮里已经互相找了 {MAX_PAIR_ASKS} 次，不能再来回了。请直接把结论或卡住的地方告诉用户。"
            ));
        }
        let normalized = normalize(message);
        if self
            .recent
            .iter()
            .any(|earlier| similarity(earlier, &normalized) >= REPEAT_SIMILARITY)
        {
            return Err(
                "这句话和刚才说过的几乎一样，没有发出去。没有新的内容就不要再找别的 Bot 了。"
                    .into(),
            );
        }
        self.pairs.insert(pair, asked + 1);
        self.remember(message);
        let job = Job {
            bot_id: to.clone(),
            hop: hop + 1,
        };
        self.queue.push_back(job.clone());
        Ok(job)
    }
}

/// 去掉空白和标点、统一大小写：只比较实际说了什么。
fn normalize(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// 两段话有多像：相邻两个字组成的集合的重合程度，0 到 1。
fn similarity(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    let grams = |text: &str| -> std::collections::HashSet<(char, char)> {
        let chars: Vec<char> = text.chars().collect();
        chars.windows(2).map(|pair| (pair[0], pair[1])).collect()
    };
    let (a, b) = (grams(a), grams(b));
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let shared = a.intersection(&b).count() as f64;
    shared / (a.len() as f64 + b.len() as f64 - shared)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bot(name: &str) -> BotId {
        BotId::from(format!("bot_{name}"))
    }

    fn started(first: &str) -> Episode {
        let mut episode = Episode::default();
        episode.restart(Some(Job {
            bot_id: bot(first),
            hop: 0,
        }));
        assert_eq!(episode.next().unwrap().bot_id, bot(first));
        episode
    }

    #[test]
    fn asks_queue_the_other_bot_one_hop_further() {
        let mut episode = started("a");
        let job = episode
            .ask(&bot("a"), 0, &bot("b"), "帮我看看这个")
            .unwrap();
        assert_eq!(
            job,
            Job {
                bot_id: bot("b"),
                hop: 1
            }
        );
        assert_eq!(episode.queued(), [bot("b")]);
        assert_eq!(episode.next(), Some(job));
        assert_eq!(episode.next(), None);
    }

    #[test]
    fn two_bots_being_polite_are_cut_off_at_the_third_ask() {
        let mut episode = started("a");
        episode
            .ask(&bot("a"), 0, &bot("b"), "辛苦看一下第一版")
            .unwrap();
        episode.next();
        episode
            .ask(&bot("b"), 1, &bot("a"), "看过了，你再确认下细节")
            .unwrap();
        episode.next();
        let error = episode
            .ask(&bot("a"), 2, &bot("b"), "好的，那再麻烦你一次")
            .unwrap_err();
        assert!(error.contains("互相找了 2 次"), "{error}");
        assert!(episode.queued().is_empty());
        // 换一个 Bot 还可以，但转手次数也有上限。
        episode
            .ask(&bot("a"), 2, &bot("c"), "你来看看结论对不对")
            .unwrap();
        episode.next();
        let error = episode
            .ask(&bot("c"), 3, &bot("d"), "再转给你确认一遍")
            .unwrap_err();
        assert!(error.contains("转了 3 手"), "{error}");
    }

    #[test]
    fn one_human_message_gets_at_most_six_bot_turns() {
        let mut episode = started("b0");
        // 每个 Bot 都找下一个：0 手的那次算第 1 次发言。
        let mut asked = 0;
        for index in 0..10 {
            let (from, to) = (bot(&format!("b{index}")), bot(&format!("b{}", index + 1)));
            // 不让转手上限先触发：这里只看发言次数。
            match episode.ask(&from, 0, &to, &format!("第 {index} 件不同的事情")) {
                Ok(_) => {
                    asked += 1;
                    episode.next();
                }
                Err(error) => {
                    assert!(error.contains("发言 6 次"), "{error}");
                    break;
                }
            }
        }
        assert_eq!(asked, MAX_BOT_TURNS - 1);

        // 排着队还没说的也算在内。
        let mut episode = started("x");
        for index in 0..MAX_BOT_TURNS - 1 {
            episode
                .ask(
                    &bot("x"),
                    0,
                    &bot(&format!("y{index}")),
                    &format!("问题 {index} 各不相同"),
                )
                .unwrap();
        }
        assert!(episode
            .ask(&bot("x"), 0, &bot("z"), "还有一个新问题")
            .is_err());
    }

    #[test]
    fn repeating_what_was_just_said_is_refused() {
        let mut episode = started("a");
        episode.remember("我已经检查过了，没有问题。");
        let error = episode
            .ask(&bot("a"), 0, &bot("b"), "我已经检查过了，没有问题！")
            .unwrap_err();
        assert!(error.contains("几乎一样"), "{error}");
        episode
            .ask(&bot("a"), 0, &bot("b"), "请你检查第二个文件的边界情况")
            .unwrap();
        // 被拒绝的不占名额。
        assert_eq!(episode.queued(), [bot("b")]);
    }

    #[test]
    fn a_new_human_message_starts_the_count_again() {
        let mut episode = started("a");
        episode.ask(&bot("a"), 0, &bot("b"), "第一件事").unwrap();
        episode.restart(Some(Job {
            bot_id: bot("c"),
            hop: 0,
        }));
        // 之前排着的 b 不再说话。
        assert_eq!(episode.queued(), [bot("c")]);
        episode.next();
        episode.ask(&bot("c"), 0, &bot("b"), "第一件事").unwrap();

        episode.clear_queue();
        assert_eq!(episode.next(), None);
    }

    #[test]
    fn a_bot_owed_a_turn_keeps_it_through_new_messages_until_stop() {
        let mut episode = Episode::default();
        episode.owe(bot("a"));
        episode.owe(bot("b"));
        episode.owe(bot("a"));
        assert_eq!(episode.queued(), [bot("a"), bot("b")]);
        assert_eq!(episode.next().unwrap().bot_id, bot("a"));

        // a 说话的时候人说了新的话，由 c 来接；又来了一件没人要看的事。b 还欠着。
        episode.restart(Some(Job {
            bot_id: bot("c"),
            hop: 0,
        }));
        episode.restart(None);
        assert_eq!(episode.queued(), [bot("b")]);
        // 人点名的正好是欠着的那个：只说一次。
        episode.restart(Some(Job {
            bot_id: bot("b"),
            hop: 0,
        }));
        episode.owe(bot("b"));
        assert_eq!(episode.queued(), [bot("b")]);
        assert_eq!(episode.next().unwrap().bot_id, bot("b"));
        assert_eq!(episode.next(), None);

        // 排着队的先说，欠着的排在后面；轮到它时上限重新算。
        let mut episode = started("a");
        episode.owe(bot("d"));
        episode.ask(&bot("a"), 0, &bot("b"), "看一下这个").unwrap();
        assert_eq!(episode.queued(), [bot("b"), bot("d")]);
        assert_eq!(episode.next().unwrap().bot_id, bot("b"));
        let job = episode.next().unwrap();
        assert_eq!((job.bot_id, job.hop), (bot("d"), 0));
        assert_eq!(episode.turns, 1);

        // 人点了停止：欠着的也不说了。
        episode.owe(bot("e"));
        episode.clear_queue();
        assert_eq!(episode.next(), None);
    }

    #[test]
    fn similarity_is_about_content_not_punctuation() {
        assert_eq!(
            similarity(&normalize("好的，收到！"), &normalize("好的 收到")),
            1.0
        );
        assert!(similarity(&normalize("请检查登录页的样式"), &normalize("今天天气不错")) < 0.2);
        assert_eq!(similarity("", "abc"), 0.0);
    }
}
