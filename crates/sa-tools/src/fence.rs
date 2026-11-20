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
    /// 根是从禁区里划出来的一块。
    carved: bool,
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
        Ok(Self {
            root,
            denied,
            carved: false,
        })
    }

    /// 同一套禁区、换一个根的围栏。`root` 可以在禁区里面：它自己能进，禁区里别的地方照样进不去。
    ///
    /// 任务的独立目录就放在数据目录里：任务能随便写自己的目录，数据目录的其余部分
    /// （令牌、数据库、别的任务）和原来一样，连问都不问就拒绝。
    pub fn within(&self, root: &Path) -> std::io::Result<Self> {
        let mut fence = Self::new(root, &[])?;
        fence.denied = self.denied.clone();
        fence.carved = true;
        Ok(fence)
    }

    /// `real` 是不是在禁区里。
    fn denies(&self, real: &Path) -> bool {
        // 根在禁区里面时（见 [`Fence::within`]），根下面的不算。
        let exempt = self.carved && real.starts_with(&self.root);
        !exempt && self.denied.iter().any(|d| real.starts_with(d))
    }

    /// 给人看的根目录。Windows 上去掉规范化路径带的 `\\?\` 前缀。
    pub fn root_display(&self) -> String {
        let text = self.root.display().to_string();
        match text.strip_prefix(r"\\?\") {
            Some(rest) if !rest.starts_with("UNC\\") => rest.to_string(),
            _ => text,
        }
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

    /// 把一个要写入的路径变成真实路径。文件可以还不存在。
    ///
    /// 返回真实路径，以及它是否在工作目录里面。SuperAgent 自己的数据目录任何时候都不能写。
    /// 做法：和操作系统一样一段一段往下走，已经存在的每一段都解析掉符号链接，
    /// `..` 回到的是解析之后的上一级（`链接/..` 是链接指向的目录的上一级，不是链接所在的目录），
    /// 这样写到的和 [`Fence::resolve`] 读到的是同一个地方。还不存在的那几段原样接在后面。
    pub fn resolve_for_write(&self, input: &str) -> Result<(PathBuf, bool), ToolError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(ToolError::new("没有给出要写入的路径。"));
        }
        let joined = self.root.join(input);
        // 工作目录本身已经是真实路径，从它下面开始走；绝对路径从头走。
        let (mut real, rest) = match joined.strip_prefix(&self.root) {
            Ok(rest) => (self.root.clone(), rest),
            Err(_) => (PathBuf::new(), joined.as_path()),
        };
        for component in rest.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    // 和操作系统一样：文件后面不能跟 `..`。只对围栏里面的路径这么说：
                    // 外面的和数据目录里的，有没有、是不是文件都不透露，照旧算出路径交给上层去问人或拒绝。
                    if self.allows(&real) && real.exists() && !real.is_dir() {
                        return Err(ToolError(format!(
                            "路径 {input} 没法解析：`..` 前面的不是目录。"
                        )));
                    }
                    real.pop();
                }
                Component::Normal(name) => {
                    let next = real.join(name);
                    // 每一段都试着解析，不管前面有没有遇到过不存在的：
                    // `不存在/../链接` 退回来之后，后面的链接照样要看穿。
                    // 解析不了（还不存在）就原样接上。
                    real = next.canonicalize().unwrap_or(next);
                }
                other => real.push(other),
            }
        }
        if real.file_name().is_none() {
            return Err(ToolError(format!("路径 {input} 没法解析。")));
        }
        if self.denies(&real) {
            return Err(ToolError(format!(
                "路径 {input} 在 SuperAgent 自己的数据目录里，不能写。"
            )));
        }
        let inside = real.starts_with(&self.root);
        Ok((real, inside))
    }

    /// 真实路径 `real` 是否在围栏里。
    pub fn allows(&self, real: &Path) -> bool {
        real.starts_with(&self.root) && !self.denies(real)
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

    /// 从禁区里划出来的一块：自己能读能写，禁区里别的地方照样不让碰。
    #[test]
    fn a_fence_carved_out_of_a_denied_directory_keeps_the_rest_off_limits() {
        let outer = tempfile::tempdir().unwrap();
        let (work, data) = (outer.path().join("work"), outer.path().join("data"));
        let task = data.join("tasks/t1");
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&task).unwrap();
        fs::write(task.join("a.txt"), "x").unwrap();
        fs::write(data.join("token"), "secret").unwrap();
        let room = Fence::new(&work, std::slice::from_ref(&data)).unwrap();
        let fence = room.within(&task).unwrap();

        assert!(fence.resolve("a.txt").is_ok());
        let (_, inside) = fence.resolve_for_write("new/b.txt").unwrap();
        assert!(inside);
        // 数据目录里别的东西：读不到，写也不是「问一下」，是直接拒绝。
        assert!(fence.resolve("../../token").is_err());
        for target in ["../../token", "../t2/x.txt", "../../state.db"] {
            let error = fence.resolve_for_write(target).unwrap_err();
            assert!(error.0.contains("数据目录"), "{target}: {error}");
        }
        // 数据目录之外的地方和平常一样：算出路径，由上层去问人。
        let (_, inside) = fence.resolve_for_write("../../../work/x.txt").unwrap();
        assert!(!inside);
        // 原来的围栏不受影响：它的根不在禁区里，禁区里的一律不行。
        assert!(room
            .resolve_for_write(task.join("a.txt").to_str().unwrap())
            .is_err());
    }

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
        // 写入目标可以还不存在；在外面的会被标出来，由上层决定要不要问人。
        let (new_file, inside) = fence.resolve_for_write("新目录/子目录/a.txt").unwrap();
        assert!(inside && new_file.starts_with(fence.root()), "{new_file:?}");
        assert!(new_file.ends_with("新目录/子目录/a.txt"));
        let (escaped, inside) = fence.resolve_for_write("sub/../../外面.txt").unwrap();
        assert!(!inside && !escaped.starts_with(fence.root()), "{escaped:?}");
        assert!(fence.resolve_for_write("  ").is_err());
        // 工作目录的上一级：在外面，要问人。
        let (parent, inside) = fence.resolve_for_write("..").unwrap();
        assert!(!inside);
        assert_eq!(Some(parent.as_path()), fence.root().parent());
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
        // 写也一样；并且不透露里面有什么：文件存在和不存在，回答相同。
        for path in [".superagent/token/../x.txt", ".superagent/没有/../x.txt"] {
            let error = fence.resolve_for_write(path).unwrap_err();
            assert!(error.0.contains("数据目录"), "{path}: {error}");
        }
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
        assert!(fence.resolve_for_write("/").is_err());
    }

    /// `链接/..` 是链接指向的目录的上一级。写和读要落在同一个地方。
    #[cfg(unix)]
    #[test]
    fn parent_of_a_link_is_the_parent_of_its_target_for_writes_too() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("work");
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::create_dir_all(outer.path().join("elsewhere/deep")).unwrap();
        fs::write(outer.path().join("elsewhere/x.txt"), "外面的").unwrap();
        fs::write(root.join("x.txt"), "里面的").unwrap();
        fs::write(root.join("docs/y.txt"), "y").unwrap();
        std::os::unix::fs::symlink(outer.path().join("elsewhere/deep"), root.join("out")).unwrap();
        std::os::unix::fs::symlink(root.join("docs"), root.join("in")).unwrap();
        let fence = Fence::new(&root, &[]).unwrap();
        let outer_real = outer.path().canonicalize().unwrap();

        // 操作系统会把 out/../x.txt 解析成 elsewhere/x.txt：在外面。
        let (real, inside) = fence.resolve_for_write("out/../x.txt").unwrap();
        assert_eq!(real, outer_real.join("elsewhere/x.txt"));
        assert!(!inside);
        assert!(fence.resolve("out/../x.txt").is_err());
        // 还不存在的文件也一样。
        let (real, inside) = fence.resolve_for_write("out/../新的/a.txt").unwrap();
        assert_eq!(real, outer_real.join("elsewhere/新的/a.txt"));
        assert!(!inside);
        // 指向里面的链接：读和写是同一个文件。
        let (real, inside) = fence.resolve_for_write("in/../x.txt").unwrap();
        assert_eq!(real, fence.resolve("in/../x.txt").unwrap());
        assert!(inside);
        let (real, inside) = fence.resolve_for_write("in/y.txt").unwrap();
        assert_eq!(real, fence.root().join("docs/y.txt"));
        assert!(inside);
        // 先经过一段不存在的路径再退回来，后面的链接照样看穿。
        let (real, inside) = fence.resolve_for_write("没有/../out/a.txt").unwrap();
        assert_eq!(real, outer_real.join("elsewhere/deep/a.txt"));
        assert!(!inside);
        let (real, inside) = fence
            .resolve_for_write("没有/子/../../out/../a.txt")
            .unwrap();
        assert_eq!(real, outer_real.join("elsewhere/a.txt"));
        assert!(!inside);
        // 文件后面跟 `..`：操作系统不认，这里也不认（不管是文件本身还是指向文件的链接）。
        std::os::unix::fs::symlink(root.join("x.txt"), root.join("file-link")).unwrap();
        for bad in ["x.txt/../y.txt", "file-link/../y.txt", "docs/y.txt/.."] {
            let error = fence.resolve_for_write(bad).unwrap_err();
            assert!(error.0.contains("不是目录"), "{bad}: {error}");
        }
        // 围栏外面的文件：不说它存不存在、是不是目录，结果和它不存在时一样。
        let (there, inside) = fence
            .resolve_for_write("../elsewhere/x.txt/../y.txt")
            .unwrap();
        let (absent, _) = fence
            .resolve_for_write("../elsewhere/没有/../y.txt")
            .unwrap();
        assert_eq!(there, absent);
        assert!(!inside);
        // 悬空的链接本身可以被换成普通文件；穿过它写到外面要问。
        std::os::unix::fs::symlink(outer.path().join("nowhere"), root.join("dangling")).unwrap();
        let (real, inside) = fence.resolve_for_write("dangling").unwrap();
        assert_eq!(real, fence.root().join("dangling"));
        assert!(inside);
    }
}
