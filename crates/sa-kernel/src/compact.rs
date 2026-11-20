//! 压缩：对话快把模型的上下文窗口占满时，把前面的部分换成一份摘要。

use crate::transcript::{EntryBody, Transcript};
use sa_llm::{ChatMessage, ToolSpec};

pub(crate) const COMPACT_PROMPT: &str = include_str!("../prompts/compact.md");

/// 给模型输出留的位置最多按这么多算。
const MAX_OUTPUT_RESERVE: usize = 20_000;
/// 离上限还有这么多就开始压缩，不等到真的放不下。
const SAFETY_MARGIN: usize = 13_000;
/// 写摘要时，每条工具结果最多带这么多字。
const TOOL_RESULT_CHARS: usize = 2_000;

/// 一个窗口里什么时候该压缩、最多能放多少。单位都是估算的 token。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    /// 超过它就压缩。
    pub threshold: usize,
    /// 超过它就不能再调模型了。
    pub hard: usize,
}

impl Limits {
    /// 压缩线 = 窗口 − min(最大输出, 20,000) − 13,000。
    ///
    /// 窗口很小时这两项按比例收着算，不会把压缩线算成零。
    pub fn new(window: usize, max_output: Option<u32>) -> Self {
        let reserve = max_output
            .map_or(MAX_OUTPUT_RESERVE, |n| n as usize)
            .min(MAX_OUTPUT_RESERVE)
            .min(window / 2);
        let hard = window - reserve;
        Self {
            threshold: hard.saturating_sub(SAFETY_MARGIN).max(hard / 2),
            hard,
        }
    }

    /// 压缩时原样留下的最近几条，最多占这么多。
    pub fn keep(&self) -> usize {
        self.threshold / 4
    }
}

/// 粗略估算一段文字有多少 token：中文等非 ASCII 字符一个算一个，ASCII 四个算一个。
///
/// 宁可估多：估少了会在窗口放不下时才发现。
pub(crate) fn estimate(text: &str) -> usize {
    let ascii = text.bytes().filter(u8::is_ascii).count();
    let other = text.chars().filter(|c| !c.is_ascii()).count();
    other + ascii.div_ceil(4)
}

fn estimate_message(message: &ChatMessage) -> usize {
    // 每条消息自己的格式开销。
    4 + estimate(&message.content)
        + message.reasoning.as_deref().map_or(0, estimate)
        + message
            .tool_calls
            .iter()
            .map(|call| 8 + estimate(&call.name) + estimate(&call.arguments))
            .sum::<usize>()
}

pub(crate) fn estimate_messages(messages: &[ChatMessage]) -> usize {
    messages.iter().map(estimate_message).sum()
}

pub(crate) fn estimate_tools(specs: &[ToolSpec]) -> usize {
    specs
        .iter()
        .map(|spec| {
            8 + estimate(&spec.name)
                + estimate(&spec.description)
                + estimate(&spec.parameters.to_string())
        })
        .sum()
}

fn estimate_entry(body: &EntryBody) -> usize {
    match body {
        EntryBody::User { content, .. } => 4 + estimate(content),
        EntryBody::Assistant {
            content,
            reasoning,
            tool_calls,
        } => {
            4 + estimate(content)
                + reasoning.as_deref().map_or(0, estimate)
                + tool_calls
                    .iter()
                    .map(|call| 8 + estimate(&call.name) + estimate(&call.arguments))
                    .sum::<usize>()
        }
        EntryBody::ToolResult { content, .. } => 4 + estimate(content),
        EntryBody::Summary { .. } => 0,
    }
}

/// 一次压缩要做的事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    /// 最近的这么多条原样留下。
    pub kept: usize,
    /// 要写成摘要的那部分对话，已经排成文字。
    pub text: String,
}

