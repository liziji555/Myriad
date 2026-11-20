//! 路径围栏：文件类工具只能访问工作目录里面的东西，并且碰不到 SuperAgent 自己的数据目录
//! （那里有访问令牌和数据库）。

use crate::ToolError;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Fence {
    /// 工作目录，已解析成真实路径。
    root: PathBuf,
    /// 工作目录里也不许碰的目录，已解析成真实路径。
    denied: Vec<PathBuf>,
}

impl Fence {
    /// `root` 必须是已存在的目录。`denied` 里不存在的路径会被忽略。
    pub fn new(root: &Path, denied: &[PathBuf]) -> std::io::Result<Self> {
        let root = root.canonicalize()?;
        if !root.is_dir() {
            return Err(std::io::Error::other(format!(
                "{} 不是目录",
                root.display()
            )));
        }
        let denied = denied
            .iter()
            .filter_map(|path| path.canonicalize().ok())
            .collect();
        Ok(Self { root, denied })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 把模型给的路径变成真实路径。相对路径从工作目录算起。
    ///
    /// 先解析符号链接再判断，所以指向外面的链接进不来。路径必须已经存在。
    pub fn resolve(&self, input: &str) -> Result<PathBuf, ToolError> {
        let input = input.trim();
        let joined = if input.is_empty() {
            self.root.clone()
        } else {
            self.root.join(input)
        };
        let outside = || {
            ToolError(format!(
                "路径 {input} 在工作目录之外，不能访问。只能看工作目录里面的文件。"
            ))
        };
        // 先按字面判断：明显指向外面的路径直接拒绝，不透露它存不存在。
        if !lexically_normalized(&joined).starts_with(&self.root) {
            return Err(outside());
        }
        let real = joined.canonicalize().map_err(|e| {
            let reason = match e.kind() {
                std::io::ErrorKind::NotFound => "不存在".to_string(),
                _ => format!("打不开（{e}）"),
            };
            ToolError(format!(
                "路径 {input} {reason}。路径从工作目录算起，可以先用 list_dir 或 glob 找一下。"
            ))
        })?;
        if !self.allows(&real) {
            return Err(outside());
        }
        Ok(real)
    }

    /// 真实路径 `real` 是否在围栏里。
    pub fn allows(&self, real: &Path) -> bool {
        real.starts_with(&self.root) && !self.denied.iter().any(|d| real.starts_with(d))
    }

    /// 给模型和人看的写法：相对工作目录、用 `/` 分隔；工作目录本身是 `.`。
    pub fn display(&self, real: &Path) -> String {
        let relative = real.strip_prefix(&self.root).unwrap_or(real);
        let text = relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        if text.is_empty() {
            ".".into()
        } else {
            text
        }
    }
}

/// 不碰文件系统，只按字面消掉 `.` 和 `..`。
fn lexically_normalized(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn resolves_inside_and_rejects_outside() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("work");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.rs"), "x").unwrap();
        fs::write(outer.path().join("secret.txt"), "x").unwrap();
        let fence = Fence::new(&root, &[]).unwrap();

        let inside = fence.resolve("src/a.rs").unwrap();
        assert_eq!(fence.display(&inside), "src/a.rs");
        assert_eq!(fence.display(&fence.resolve("").unwrap()), ".");
        assert_eq!(fence.display(&fence.resolve("src/..").unwrap()), ".");
        // 工作目录里的绝对路径也可以。
        assert!(fence.resolve(inside.to_str().unwrap()).is_ok());

        for outside in ["../secret.txt", "src/../../secret.txt", ".."] {
            let error = fence.resolve(outside).unwrap_err();
            assert!(error.0.contains("工作目录之外"), "{outside}: {error}");
        }
        let absolute = outer.path().join("secret.txt");
        assert!(fence.resolve(absolute.to_str().unwrap()).is_err());
        assert!(fence.resolve("nope.txt").unwrap_err().0.contains("不存在"));
        // 外面的路径不管存不存在，回答都一样。
        let ghost = fence.resolve("../没有这个文件").unwrap_err();
        assert!(ghost.0.contains("工作目录之外"), "{ghost}");
    }

    #[test]
    fn denied_directories_are_off_limits_even_inside_root() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join(".superagent");
        fs::create_dir_all(&data).unwrap();
        fs::write(data.join("token"), "secret").unwrap();
        fs::write(root.path().join("ok.txt"), "x").unwrap();
        let fence = Fence::new(root.path(), &[data, root.path().join("missing")]).unwrap();

        assert!(fence.resolve("ok.txt").is_ok());
        assert!(fence.resolve(".superagent/token").is_err());
        assert!(fence.resolve(".superagent").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_pointing_outside_are_rejected() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("work");
        fs::create_dir_all(&root).unwrap();
        fs::write(outer.path().join("secret.txt"), "x").unwrap();
        fs::write(root.join("real.txt"), "x").unwrap();
        std::os::unix::fs::symlink(outer.path().join("secret.txt"), root.join("leak")).unwrap();
        std::os::unix::fs::symlink(root.join("real.txt"), root.join("alias")).unwrap();
        let fence = Fence::new(&root, &[]).unwrap();

        assert!(fence.resolve("leak").is_err());
        assert_eq!(fence.display(&fence.resolve("alias").unwrap()), "real.txt");
    }
}
