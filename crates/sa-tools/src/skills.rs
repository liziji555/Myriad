//! 技能：写在文件里的做事套路。
//!
//! 一个技能是技能目录里的 `<名字>/SKILL.md`（旁边可以带别的文件），或者单独一个 `<名字>.md`。
//! 文件开头两行 `---` 之间写名字、一句话说明、给哪些 Bot 用：
//!
//! ```text
//! ---
//! name: 发版
//! description: 发新版本之前要做的检查
//! bots: Coder, 助手
//! ---
//! 正文……
//! ```
//!
//! 提示词里只放「名字 + 一句话说明」；Bot 决定要用了，再用 `skill` 工具读全文。
//! 每次都从磁盘上重新读：改了文件不用重启。

use crate::{blocking, parse_input, Tool, ToolCtx, ToolError, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};

/// 最多认这么多个技能。
pub const MAX_SKILLS: usize = 50;
/// 技能里的一个文件最多这么大。
const MAX_FILE_BYTES: u64 = 64 * 1024;
/// 提示词里一句话说明最多这么多字。
const MAX_DESCRIPTION_CHARS: usize = 200;
const MAX_NAME_CHARS: usize = 40;
/// 一个技能最多列出这么多个附带的文件。
const MAX_EXTRA_FILES: usize = 50;
const MAIN_FILE: &str = "SKILL.md";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// 给哪些 Bot 用（Bot 的名字）。空着就是谁都能用。
    pub bots: Vec<String>,
    /// 正文在哪个文件里。
    file: PathBuf,
    /// 技能自己的目录。单独一个 `.md` 的技能没有。
    dir: Option<PathBuf>,
}

impl Skill {
    pub fn is_for(&self, bot_name: &str) -> bool {
        self.bots.is_empty() || self.bots.iter().any(|name| name == bot_name)
    }
}

/// 技能目录里现在有的技能，按名字排。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillSet {
    pub skills: Vec<Skill>,
    /// 没认出来的文件和原因，给人看。
    pub problems: Vec<String>,
}

impl SkillSet {
    /// 读技能目录。目录不存在就是没有技能。
    pub fn load(dir: &Path) -> Self {
        let mut set = Self::default();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return set;
        };
        let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        paths.sort();
        for path in paths {
            let stem = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            let (file, skill_dir) = if path.is_dir() {
                let main = path.join(MAIN_FILE);
                if !main.is_file() {
                    continue;
                }
                (main, Some(path.clone()))
            } else if path.extension().is_some_and(|ext| ext == "md") {
                (path.clone(), None)
            } else {
                continue;
            };
            let shown = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            match read_skill(&file, skill_dir, &stem) {
                Ok(skill) if set.skills.iter().any(|known| known.name == skill.name) => set
                    .problems
                    .push(format!("{shown}：已经有一个叫「{}」的技能了", skill.name)),
                Ok(_) if set.skills.len() >= MAX_SKILLS => {
                    set.problems
                        .push(format!("{shown}：技能太多了，最多 {MAX_SKILLS} 个"));
                }
                Ok(skill) => set.skills.push(skill),
                Err(problem) => set.problems.push(format!("{shown}：{problem}")),
            }
        }
        set.skills.sort_by(|a, b| a.name.cmp(&b.name));
        set
    }

    /// 放进 `bot_name` 的提示词里的那一段。它没有能用的技能时是空的。
    pub fn prompt_block(&self, bot_name: &str) -> String {
        let mine: Vec<&Skill> = self
            .skills
            .iter()
            .filter(|skill| skill.is_for(bot_name))
            .collect();
        if mine.is_empty() {
            return String::new();
        }
        let mut block = String::from(
            "你有下面这些技能（事先写好的做事套路）。碰到对得上的事，先用 skill 工具读它的全文，再照着做；\
             对不上就不用管：\n",
        );
        for skill in mine {
            block.push_str(&format!("- {}：{}\n", skill.name, skill.description));
        }
        block.trim_end().to_string()
    }
}