/// 决定哪些留下、哪些写成摘要。没有东西可压缩时返回 `None`。
///
/// 从最新的一条往回数，在 `keep_budget` 以内的留下；切口不会落在工具调用和它的结果中间。
/// 人刚说的、模型还没回应的话，只要不超过 `verbatim_budget` 就原样留下（哪怕超过 `keep_budget`）：
/// 模型要回答的是原话，不是它的摘要。超过了就和别的条目一样处理，
/// 否则一条放不下的消息会永远留在那里，这个会话再也发不出去。
pub(crate) fn plan(
    transcript: &Transcript,
    keep_budget: usize,
    verbatim_budget: usize,
    text_budget: usize,
) -> Option<Plan> {
    let entries = transcript.entries();
    let start = transcript.live_start();
    let mut cut = entries.len();
    let mut total = 0;
    while cut > start && matches!(entries[cut - 1].body, EntryBody::User { .. }) {
        total += estimate_entry(&entries[cut - 1].body);
        cut -= 1;
    }
    if total > verbatim_budget {
        (cut, total) = (entries.len(), 0);
    }
    while cut > start {
        total += estimate_entry(&entries[cut - 1].body);
        if total > keep_budget {
            break;
        }
        cut -= 1;
    }
    // 留下的部分不能以工具结果开头：它对应的调用已经被压缩掉了，模型接口不接受。
    while cut < entries.len() && matches!(entries[cut].body, EntryBody::ToolResult { .. }) {
        cut += 1;
    }
    let old: Vec<&EntryBody> = entries[start..cut]
        .iter()
        .map(|entry| &entry.body)
        .filter(|body| !matches!(body, EntryBody::Summary { .. }))
        .collect();
    if old.is_empty() {
        return None;
    }

    let mut text = String::new();
    if let Some(summary) = transcript.last_summary() {
        text.push_str("【更早的对话的摘要】\n");
        text.push_str(summary);
        text.push_str("\n\n");
    }
    text.push_str("【对话记录】\n");
    for body in old {
        render(body, &mut text);
    }
    Some(Plan {
        kept: entries.len() - cut,
        text: fit_tail(text, text_budget),
    })
}

fn render(body: &EntryBody, out: &mut String) {
    match body {
        EntryBody::User { content, .. } => {
            out.push_str("用户：");
            out.push_str(content);
            out.push('\n');
        }
        EntryBody::Assistant {
            content,
            tool_calls,
            ..
        } => {
            if !content.is_empty() {
                out.push_str("助手：");
                out.push_str(content);
                out.push('\n');
            }
            for call in tool_calls {
                out.push_str(&format!("助手调用工具 {}：{}\n", call.name, call.arguments));
            }
        }
        EntryBody::ToolResult {
            content, is_error, ..
        } => {
            out.push_str(if *is_error {
                "工具报错："
            } else {
                "工具结果："
            });
            let mut chars = content.chars();
            out.extend(chars.by_ref().take(TOOL_RESULT_CHARS));
            if chars.next().is_some() {
                out.push_str("…（后面省略）");
            }
            out.push('\n');
        }
        EntryBody::Summary { .. } => {}
    }
}

