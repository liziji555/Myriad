//! 会话的对话记录。
//!
//! 文件是 JSONL：一行一条，只追加。每条带 `uuid` 和 `parent_uuid`（上一条的 uuid），
//! 以后做压缩时在边界处把 `parent_uuid` 置空，加载时就只取边界之后的部分。

use sa_llm::{ChatMessage, ToolCall};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// 工具调用没有等到结果（进程中途退出）时补上的结果。
pub(crate) const LOST_RESULT: &str =
    "（上次运行在这个工具返回之前中断了，结果未知。需要的话请重新调用。）";

#[derive(Debug, thiserror::Error)]
pub enum TranscriptError {
    #[error("读写会话记录 {path} 失败：{source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("会话记录 {path} 的第 {line} 行损坏：{reason}")]
    Corrupt {
        path: PathBuf,
        line: usize,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub uuid: String,
    pub parent_uuid: Option<String>,
    /// 写入时间，Unix 毫秒。
    pub ts: i64,
    #[serde(flatten)]
    pub body: EntryBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EntryBody {
    User {
        content: String,
        /// 这条输入对应房间里的哪条消息。重启后靠 `message_seq` 判断哪些消息还没进过会话。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_seq: Option<i64>,
    },
    Assistant {
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    ToolResult {
        tool_call_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
    /// 压缩边界：这之前的对话用一段摘要代替。记录本身不删，只是不再发给模型。
    Summary {
        content: String,
        /// 紧挨在这条之前、仍然原样发给模型的条数（最近的几条不压缩）。
        #[serde(default)]
        kept: usize,
    },
}

/// 摘要发给模型时前面加的说明。
pub const SUMMARY_PREFIX: &str =
    "（之前的对话太长，下面是它的摘要。摘要之后是最近的原始对话。）\n\n";

pub struct Transcript {
    path: PathBuf,
    file: File,
    entries: Vec<Entry>,
    /// 文件里完整记录的总长度。写入失败时退回到这里。
    len: u64,
    /// 写入失败后又没能退回去：文件末尾可能留着半行，不能再往后写。
    broken: bool,
    /// 测试用：下一次写入只写这么多字节就报错。
    #[cfg(test)]
    fail_next_write_after: Option<usize>,
}

impl Transcript {
    /// 打开（没有就新建）一份记录并读入全部内容。
    ///
    /// 两种残缺会被就地修好：最后一行没写完（进程在写入中途退出）会被截掉；
    /// 没等到结果的工具调用会补一条「结果未知」。中间的行损坏则报错，不去猜。
    pub fn open(path: &Path) -> Result<Self, TranscriptError> {
        let io = |source| TranscriptError::Io {
            path: path.to_path_buf(),
            source,
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        // 记录里有用户说的话和工具读到的文件内容，只给当前用户读写。
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(path).map_err(io)?;
        let mut raw = Vec::new();
        file.read_to_end(&mut raw).map_err(io)?;

        let mut entries = Vec::new();
        let mut good_len = 0usize;
        let mut lines = raw.split_inclusive(|b| *b == b'\n').enumerate().peekable();
        while let Some((index, line)) = lines.next() {
            let complete = line.ends_with(b"\n");
            let is_last = lines.peek().is_none();
            let parsed = std::str::from_utf8(line)
                .map_err(|e| e.to_string())
                .and_then(|text| {
                    if text.trim().is_empty() {
                        Ok(None)
                    } else {
                        serde_json::from_str::<Entry>(text)
                            .map(Some)
                            .map_err(|e| e.to_string())
                    }
                });
            match parsed {
                Ok(entry) if complete => {
                    entries.extend(entry);
                    good_len += line.len();
                }
                // 最后一行没有换行符：写到一半。即使碰巧能解析也不算数。
                _ if is_last && !complete => break,
                Ok(_) => unreachable!("完整的行在上面处理了"),
                Err(reason) => {
                    return Err(TranscriptError::Corrupt {
                        path: path.to_path_buf(),
                        line: index + 1,
                        reason,
                    })
                }
            }
        }
        if good_len < raw.len() {
            tracing::warn!(path = %path.display(), "会话记录的最后一行没写完，已截掉");
            file.set_len(good_len as u64).map_err(io)?;
        }
        file.seek(SeekFrom::End(0)).map_err(io)?;

        let mut transcript = Self {
            path: path.to_path_buf(),
            file,
            entries,
            len: good_len as u64,
            broken: false,
            #[cfg(test)]
            fail_next_write_after: None,
        };
        transcript.close_open_tool_calls(LOST_RESULT)?;
        Ok(transcript)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// 追加一条并落盘（写完会 fsync）。返回之后这条记录就算断电也在。
    pub fn append(&mut self, body: EntryBody) -> Result<&Entry, TranscriptError> {
        let entry = Entry {
            uuid: Uuid::now_v7().simple().to_string(),
            parent_uuid: self.entries.last().map(|e| e.uuid.clone()),
            ts: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as i64),
            body,
        };
        let mut line = serde_json::to_string(&entry).expect("记录总能序列化成 JSON");
        line.push('\n');
        let path = self.path.clone();
        let io = move |source| TranscriptError::Io { path, source };
        if self.broken {
            return Err(io(std::io::Error::other(
                "上一次写入失败后文件没能恢复，重新启动后再试",
            )));
        }
        if let Err(error) = self.write_line(line.as_bytes()) {
            // 可能只写进去半行。不退回去的话，下一条会接在半行后面，拼成一行坏记录，
            // 以后整个文件都打不开。
            let restored = self
                .file
                .set_len(self.len)
                .and_then(|()| self.file.seek(SeekFrom::End(0)));
            self.broken = restored.is_err();
            return Err(io(error));
        }
        self.len += line.len() as u64;
        self.entries.push(entry);
        Ok(self.entries.last().expect("刚刚放进去"))
    }

    fn write_line(&mut self, line: &[u8]) -> std::io::Result<()> {
        #[cfg(test)]
        if let Some(written) = self.fail_next_write_after.take() {
            self.file.write_all(&line[..written.min(line.len())])?;
            return Err(std::io::Error::other("磁盘满了"));
        }
        self.file.write_all(line)?;
        self.file.sync_data()
    }

    /// 给还没有结果的工具调用补上结果，内容是 `note`。
    ///
    /// 模型接口要求每个工具调用后面都跟着它的结果，缺了整个会话就再也发不出去。
    pub fn close_open_tool_calls(&mut self, note: &str) -> Result<usize, TranscriptError> {
        // 按先后顺序配对，不按 id 全局去重：有的模型每一轮都用同样的 id（call_0、call_1…），
        // 前面某一轮答过的 id 不能算作后面这一次也答过了。
        let mut open: Vec<String> = vec![];
        for entry in &self.entries {
            match &entry.body {
                EntryBody::Assistant { tool_calls, .. } => {
                    open.extend(tool_calls.iter().map(|call| call.id.clone()));
                }
                EntryBody::ToolResult { tool_call_id, .. } => {
                    if let Some(index) = open.iter().position(|id| id == tool_call_id) {
                        open.remove(index);
                    }
                }
                EntryBody::User { .. } | EntryBody::Summary { .. } => {}
            }
        }
        for tool_call_id in &open {
            self.append(EntryBody::ToolResult {
                tool_call_id: tool_call_id.clone(),
                content: note.to_string(),
                is_error: true,
            })?;
        }
        Ok(open.len())
    }

    /// 已经进过会话的房间消息里最大的 `seq`；一条都没有时是 0。
    /// 进过会话的房间消息的序号，从小到大。
    pub fn message_seqs(&self) -> BTreeSet<i64> {
        self.entries
            .iter()
            .filter_map(|entry| match &entry.body {
                EntryBody::User { message_seq, .. } => *message_seq,
                _ => None,
            })
            .collect()
    }

    pub fn last_message_seq(&self) -> i64 {
        self.entries
            .iter()
            .filter_map(|entry| match &entry.body {
                EntryBody::User { message_seq, .. } => *message_seq,
                _ => None,
            })
            .max()
            .unwrap_or(0)
    }

    /// 最后一个压缩边界的位置。
    fn boundary(&self) -> Option<usize> {
        self.entries
            .iter()
            .rposition(|entry| matches!(entry.body, EntryBody::Summary { .. }))
    }

    /// 仍然原样发给模型的第一条的位置。没压缩过就是 0。
    pub fn live_start(&self) -> usize {
        match self
            .boundary()
            .map(|index| (index, &self.entries[index].body))
        {
            Some((index, EntryBody::Summary { kept, .. })) => index.saturating_sub(*kept),
            _ => 0,
        }
    }

    /// 最近一次压缩写下的摘要。
    pub fn last_summary(&self) -> Option<&str> {
        match &self.entries[self.boundary()?].body {
            EntryBody::Summary { content, .. } => Some(content),
            _ => None,
        }
    }

    /// 换成发给模型的消息列表（不含系统提示词）。
    ///
    /// 有压缩边界时，从最后一个边界开始：先是摘要，再是边界之前保留的那几条，然后是边界之后的。
    pub fn to_chat_messages(&self) -> Vec<ChatMessage> {
        let start = self.live_start();
        let summary = self
            .last_summary()
            .map(|content| ChatMessage::user(format!("{SUMMARY_PREFIX}{content}")));
        let rest = self.entries[start..].iter().filter_map(|entry| {
            Some(match &entry.body {
                EntryBody::Summary { .. } => return None,
                EntryBody::User { content, .. } => ChatMessage::user(content.clone()),
                EntryBody::Assistant {
                    content,
                    reasoning,
                    tool_calls,
                } => ChatMessage {
                    reasoning: reasoning.clone(),
                    tool_calls: tool_calls.clone(),
                    ..ChatMessage::assistant(content.clone())
                },
                EntryBody::ToolResult {
                    tool_call_id,
                    content,
                    ..
                } => ChatMessage::tool_result(tool_call_id.clone(), content.clone()),
            })
        });
        summary.into_iter().chain(rest).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sa_llm::Role;

    fn user(text: &str, seq: i64) -> EntryBody {
        EntryBody::User {
            content: text.into(),
            message_id: Some(format!("msg_{seq}")),
            message_seq: Some(seq),
        }
    }

    fn assistant(text: &str, calls: &[&str]) -> EntryBody {
        EntryBody::Assistant {
            content: text.into(),
            reasoning: None,
            tool_calls: calls
                .iter()
                .map(|id| ToolCall {
                    id: id.to_string(),
                    name: "read".into(),
                    arguments: "{}".into(),
                })
                .collect(),
        }
    }

    #[test]
    fn appends_chain_and_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions/ses_1.jsonl");
        {
            let mut t = Transcript::open(&path).unwrap();
            assert!(t.entries().is_empty());
            assert_eq!(t.last_message_seq(), 0);
            t.append(user("你好", 3)).unwrap();
            t.append(assistant("我看看", &["c1"])).unwrap();
            t.append(EntryBody::ToolResult {
                tool_call_id: "c1".into(),
                content: "内容".into(),
                is_error: false,
            })
            .unwrap();
            t.append(assistant("看完了", &[])).unwrap();
        }
        let t = Transcript::open(&path).unwrap();
        assert_eq!(t.entries().len(), 4);
        assert_eq!(t.entries()[0].parent_uuid, None);
        for pair in t.entries().windows(2) {
            assert_eq!(pair[1].parent_uuid.as_ref(), Some(&pair[0].uuid));
        }
        assert_eq!(t.last_message_seq(), 3);

        let messages = t.to_chat_messages();
        let roles: Vec<_> = messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            [Role::User, Role::Assistant, Role::Tool, Role::Assistant]
        );
        assert_eq!(messages[1].tool_calls[0].id, "c1");
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("c1"));

        // 文件是一行一条的 JSON，带 type 字段。
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 4);
        let first: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(first["type"], "user");
        assert_eq!(first["message_seq"], 3);
    }

    #[cfg(unix)]
    #[test]
    fn new_transcripts_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions/s.jsonl");
        Transcript::open(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn half_written_last_line_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        {
            let mut t = Transcript::open(&path).unwrap();
            t.append(user("第一条", 1)).unwrap();
        }
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"uuid\":\"x\",\"parent_uu").unwrap();
        drop(file);

        let mut t = Transcript::open(&path).unwrap();
        assert_eq!(t.entries().len(), 1);
        t.append(user("第二条", 2)).unwrap();
        drop(t);
        let t = Transcript::open(&path).unwrap();
        assert_eq!(t.entries().len(), 2);
        assert_eq!(t.last_message_seq(), 2);
    }

    #[test]
    fn corrupt_middle_line_is_reported_with_its_number() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        {
            let mut t = Transcript::open(&path).unwrap();
            t.append(user("第一条", 1)).unwrap();
        }
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("这不是 JSON\n");
        std::fs::write(&path, text).unwrap();
        match Transcript::open(&path) {
            Err(TranscriptError::Corrupt { line, .. }) => assert_eq!(line, 2),
            other => panic!("应该报损坏，实际是 {:?}", other.map(|t| t.entries().len())),
        }
    }

