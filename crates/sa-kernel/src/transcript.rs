//! 会话的对话记录。
//!
//! 文件是 JSONL：一行一条，只追加。每条带 `uuid` 和 `parent_uuid`（上一条的 uuid），
//! 以后做压缩时在边界处把 `parent_uuid` 置空，加载时就只取边界之后的部分。

use sa_llm::{ChatMessage, ToolCall};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
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
}

pub struct Transcript {
    path: PathBuf,
    file: File,
    entries: Vec<Entry>,
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
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(io)?;
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
        // 一次 write 写完整行：中途失败留下的半行会在下次打开时被截掉。
        self.file
            .write_all(line.as_bytes())
            .and_then(|()| self.file.sync_data())
            .map_err(|source| TranscriptError::Io {
                path: self.path.clone(),
                source,
            })?;
        self.entries.push(entry);
        Ok(self.entries.last().expect("刚刚放进去"))
    }

    /// 给还没有结果的工具调用补上结果，内容是 `note`。
    ///
    /// 模型接口要求每个工具调用后面都跟着它的结果，缺了整个会话就再也发不出去。
    pub fn close_open_tool_calls(&mut self, note: &str) -> Result<usize, TranscriptError> {
        let answered: HashSet<&str> = self
            .entries
            .iter()
            .filter_map(|entry| match &entry.body {
                EntryBody::ToolResult { tool_call_id, .. } => Some(tool_call_id.as_str()),
                _ => None,
            })
            .collect();
        let open: Vec<String> = self
            .entries
            .iter()
            .filter_map(|entry| match &entry.body {
                EntryBody::Assistant { tool_calls, .. } => Some(tool_calls),
                _ => None,
            })
            .flatten()
            .filter(|call| !answered.contains(call.id.as_str()))
            .map(|call| call.id.clone())
            .collect();
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

    /// 换成发给模型的消息列表（不含系统提示词）。
    pub fn to_chat_messages(&self) -> Vec<ChatMessage> {
        self.entries
            .iter()
            .map(|entry| match &entry.body {
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
            .collect()
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
}