/// 要总结的内容本身也放不进窗口时，丢掉最早的部分，留下最近的。
fn fit_tail(text: String, budget: usize) -> String {
    if estimate(&text) <= budget {
        return text;
    }
    const NOTE: &str = "（更早的内容太长，已经省略。）\n";
    let budget = budget.saturating_sub(estimate(NOTE));
    let mut used = 0;
    let mut start = text.len();
    for (index, c) in text.char_indices().rev() {
        // 和 `estimate` 一样的算法，按四分之一 token 记账。
        used += if c.is_ascii() { 1 } else { 4 };
        if used > budget * 4 {
            break;
        }
        start = index;
    }
    format!("{NOTE}{}", &text[start..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use sa_llm::ToolCall;

    #[test]
    fn limits_follow_the_formula_and_stay_sane_for_small_windows() {
        // 窗口 − min(最大输出, 20,000) − 13,000
        assert_eq!(
            Limits::new(64_000, None),
            Limits {
                threshold: 31_000,
                hard: 44_000
            }
        );
        assert_eq!(Limits::new(128_000, Some(8_000)).threshold, 107_000);
        assert_eq!(Limits::new(128_000, Some(64_000)).threshold, 95_000);
        // 小窗口：不会算成零，压缩线在上限以下。
        let small = Limits::new(2_000, None);
        assert_eq!((small.threshold, small.hard), (500, 1_000));
        assert_eq!(Limits::new(0, None).threshold, 0);
    }

    #[test]
    fn estimate_counts_chinese_per_char_and_ascii_per_four() {
        assert_eq!(estimate(""), 0);
        assert_eq!(estimate("你好世界"), 4);
        assert_eq!(estimate("abcdefgh"), 2);
        assert_eq!(estimate("abcde"), 2);
        assert_eq!(estimate("你好 ok"), 3);
    }

    fn transcript(bodies: Vec<EntryBody>) -> (tempfile::TempDir, Transcript) {
        let dir = tempfile::tempdir().unwrap();
        let mut transcript = Transcript::open(&dir.path().join("t.jsonl")).unwrap();
        for body in bodies {
            transcript.append(body).unwrap();
        }
        (dir, transcript)
    }

    fn user(text: &str) -> EntryBody {
        EntryBody::User {
            content: text.into(),
            message_id: None,
            message_seq: None,
        }
    }

    fn calls(id: &str) -> EntryBody {
        EntryBody::Assistant {
            content: String::new(),
            reasoning: None,
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: "read".into(),
                arguments: r#"{"path":"a.md"}"#.into(),
            }],
        }
    }

    fn result(id: &str, text: &str) -> EntryBody {
        EntryBody::ToolResult {
            tool_call_id: id.into(),
            content: text.into(),
            is_error: false,
        }
    }

    fn says(text: &str) -> EntryBody {
        EntryBody::Assistant {
            content: text.into(),
            reasoning: None,
            tool_calls: vec![],
        }
    }

    #[test]
    fn plan_keeps_the_newest_entries_and_renders_the_rest() {
        let (_dir, transcript) = transcript(vec![
            user("第一个问题"),
            calls("c1"),
            result("c1", &"长".repeat(3_000)),
            says("第一个回答"),
            user("第二个问题"),
        ]);
        // 预算只够最后一条。
        let plan = plan(&transcript, 12, 100_000, 100_000).unwrap();
        assert_eq!(plan.kept, 1);
        assert!(
            plan.text.starts_with("【对话记录】\n用户：第一个问题\n"),
            "{}",
            plan.text
        );
        assert!(plan.text.contains(r#"助手调用工具 read：{"path":"a.md"}"#));
        assert!(plan.text.contains("…（后面省略）"));
        assert_eq!(plan.text.matches('长').count(), TOOL_RESULT_CHARS);
        assert!(plan.text.ends_with("助手：第一个回答\n"));
        assert!(!plan.text.contains("第二个问题"));
    }

    #[test]
    fn plan_never_starts_the_kept_part_with_a_tool_result() {
        let (_dir, transcript) = transcript(vec![
            user("问题"),
            calls("c1"),
            result("c1", "结果一"),
            result("c1b", "结果二"),
        ]);
        // 预算够最后两条结果，但它们的调用留不下：那就一条都不留。
        let plan = plan(&transcript, 16, 100_000, 100_000).unwrap();
        assert_eq!(plan.kept, 0);
        assert!(plan.text.contains("结果二"));
    }

    #[test]
    fn plan_keeps_what_the_user_just_said_as_long_as_it_fits() {
        let (_dir, log) = transcript(vec![
            user("以前的问题"),
            says("以前的回答"),
            user(&"刚贴的一大段".repeat(500)),
            user("还有一句"),
        ]);
        // 留存预算远小于刚发的这两条，但它们放得下：原样留下。
        let made = plan(&log, 10, 100_000, 100_000).unwrap();
        assert_eq!(made.kept, 2);
        assert_eq!(
            made.text,
            "【对话记录】\n用户：以前的问题\n助手：以前的回答\n"
        );
        // 放不下：和别的条目一样，只留预算以内的。
        let made = plan(&log, 10, 1_000, 100_000).unwrap();
        assert_eq!(made.kept, 1);
        assert!(made.text.ends_with("刚贴的一大段\n"), "{}", made.text);

        // 只有刚说的话：放得下就没有可以压缩的；放不下就连它一起压缩，不能卡死在那里。
        let (_dir, only) = transcript(vec![user(&"长".repeat(5_000))]);
        assert_eq!(plan(&only, 10, 100_000, 100_000), None);
        assert_eq!(plan(&only, 10, 1_000, 100_000).unwrap().kept, 0);
    }

    #[test]
    fn plan_has_nothing_to_do_when_everything_fits_in_the_kept_part() {
        let (_dir, transcript) = transcript(vec![user("短"), says("好")]);
        assert_eq!(plan(&transcript, 1_000, 100_000, 100_000), None);
    }

    #[test]
    fn plan_builds_on_the_previous_summary() {
        let (_dir, transcript) = transcript(vec![
            user("很久以前"),
            says("很久以前的回答"),
            user("留下的一条"),
            EntryBody::Summary {
                content: "上次的摘要".into(),
                kept: 1,
            },
            says("之后的回答"),
            user("最新的问题"),
        ]);
        let plan = plan(&transcript, 10, 100_000, 100_000).unwrap();
        assert_eq!(plan.kept, 1);
        assert_eq!(
            plan.text,
            "【更早的对话的摘要】\n上次的摘要\n\n【对话记录】\n用户：留下的一条\n助手：之后的回答\n"
        );
    }

    #[test]
    fn oversized_material_keeps_only_its_tail() {
        let text = format!("{}结尾", "头".repeat(1_000));
        let fitted = fit_tail(text.clone(), 100);
        assert!(fitted.starts_with("（更早的内容太长"));
        assert!(fitted.ends_with("结尾"));
        assert!(estimate(&fitted) <= 100, "{}", estimate(&fitted));
        assert_eq!(fit_tail(text.clone(), 5_000), text);
    }
}