fn read_skill(file: &Path, dir: Option<PathBuf>, fallback_name: &str) -> Result<Skill, String> {
    let text = read_text(file)?;
    let (meta, body) = split_frontmatter(&text);
    let field = |key: &str| {
        meta.iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    };
    let name = field("name").unwrap_or(fallback_name).trim().to_string();
    let bad = |c: char| c.is_whitespace() || c.is_control() || matches!(c, '/' | '\\');
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS || name.chars().any(bad) {
        return Err(format!(
            "名字「{name}」不行：不能空着、不能有空格和斜杠、最多 {MAX_NAME_CHARS} 个字"
        ));
    }
    // 没写说明的，拿正文第一行（去掉标题的 #）顶上。
    let first_line = body
        .lines()
        .map(|line| line.trim().trim_start_matches('#').trim())
        .find(|line| !line.is_empty());
    let Some(description) = field("description").or(first_line) else {
        return Err("没有写 description，正文也是空的".into());
    };
    let bots = field("bots")
        .map(|value| {
            value
                .trim_matches(|c| c == '[' || c == ']')
                .split([',', '，', '、'])
                .map(|name| name.trim().trim_matches(['"', '\'']).to_string())
                .filter(|name| !name.is_empty())
                .collect()
        })
        .unwrap_or_default();
    Ok(Skill {
        name,
        description: description.chars().take(MAX_DESCRIPTION_CHARS).collect(),
        bots,
        file: file.to_path_buf(),
        dir,
    })
}

/// 把开头两行 `---` 之间的 `键: 值` 和后面的正文分开。没有这一段的，整个都是正文。
fn split_frontmatter(text: &str) -> (Vec<(String, String)>, &str) {
    let text = text.trim_start_matches('\u{feff}');
    let Some(rest) = text.strip_prefix("---").and_then(|rest| {
        rest.strip_prefix("\r\n")
            .or_else(|| rest.strip_prefix('\n'))
    }) else {
        return (vec![], text);
    };
    let mut meta = vec![];
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        offset += line.len();
        let line = line.trim();
        if line == "---" {
            return (meta, &rest[offset..]);
        }
        if let Some((key, value)) = line.split_once([':', '：']) {
            let value = value.trim().trim_matches(['"', '\'']).trim();
            meta.push((key.trim().to_lowercase(), value.to_string()));
        }
    }
    // 没有收尾的 `---`：不当它是开头的说明。
    (vec![], text)
}

fn read_text(file: &Path) -> Result<String, String> {
    let size = std::fs::metadata(file)
        .map_err(|e| format!("读不了：{e}"))?
        .len();
    if size > MAX_FILE_BYTES {
        return Err(format!(
            "太大了（{} KB），最多 {} KB",
            size / 1024,
            MAX_FILE_BYTES / 1024
        ));
    }
    let bytes = std::fs::read(file).map_err(|e| format!("读不了：{e}"))?;
    String::from_utf8(bytes).map_err(|_| "不是 UTF-8 的文本文件".to_string())
}

/// 技能目录里除了 `SKILL.md` 之外的文件，相对路径，按名字排。
fn extra_files(dir: &Path) -> Vec<String> {
    let mut found = vec![];
    let mut pending = vec![(dir.to_path_buf(), 0)];
    while let Some((current, depth)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // 不跟着链接走：技能只管自己目录里的东西。
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() && depth < 3 {
                pending.push((path, depth + 1));
            } else if kind.is_file() {
                let Ok(relative) = path.strip_prefix(dir) else {
                    continue;
                };
                let relative = relative.to_string_lossy().replace('\\', "/");
                if relative != MAIN_FILE {
                    found.push(relative);
                }
            }
        }
    }
    found.sort();
    found.truncate(MAX_EXTRA_FILES);
    found
}

/// `skill` 工具：读一个技能的全文，或者技能带着的某个文件。
pub struct SkillTool {
    /// 技能目录。
    pub dir: PathBuf,
    /// 用这个工具的 Bot 的名字：只能读给它用的技能。
    pub bot_name: String,
}

