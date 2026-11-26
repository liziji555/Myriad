//! 在线检查更新与自更新（服务端 `sa` 二进制和桌面端安装包共用）。
//!
//! 从 GitHub Releases 最新版本拉取元数据与 `SHA256SUMS.txt`，校验 SHA-256 后再替换文件；
//! 校验不通过或缺少校验和时一律拒绝替换。

use anyhow::{bail, Context, Result};
use clap::Args;
use futures_util::{future::BoxFuture, StreamExt};
use sa_protocol::{UpdateApplyResult, UpdateStatus};
use sa_server::UpdateAdmin;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const DEFAULT_REPO: &str = "liziji555/Myriad";
const DEFAULT_API_BASE: &str = "https://api.github.com";
const CHECK_TIMEOUT: Duration = Duration::from_secs(15);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);
/// 单个安装包/二进制最多允许 256 MB，防止异常响应撑爆磁盘。
const MAX_ASSET_BYTES: u64 = 256 * 1024 * 1024;
const MAX_NOTES_CHARS: usize = 600;

#[derive(Args)]
pub struct UpdateArgs {
    /// 只检查有没有新版本，不下载、不替换
    #[arg(long)]
    check: bool,
}

/// 桌面端检查更新时返回给界面的信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopUpdateInfo {
    pub current: String,
    pub latest: Option<String>,
    pub available: bool,
    pub release_url: Option<String>,
    pub asset_name: Option<String>,
    pub can_download: bool,
}

#[derive(Debug, Clone)]
pub struct ReleaseCheck {
    pub current: String,
    pub latest: Option<String>,
    pub available: bool,
    pub release_url: Option<String>,
    pub notes: Option<String>,
    pub asset_name: Option<String>,
    pub asset_url: Option<String>,
    pub asset_sha256: Option<String>,
    pub checked_at: i64,
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

pub fn cli_asset_name() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("sa-linux-x86_64"),
        ("windows", "x86_64") => Some("sa-windows-x86_64.exe"),
        _ => None,
    }
}

pub fn desktop_asset_name() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Some("Myriad-setup-windows-x64.exe"),
        ("linux", "x86_64") => Some("Myriad-linux-amd64.deb"),
        _ => None,
    }
}

fn strip_v(version: &str) -> &str {
    let trimmed = version.trim();
    trimmed
        .strip_prefix('v')
        .or_else(|| trimmed.strip_prefix('V'))
        .unwrap_or(trimmed)
}

fn parse_semver(version: &str) -> Option<(u64, u64, u64, &str)> {
    let clean = strip_v(version);
    let (core, pre) = match clean.split_once('-') {
        Some((core, pre)) => (core, pre),
        None => (clean, ""),
    };
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch, pre))
}

/// 判断 `latest` 是否比 `current` 更新。
///
/// 标准语义化版本按主/次/修订号比；同一版本号带预发布后缀（如 `0.1.0-20261126`）时，
/// 只要字符串不同就视为有新构建，方便日常预览版更新。
pub fn is_newer(current: &str, latest: &str) -> bool {
    let cur = strip_v(current);
    let lat = strip_v(latest);
    if cur.is_empty() || lat.is_empty() || cur == lat {
        return false;
    }
    match (parse_semver(cur), parse_semver(lat)) {
        (Some((cm, cn, cp, cpre)), Some((lm, ln, lp, lpre))) => {
            if (lm, ln, lp) != (cm, cn, cp) {
                (lm, ln, lp) > (cm, cn, cp)
            } else {
                // 同主次修订号：正式版 > 预发布；或不同的日期/构建后缀视为有更新。
                cpre != lpre
            }
        }
        _ => cur != lat,
    }
}

/// 解析标准 `sha256sum` 格式的校验文件：每行 `<64位十六进制>  <文件名>`。
pub fn parse_sha256sums(text: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        let Some(hash) = parts.next() else { continue };
        let Some(rest) = parts.next() else { continue };
        let name = rest.trim().trim_start_matches('*');
        if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) && !name.is_empty() {
            map.insert(name.to_string(), hash.to_ascii_lowercase());
        }
    }
    map
}

fn http_client(timeout: Duration) -> Result<reqwest::Client, String> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .user_agent(concat!("myriad/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(timeout)
        .build()
        .map_err(|error| format!("初始化 HTTP 客户端失败：{error}"))
}

fn update_repo() -> String {
    std::env::var("SA_UPDATE_REPO")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_REPO.to_string())
}