    #[test]
    fn unanswered_tool_calls_get_a_synthetic_result_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        {
            let mut t = Transcript::open(&path).unwrap();
            t.append(user("读两个文件", 1)).unwrap();
            t.append(assistant("", &["c1", "c2"])).unwrap();
            t.append(EntryBody::ToolResult {
                tool_call_id: "c1".into(),
                content: "好".into(),
                is_error: false,
            })
            .unwrap();
            // 进程在 c2 返回之前退出。
        }
        let t = Transcript::open(&path).unwrap();
        assert_eq!(t.entries().len(), 4);
        match &t.entries()[3].body {
            EntryBody::ToolResult {
                tool_call_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_call_id, "c2");
                assert_eq!(content, LOST_RESULT);
                assert!(is_error);
            }
            other => panic!("{other:?}"),
        }
        // 再开一次不会重复补。
        drop(t);
        assert_eq!(Transcript::open(&path).unwrap().entries().len(), 4);
    }

    #[test]
    fn a_reused_call_id_from_an_earlier_turn_does_not_hide_an_open_call() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        {
            let mut t = Transcript::open(&path).unwrap();
            t.append(user("第一轮", 1)).unwrap();
            t.append(assistant("", &["call_0"])).unwrap();
            t.append(EntryBody::ToolResult {
                tool_call_id: "call_0".into(),
                content: "好".into(),
                is_error: false,
            })
            .unwrap();
            t.append(assistant("读完了", &[])).unwrap();
            t.append(user("第二轮", 2)).unwrap();
            // 第二轮又用了 call_0，进程在它返回之前退出。
            t.append(assistant("", &["call_0"])).unwrap();
        }
        let t = Transcript::open(&path).unwrap();
        assert_eq!(t.entries().len(), 7);
        assert!(matches!(
            &t.entries()[6].body,
            EntryBody::ToolResult { tool_call_id, is_error: true, .. } if tool_call_id == "call_0"
        ));
        // 发给模型的列表里，每个工具调用后面都跟着结果。
        let roles: Vec<_> = t.to_chat_messages().iter().map(|m| m.role).collect();
        assert_eq!(roles.last(), Some(&Role::Tool));
    }

    #[test]
    fn a_failed_append_leaves_no_fragment_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut t = Transcript::open(&path).unwrap();
        t.append(user("第一条", 1)).unwrap();

        t.fail_next_write_after = Some(20);
        assert!(t.append(user("写到一半磁盘满了", 2)).is_err());
        assert_eq!(t.entries().len(), 1, "没写成的不算数");

        // 同一个对象接着写（房间任务会一直复用它），文件必须还是完好的。
        t.append(user("第三条", 3)).unwrap();
        drop(t);
        let t = Transcript::open(&path).unwrap();
        assert_eq!(t.entries().len(), 2);
        assert_eq!(t.last_message_seq(), 3);
        assert_eq!(
            t.entries()[1].parent_uuid.as_ref(),
            Some(&t.entries()[0].uuid)
        );
    }
}