#[derive(Deserialize)]
struct Input {
    name: String,
    file: Option<String>,
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> String {
        "读一个技能的全文。系统提示词里列着你有哪些技能，每个只有一句话说明；碰到对得上的事，\
         先用这个工具把它的全文读出来，再照着做。技能还带着别的文件时，结果里会列出来，\
         要看哪个就再调用一次并写上 file。"
            .into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "技能的名字，和提示词里列的一样" },
                "file": { "type": "string", "description": "可选。技能带着的某个文件的相对路径；不写就读技能的全文" }
            },
            "required": ["name"]
        })
    }

    fn title(&self, input: &Value) -> String {
        let name = input["name"].as_str().unwrap_or("?");
        match input["file"].as_str() {
            Some(file) => format!("读技能「{name}」里的 {file}"),
            None => format!("读技能「{name}」"),
        }
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    fn max_result_chars(&self) -> usize {
        30_000
    }

    async fn call(&self, input: Value, _ctx: &ToolCtx) -> ToolResult {
        let input: Input = parse_input(input)?;
        let (dir, bot_name) = (self.dir.clone(), self.bot_name.clone());
        blocking(move || {
            let set = SkillSet::load(&dir);
            let wanted = input.name.trim();
            let Some(skill) = set
                .skills
                .iter()
                .find(|skill| skill.name == wanted && skill.is_for(&bot_name))
            else {
                let names: Vec<&str> = set
                    .skills
                    .iter()
                    .filter(|skill| skill.is_for(&bot_name))
                    .map(|skill| skill.name.as_str())
                    .collect();
                return Err(ToolError(if names.is_empty() {
                    format!("没有叫「{wanted}」的技能，你现在没有能用的技能。")
                } else {
                    format!("没有叫「{wanted}」的技能。你能用的有：{}", names.join("、"))
                }));
            };
            match input.file.as_deref().map(str::trim) {
                Some(file) if !file.is_empty() => read_extra(skill, file),
                _ => {
                    let text = read_text(&skill.file).map_err(ToolError)?;
                    let (_, body) = split_frontmatter(&text);
                    let mut out = body.trim().to_string();
                    let extras = skill.dir.as_deref().map(extra_files).unwrap_or_default();
                    if !extras.is_empty() {
                        out.push_str(&format!(
                            "\n\n（这个技能还带着这些文件，要看哪个就再调用一次 skill 并写上 file：{}）",
                            extras.join("、")
                        ));
                    }
                    Ok(out)
                }
            }
        })
        .await
    }
}