fn api_base() -> String {
    std::env::var("SA_UPDATE_API_BASE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_API_BASE.to_string())
}

fn truncate_notes(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut out: String = trimmed.chars().take(MAX_NOTES_CHARS).collect();
    if trimmed.chars().count() > MAX_NOTES_CHARS {
        out.push('…');
    }
    Some(out)
}

/// 查询 GitHub 最新 Release，并匹配指定平台产物与 SHA-256 校验值。
pub async fn check_for(
    current_version: &str,
    wanted_asset: Option<&str>,
) -> Result<ReleaseCheck, String> {
    let client = http_client(CHECK_TIMEOUT)?;
    let repo = update_repo();
    let base = api_base();
    let url = format!(
        "{}/repos/{repo}/releases/latest",
        base.trim_end_matches('/')
    );
    let response = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|error| format!("连不上 GitHub 检查更新：{error}"))?;
    let checked_at = sa_engine::now_ms();
    let current = strip_v(current_version).to_string();
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(ReleaseCheck {
            current,
            latest: None,
            available: false,
            release_url: Some(format!("https://github.com/{repo}/releases")),
            notes: None,
            asset_name: wanted_asset.map(String::from),
            asset_url: None,
            asset_sha256: None,
            checked_at,
        });
    }
    if !response.status().is_success() {
        return Err(format!("GitHub 返回了 {}", response.status()));
    }
    let release: GhRelease = response
        .json()
        .await
        .map_err(|error| format!("解析 Release 信息失败：{error}"))?;
    let latest = strip_v(&release.tag_name).to_string();
    let available = is_newer(&current, &latest);

    let asset = wanted_asset.and_then(|name| release.assets.iter().find(|a| a.name == name));
    let mut sha256 = None;
    if let (Some(found), Some(sums_asset)) = (
        asset,
        release.assets.iter().find(|a| a.name == "SHA256SUMS.txt"),
    ) {
        if let Ok(resp) = client.get(&sums_asset.browser_download_url).send().await {
            if resp.status().is_success() {
                if let Ok(text) = resp.text().await {
                    let sums = parse_sha256sums(&text);
                    sha256 = sums.get(&found.name).cloned();
                }
            }
        }
    }

    Ok(ReleaseCheck {
        current,
        latest: Some(latest),
        available,
        release_url: Some(release.html_url),
        notes: truncate_notes(&release.body),
        asset_name: wanted_asset.map(String::from),
        asset_url: asset.map(|a| a.browser_download_url.clone()),
        asset_sha256: sha256,
        checked_at,
    })
}

/// 下载文件到 `dest_dir` 下的临时文件并校验 SHA-256；校验通过后返回该临时文件路径。
pub async fn download_verified(
    url: &str,
    expected_sha256: &str,
    dest_dir: &Path,
    prefix: &str,
) -> Result<PathBuf, String> {
    let client = http_client(DOWNLOAD_TIMEOUT)?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| format!("下载更新包失败：{error}"))?;
    if !response.status().is_success() {
        return Err(format!("下载更新包失败：服务器返回 {}", response.status()));
    }
    if let Some(len) = response.content_length() {
        if len > MAX_ASSET_BYTES {
            return Err("更新包体积异常，已中止下载".into());
        }
    }
    let tmp_name = format!(".{prefix}-{}", sa_protocol::RoutineId::new());
    let tmp_path = dest_dir.join(tmp_name);
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&tmp_path)
        .map_err(|error| format!("无法在 {} 创建临时文件：{error}", dest_dir.display()))?;

    let mut hasher = Sha256::new();
    let mut written: u64 = 0;
    let mut stream = response.bytes_stream();
    let result: Result<(), String> = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| format!("下载中断：{error}"))?;
            written = written.saturating_add(chunk.len() as u64);
            if written > MAX_ASSET_BYTES {
                return Err("更新包体积超出限制，已中止".into());
            }
            hasher.update(&chunk);
            file.write_all(&chunk)
                .map_err(|error| format!("写入更新包失败：{error}"))?;
        }
        file.flush()
            .map_err(|error| format!("落盘更新包失败：{error}"))?;
        Ok(())
    }
    .await;

    if let Err(error) = result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(error);
    }
    drop(file);

    let actual = hex::encode(hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!(
            "SHA-256 校验不通过（期望 {}，实际 {}），已拒绝安装",
            &expected_sha256[..expected_sha256.len().min(12)],
            &actual[..actual.len().min(12)]
        ));
    }
    Ok(tmp_path)
}

