//! 内核：一个会话怎么记、一轮对话怎么跑。
//!
//! - [`Transcript`]：会话的对话记录，一行一条追加到 `sessions/<id>.jsonl`。
//! - [`run_turn`]：调模型 → 执行它要的工具 → 把结果交回去，直到模型不再要工具。
//!
//! 这一层不知道房间、Bot、数据库：输入从 [`Inbox`] 来，过程通过 [`Observer`] 报出去。

mod compact;
mod transcript;
mod turn;

pub use compact::compact_threshold;
pub use transcript::{Entry, EntryBody, Transcript, TranscriptError};
pub use turn::{
    fixed_context, run_turn, ApprovalRequest, Inbox, Observer, StepEnd, TurnConfig, TurnEnd,
    TurnOutcome, UserInput,
};