/// 读技能目录里的一个文件。只能读这个技能自己目录里面的。
fn read_extra(skill: &Skill, file: &str) -> ToolResult {
    let Some(dir) = &skill.dir else {
        return Err(ToolError(format!(
            "技能「{}」只有一个文件，没有带别的。",
            skill.name
        )));
    };
    let relative = Path::new(file);
    let plain = relative
        .components()
        .all(|part| matches!(part, Component::Normal(_)));
    if !plain {
        return Err(ToolError::new(
            "file 要写技能目录里面的相对路径，不能有 .. 也不能是绝对路径。",
        ));
    }
    let missing = || {
        let extras = extra_files(dir);
        ToolError(if extras.is_empty() {
            format!("技能「{}」没有带别的文件。", skill.name)
        } else {
            format!(
                "技能「{}」里没有 {file}。它带着的文件有：{}",
                skill.name,
                extras.join("、")
            )
        })
    };
    // 链接可能指到目录外面去：按真实路径再比一次。
    let root = dir.canonicalize().map_err(|_| missing())?;
    let real = dir.join(relative).canonicalize().map_err(|_| missing())?;
    if !real.starts_with(&root) || !real.is_file() {
        return Err(missing());
    }
    read_text(&real).map_err(|problem| ToolError(format!("{file} {problem}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Fence;

    fn write(dir: &Path, path: &str, text: &str) {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn library() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "release/SKILL.md",
            "---\nname: 发版\ndescription: \"发新版本之前要做的检查\"\nbots: [Coder, 助手]\n---\n# 发版\n\n1. 跑测试\n2. 改版本号\n",
        );
        write(dir.path(), "release/checklist.md", "- [ ] 更新日志\n");
        write(dir.path(), "release/scripts/bump.sh", "echo bump\n");
        write(
            dir.path(),
            "review.md",
            "# 代码评审\n\n先看测试，再看实现。\n",
        );
        dir
    }

    async fn call(tool: &SkillTool, input: Value) -> ToolResult {
        let ctx = ToolCtx::new(Fence::new(&tool.dir, &[]).unwrap());
        tool.call(input, &ctx).await
    }

    #[test]
    fn skills_are_found_in_folders_and_single_files() {
        let dir = library();
        let set = SkillSet::load(dir.path());
        assert!(set.problems.is_empty(), "{:?}", set.problems);
        let shown: Vec<_> = set
            .skills
            .iter()
            .map(|skill| {
                (
                    skill.name.as_str(),
                    skill.description.as_str(),
                    skill.bots.len(),
                )
            })
            .collect();
        // 没写名字的用文件名，没写说明的用正文第一行。
        assert_eq!(
            shown,
            [
                ("review", "代码评审", 0),
                ("发版", "发新版本之前要做的检查", 2)
            ]
        );
        // 没有技能目录就是没有技能。
        assert_eq!(
            SkillSet::load(&dir.path().join("没有")),
            SkillSet::default()
        );
    }

    #[test]
    fn the_prompt_lists_only_what_this_bot_may_use() {
        let dir = library();
        let set = SkillSet::load(dir.path());
        let coder = set.prompt_block("Coder");
        assert!(coder.contains("- 发版：发新版本之前要做的检查"), "{coder}");
        assert!(coder.contains("- review：代码评审"), "{coder}");
        assert!(coder.contains("skill 工具"), "{coder}");
        let other = set.prompt_block("Reviewer");
        assert!(
            other.contains("review") && !other.contains("发版"),
            "{other}"
        );
        assert_eq!(SkillSet::default().prompt_block("Coder"), "");
    }

    #[test]
    fn files_that_cannot_be_skills_are_reported_not_loaded() {
        let dir = library();
        write(
            dir.path(),
            "zz-dup.md",
            "---\nname: 发版\ndescription: 重名\n---\n正文",
        );
        write(dir.path(), "bad name.md", "正文");
        write(dir.path(), "empty.md", "");
        write(dir.path(), "notes.txt", "不是技能");
        write(dir.path(), "folder-without-main/readme.md", "不是技能");
        write(dir.path(), "huge.md", &"字".repeat(40_000));
        write(dir.path(), "open.md", "---\nname: 没收尾\n正文接着写");
        let set = SkillSet::load(dir.path());
        let names: Vec<_> = set.skills.iter().map(|skill| skill.name.as_str()).collect();
        // 开头的 `---` 没有收尾的，整个当正文：名字用文件名。
        assert_eq!(names, ["open", "review", "发版"]);
        let problems = set.problems.join("\n");
        assert_eq!(set.problems.len(), 4, "{problems}");
        assert!(
            problems.contains("zz-dup.md：已经有一个叫「发版」的技能了"),
            "{problems}"
        );
        assert!(problems.contains("bad name.md：名字"), "{problems}");
        assert!(
            problems.contains("empty.md：没有写 description"),
            "{problems}"
        );
        assert!(problems.contains("huge.md：太大了"), "{problems}");
    }

    #[tokio::test]
    async fn the_tool_reads_a_skill_and_the_files_it_brings() {
        let dir = library();
        let tool = SkillTool {
            dir: dir.path().to_path_buf(),
            bot_name: "Coder".into(),
        };
        let text = call(&tool, json!({ "name": "发版" })).await.unwrap();
        assert!(text.starts_with("# 发版\n\n1. 跑测试"), "{text}");
        assert!(!text.contains("description:"), "{text}");
        assert!(text.contains("checklist.md、scripts/bump.sh"), "{text}");
        let extra = call(&tool, json!({ "name": "发版", "file": "scripts/bump.sh" })).await;
        assert_eq!(extra.unwrap(), "echo bump\n");
        // 单独一个文件的技能没有附带的东西。
        let single = call(&tool, json!({ "name": "review" })).await.unwrap();
        assert_eq!(single, "# 代码评审\n\n先看测试，再看实现。");

        let missing = call(&tool, json!({ "name": "部署" })).await.unwrap_err();
        assert!(missing.0.contains("你能用的有：review、发版"), "{missing}");
        // 不是给这个 Bot 用的技能，读不到。
        let outsider = SkillTool {
            dir: dir.path().to_path_buf(),
            bot_name: "Reviewer".into(),
        };
        let refused = call(&outsider, json!({ "name": "发版" }))
            .await
            .unwrap_err();
        assert!(refused.0.contains("你能用的有：review"), "{refused}");
    }

    #[tokio::test]
    async fn a_skill_only_gives_out_files_inside_its_own_folder() {
        let dir = library();
        write(dir.path(), "secret.txt", "别的东西");
        let tool = SkillTool {
            dir: dir.path().to_path_buf(),
            bot_name: "Coder".into(),
        };
        let outside = dir.path().join("secret.txt").display().to_string();
        for file in [
            "../secret.txt",
            "scripts/../../secret.txt",
            outside.as_str(),
        ] {
            let error = call(&tool, json!({ "name": "发版", "file": file }))
                .await
                .unwrap_err();
            assert!(error.0.contains("相对路径"), "{file}: {error}");
        }
        let error = call(&tool, json!({ "name": "发版", "file": "nope.md" }))
            .await
            .unwrap_err();
        assert!(error.0.contains("它带着的文件有：checklist.md"), "{error}");
        let error = call(&tool, json!({ "name": "review", "file": "x.md" }))
            .await
            .unwrap_err();
        assert!(error.0.contains("只有一个文件"), "{error}");

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                dir.path().join("secret.txt"),
                dir.path().join("release/link.txt"),
            )
            .unwrap();
            let error = call(&tool, json!({ "name": "发版", "file": "link.txt" }))
                .await
                .unwrap_err();
            assert!(error.0.contains("里没有 link.txt"), "{error}");
        }
    }
}