/// 用下载并校验好的新文件替换当前正在运行的 `sa` 可执行文件。
pub fn replace_current_exe(new_file: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|error| format!("找不到当前可执行文件：{error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(new_file, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| format!("设置可执行权限失败：{error}"))?;
        std::fs::rename(new_file, &exe).map_err(|error| {
            let _ = std::fs::remove_file(new_file);
            format!("替换 {} 失败（检查目录写权限）：{error}", exe.display())
        })?;
        Ok(())
    }
    #[cfg(windows)]
    {
        let backup = exe.with_extension("exe.old");
        let _ = std::fs::remove_file(&backup);
        std::fs::rename(&exe, &backup).map_err(|error| {
            let _ = std::fs::remove_file(new_file);
            format!("备份旧可执行文件失败：{error}")
        })?;
        if let Err(error) = std::fs::rename(new_file, &exe) {
            let _ = std::fs::rename(&backup, &exe);
            let _ = std::fs::remove_file(new_file);
            return Err(format!("替换可执行文件失败：{error}"));
        }
        Ok(())
    }
}

fn exe_dir() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|error| format!("找不到当前可执行文件：{error}"))?;
    exe.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "找不到可执行文件所在目录".into())
}

fn running_under_systemd() -> bool {
    cfg!(target_os = "linux") && std::env::var_os("INVOCATION_ID").is_some()
}

fn trigger_systemd_restart() {
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "restart", "superagent"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

/// 供 `sa serve` 挂到 `/api/update` 的在线更新器。
#[derive(Default)]
pub struct ServerUpdater {
    busy: Arc<tokio::sync::Mutex<()>>,
}

impl ServerUpdater {
    pub fn new() -> Self {
        Self::default()
    }
}

impl UpdateAdmin for ServerUpdater {
    fn check(&self) -> BoxFuture<'static, Result<UpdateStatus, String>> {
        Box::pin(async {
            let found = check_for(env!("CARGO_PKG_VERSION"), cli_asset_name()).await?;
            let can_self_update =
                found.available && found.asset_url.is_some() && found.asset_sha256.is_some();
            Ok(UpdateStatus {
                current: found.current,
                latest: found.latest,
                available: found.available,
                release_url: found.release_url,
                notes: found.notes,
                can_self_update,
                asset_name: found.asset_name,
                checked_at: found.checked_at,
            })
        })
    }

    fn apply(&self) -> BoxFuture<'static, Result<UpdateApplyResult, String>> {
        let busy = self.busy.clone();
        Box::pin(async move {
            let Ok(_guard) = busy.try_lock() else {
                return Err("正在更新中，请稍候".into());
            };
            let found = check_for(env!("CARGO_PKG_VERSION"), cli_asset_name()).await?;
            if !found.available {
                return Err(format!("当前已是最新版本（v{}）", found.current));
            }
            let (Some(url), Some(sha), Some(latest)) =
                (found.asset_url, found.asset_sha256, found.latest)
            else {
                return Err(
                    "最新 Release 里没有当前平台的已校验安装包，请到 GitHub 手动下载".into(),
                );
            };
            let dir = exe_dir()?;
            let tmp = download_verified(&url, &sha, &dir, "sa-update").await?;
            replace_current_exe(&tmp)?;

            let restarting = running_under_systemd();
            if restarting {
                // 延迟 500ms 再发重启，保证这次 HTTP 200 响应先送到前端。
                tokio::spawn(async {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    trigger_systemd_restart();
                });
            }
            let message = if restarting {
                format!("已更新到 v{latest}，服务正在自动重启，几秒后自动重连。")
            } else {
                format!("已更新到 v{latest}，重启 sa serve 后生效。")
            };
            Ok(UpdateApplyResult {
                updated_to: latest,
                restarting,
                message,
            })
        })
    }
}

/// 桌面端检查自己有没有新版（直接在受信任的桌面进程里查 GitHub，不信任远端页面传入的地址）。
pub async fn check_desktop_update(current_version: &str) -> Result<DesktopUpdateInfo, String> {
    let found = check_for(current_version, desktop_asset_name()).await?;
    Ok(DesktopUpdateInfo {
        current: found.current,
        latest: found.latest,
        available: found.available,
        release_url: found.release_url,
        asset_name: found.asset_name,
        can_download: found.available && found.asset_url.is_some() && found.asset_sha256.is_some(),
    })
}

/// 桌面端下载并校验最新安装包，返回 `(版本号, 安装包路径)`。
pub async fn download_desktop_installer(
    current_version: &str,
) -> Result<(String, PathBuf), String> {
    let found = check_for(current_version, desktop_asset_name()).await?;
    if !found.available {
        return Err(format!("桌面版已是最新（v{}）", found.current));
    }
    let (Some(url), Some(sha), Some(latest), Some(asset)) = (
        found.asset_url,
        found.asset_sha256,
        found.latest,
        found.asset_name,
    ) else {
        return Err("最新 Release 里没有当前平台的桌面安装包".into());
    };
    let dir = std::env::temp_dir().join("myriad-updates");
    std::fs::create_dir_all(&dir).map_err(|error| format!("创建临时下载目录失败：{error}"))?;
    let tmp = download_verified(&url, &sha, &dir, "desktop").await?;
    let final_path = dir.join(format!("v{latest}-{asset}"));
    let _ = std::fs::remove_file(&final_path);
    std::fs::rename(&tmp, &final_path).map_err(|error| {
        let _ = std::fs::remove_file(&tmp);
        format!("保存安装包失败：{error}")
    })?;
    Ok((latest, final_path))
}

/// 命令行 `sa update [--check]`。
pub fn run(args: UpdateArgs) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("无法启动异步运行时")?;
    rt.block_on(async {
        let found = check_for(env!("CARGO_PKG_VERSION"), cli_asset_name())
            .await
            .map_err(anyhow::Error::msg)?;
        let Some(latest) = &found.latest else {
            println!(
                "当前版本：v{}（GitHub 上还没有发布 Release）",
                found.current
            );
            return Ok(());
        };
        if !found.available {
            println!("当前已是最新版本：v{}", found.current);
            return Ok(());
        }
        println!("发现新版本：v{} -> v{}", found.current, latest);
        if let Some(url) = &found.release_url {
            println!("发布页：{url}");
        }
        if args.check {
            return Ok(());
        }
        let (Some(url), Some(sha)) = (found.asset_url, found.asset_sha256) else {
            bail!("最新 Release 缺少当前平台的预编译文件或 SHA256SUMS.txt，请手动下载");
        };
        println!("正在下载并校验 SHA-256…");
        let dir = exe_dir().map_err(anyhow::Error::msg)?;
        let tmp = download_verified(&url, &sha, &dir, "sa-update")
            .await
            .map_err(anyhow::Error::msg)?;
        replace_current_exe(&tmp).map_err(anyhow::Error::msg)?;
        println!("已更新到 v{latest}。");
        #[cfg(target_os = "linux")]
        {
            // 如果本机装了 systemd 用户服务，顺手重启它让新版立即生效。
            let status = std::process::Command::new("systemctl")
                .args(["--user", "is-active", "--quiet", "superagent"])
                .status();
            if status.is_ok_and(|s| s.success()) {
                println!("检测到 superagent 用户服务正在运行，正在重启…");
                let restarted = std::process::Command::new("systemctl")
                    .args(["--user", "restart", "superagent"])
                    .status();
                if restarted.is_ok_and(|s| s.success()) {
                    println!("服务已重启到 v{latest}。");
                }
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_comparison_handles_v_prefix_and_prerelease() {
        assert!(is_newer("0.1.0", "v0.1.1"));
        assert!(is_newer("0.1.0", "0.2.0"));
        assert!(is_newer("0.1.9", "1.0.0"));
        assert!(!is_newer("0.1.1", "v0.1.1"));
        assert!(!is_newer("0.2.0", "v0.1.9"));
        assert!(is_newer("0.1.0", "v0.1.0-20261126"));
        assert!(!is_newer("0.1.0-20261126", "v0.1.0-20261126"));
    }

    #[test]
    fn parses_standard_sha256sums() {
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let text = format!(
            "# comment\n{a}  sa-linux-x86_64\n{b} *Myriad-setup-windows-x64.exe\ninvalid line\n"
        );
        let parsed = parse_sha256sums(&text);
        assert_eq!(
            parsed.get("sa-linux-x86_64").map(String::as_str),
            Some(a.as_str())
        );
        assert_eq!(
            parsed
                .get("Myriad-setup-windows-x64.exe")
                .map(String::as_str),
            Some("b".repeat(64).as_str())
        );
        assert_eq!(parsed.len(), 2);
    }
}
