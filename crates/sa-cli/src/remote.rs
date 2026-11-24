//! 连别的机器上的 Myriad。
//!
//! 服务器上的 `sa serve` 只听服务器本机，不对外开端口；这边用系统的 `ssh` 开一条隧道：
//! 本机的一个端口 → 服务器上的服务。先在服务器上跑 `sa url` 拿到端口和令牌，
//! 再 `ssh -N -L` 把端口接过来。隧道断了（网络抖动、电脑睡眠）自动重连，
//! 本机端口不变，所以页面不用重开，事件流自己会接上。

use crate::config::Config;
use crate::paths::DataDir;
use crate::serve::load_or_create_token;
use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// 隧道开好以后最多等服务应答这么久。
const READY_WAIT: Duration = Duration::from_secs(20);
/// 断了之后隔多久重连，第一次和最多。
pub(crate) const RETRY_FIRST: Duration = Duration::from_secs(2);
pub(crate) const RETRY_MAX: Duration = Duration::from_secs(30);

/// 在服务器上找 `sa` 时额外看的目录：用 SSH 直接执行命令时，很多系统不读登录脚本，
/// `cargo install` 装到的 `~/.cargo/bin` 就不在 PATH 里。
const REMOTE_PATH: &str = "PATH=\"$HOME/.cargo/bin:$HOME/.local/bin:$PATH\"";

#[derive(clap::Args)]
pub struct UrlArgs {}

#[derive(clap::Args)]
pub struct ConnectArgs {
    /// 服务器的 SSH 地址：`用户@主机`，或者 ~/.ssh/config 里的 Host 名
    target: String,
    /// 服务器的 SSH 端口（默认 22，或者 ~/.ssh/config 里写的）
    #[arg(short = 'p', long)]
    ssh_port: Option<u16>,
    /// 本机用哪个端口（默认随便挑一个空着的）
    #[arg(long)]
    local_port: Option<u16>,
    /// 连上后自动打开浏览器
    #[arg(long)]
    open: bool,
    /// 先用密码登录一次，把这台电脑的 SSH 公钥装到服务器上（之后都用密钥连）
    #[arg(long)]
    install_key: bool,
}

/// `sa url`：打印这台机器上 Myriad 的连接地址（带令牌）。不占数据目录，服务开着也能用。
pub fn url(data_dir: &DataDir, _args: UrlArgs) -> Result<()> {
    let RemoteService { addr, token } = local_service(data_dir)?;
    println!("http://{addr}/auth?token={token}");
    if TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_err() {
        eprintln!("提示：{addr} 上没有服务在听，先运行 sa serve（或 sa service install）。");
    }
    Ok(())
}

/// 这台机器上 Myriad 服务的地址和令牌。
pub(crate) fn local_service(data_dir: &DataDir) -> Result<RemoteService> {
    data_dir
        .create()
        .with_context(|| format!("无法创建数据目录 {}", data_dir.root().display()))?;
    let config = Config::load_or_default(&data_dir.config())?;
    let token = load_or_create_token(&data_dir.token())?;
    let host = match config.server.host {
        host if host.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        host => host,
    };
    Ok(RemoteService {
        addr: SocketAddr::new(host, config.server.port),
        token,
    })
}

/// `sa connect`：开隧道，打印本机地址，一直到 Ctrl+C。
pub fn connect(args: ConnectArgs) -> Result<()> {
    crate::app::init_logging(tracing::Level::WARN);
    let target = SshTarget::parse(&args.target, args.ssh_port)?;
    if args.install_key {
        println!("用密码登录 {target}，把这台电脑的公钥装上去……");
        target.install_key(None)?;
        println!("公钥装好了，以后不用再输密码。");
    }
    println!("正在连接 {target}……");
    let tunnel = Tunnel::open(&target, args.local_port, &TunnelStop::default())?;
    println!("已连上 {target}，用浏览器打开：");
    println!("{}", tunnel.url());
    println!("断线会自动重连。按 Ctrl+C 断开。");
    if args.open {
        if let Err(error) = crate::serve::open_browser(tunnel.url()) {
            eprintln!("没能打开浏览器：{error}");
        }
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(crate::serve::stop_signal());
    tunnel.close();
    Ok(())
}

/// 用 SSH 能连上的一台机器。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    /// `用户@主机`、`主机`，或者 ~/.ssh/config 里的 Host 名。
    pub host: String,
    /// 不给就用 SSH 自己的默认值。
    pub port: Option<u16>,
}

impl SshTarget {
    pub fn parse(host: &str, port: Option<u16>) -> Result<Self> {
        let host = host.trim();
        if host.is_empty() {
            bail!("没有填服务器地址");
        }
        // 以 `-` 开头会被 ssh 当成选项。
        let allowed = |c: char| c.is_alphanumeric() || "@._-:[]%".contains(c);
        if host.starts_with('-') || !host.chars().all(allowed) {
            bail!("服务器地址「{host}」不对：写成 用户@主机，比如 ubuntu@1.2.3.4");
        }
        if port == Some(0) {
            bail!("SSH 端口不能是 0");
        }
        Ok(Self {
            host: host.to_string(),
            port,
        })
    }

    /// 所有 ssh 调用共用的参数：不问密码、不卡住，第一次连的主机自动记下它的指纹，
    /// 指纹变了照样拒绝。
    fn command(&self) -> Command {
        let mut command = self.base_command();
        command.args(["-o", "BatchMode=yes"]);
        hide_console(&mut command);
        command
    }

    fn base_command(&self) -> Command {
        let mut command = Command::new("ssh");
        command.args([
            "-o",
            "ConnectTimeout=10",
            "-o",
            "StrictHostKeyChecking=accept-new",
        ]);
        if let Some(port) = self.port {
            command.arg("-p").arg(port.to_string());
        }
        command
    }

    /// 在服务器上跑 `sa url`，拿到服务在服务器上的地址和令牌。
    pub fn remote_service(&self) -> Result<RemoteService> {
        let output = self
            .command()
            .args(["--", &self.host, &format!("{REMOTE_PATH} sa url")])
            .stdin(Stdio::null())
            .output()
            .map_err(ssh_missing)?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            return Err(ssh_failure(output.status.code(), stderr.trim()));
        }
        RemoteService::parse(&stdout)
            .with_context(|| format!("服务器上的 sa url 输出看不懂：{}", stdout.trim()))
    }

    /// `ssh -G`：这台服务器最终用哪些配置（~/.ssh/config 里写的也算上），不真的去连。
    fn ssh_config(&self) -> Result<SshConfig> {
        let output = self
            .base_command()
            .args(["-G", "--", &self.host])
            .stdin(Stdio::null())
            .output()
            .map_err(ssh_missing)?;
        if !output.status.success() {
            bail!(
                "读不了这台服务器的 SSH 配置：{}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(SshConfig::parse(
            &String::from_utf8_lossy(&output.stdout),
            dirs::home_dir().as_deref(),
        ))
    }

    /// 登录一次，把这台电脑的公钥加进服务器的 `~/.ssh/authorized_keys`（已经有了就不加）。
    /// 给了 `password` 就由 ssh 来问我们要（桌面版没有终端）；不给就由 ssh 在终端里问人。
    /// 密码只交给这一个 ssh，不写到哪里。
    pub fn install_key(&self, password: Option<&str>) -> Result<()> {
        let config = self.ssh_config()?;
        let key = local_public_key(&config.identities)?;
        let mut command = self.base_command();
        command.args([
            "-o",
            "PreferredAuthentications=keyboard-interactive,password",
            "-o",
            "NumberOfPasswordPrompts=1",
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=none",
            "--",
            &self.host,
            INSTALL_KEY,
        ]);
        // 命令行里由 ssh 在终端问密码，Windows 上要留着控制台；桌面版由我们来答，不弹窗。
        if let Some(password) = password {
            hide_console(&mut command);
            let exe = std::env::current_exe().context("找不到自己的可执行文件")?;
            command
                .env("SSH_ASKPASS", exe)
                .env("SSH_ASKPASS_REQUIRE", "force")
                .env(ASKPASS_ENV, password);
            // 老版本的 ssh 没有 SSH_ASKPASS_REQUIRE，要有 DISPLAY 才肯用 SSH_ASKPASS。
            if std::env::var_os("DISPLAY").is_none() {
                command.env("DISPLAY", ":0");
            }
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(ssh_missing)?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = writeln!(stdin, "{key}");
        }
        let output = child.wait_with_output().context("ssh 没能执行")?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        if stderr.contains("Permission denied") {
            let hint = if stderr.contains("password") || stderr.contains("keyboard-interactive") {
                "密码不对，或者这个用户不能用密码登录"
            } else {
                "服务器不允许用密码登录：请在服务器上手动把公钥加进 ~/.ssh/authorized_keys"
            };
            bail!("{hint}。\n\nssh 的原话：{stderr}");
        }
        Err(ssh_failure(output.status.code(), stderr))
    }
}

/// 在服务器上从标准输入读一行公钥，没有就加进 authorized_keys。用 `sh -c`，登录 shell 是 fish 之类的也能跑。
const INSTALL_KEY: &str = "sh -c 'umask 077; read -r key || exit 1; \
mkdir -p ~/.ssh && touch ~/.ssh/authorized_keys || exit 1; \
grep -qxF \"$key\" ~/.ssh/authorized_keys && exit 0; \
if [ -s ~/.ssh/authorized_keys ] && [ -n \"$(tail -c 1 ~/.ssh/authorized_keys)\" ]; then echo >> ~/.ssh/authorized_keys; fi; \
printf \"%s\\n\" \"$key\" >> ~/.ssh/authorized_keys'";

/// ssh 问密码时，把密码交给它的环境变量。
const ASKPASS_ENV: &str = "SA_ASKPASS_PASSWORD";

/// ssh 把我们自己当成 SSH_ASKPASS 来调时，在这里答完就退出。`sa` 和桌面版启动时最先调它。
/// 只回答问密码的提示，别的（比如确认指纹）一律不答。
pub fn answer_askpass() {
    let Some(password) = std::env::var_os(ASKPASS_ENV) else {
        return;
    };
    let prompt = std::env::args().nth(1).unwrap_or_default();
    if !prompt.to_lowercase().contains("password") {
        std::process::exit(1);
    }
    println!("{}", password.to_string_lossy());
    std::process::exit(0);
}

/// `ssh -G` 里用得上的几项。
#[derive(Debug, Default, PartialEq)]
struct SshConfig {
    /// 按 ssh 试的先后排好的私钥文件。
    identities: Vec<PathBuf>,
    /// 经跳板机（ProxyJump / ProxyCommand）连。
    proxied: bool,
}

impl SshConfig {
    fn parse(output: &str, home: Option<&Path>) -> Self {
        let mut config = Self::default();
        for line in output.lines() {
            let Some((key, value)) = line.trim().split_once(' ') else {
                continue;
            };
            let value = value.trim();
            match key {
                "identityfile" => config
                    .identities
                    .push(match (value.strip_prefix("~/"), home) {
                        (Some(rest), Some(home)) => home.join(rest),
                        _ => PathBuf::from(value),
                    }),
                "proxyjump" | "proxycommand" if value != "none" => config.proxied = true,
                _ => {}
            }
        }
        config
    }
}

/// ssh 连这台服务器时会试的公钥里，第一把不用输口令就能用的（没设口令，或者已经交给了 ssh-agent）。
/// 一把都没有就新建一对 ed25519（不设口令），放在 ssh 会去找的位置，和 ssh-copy-id 前要做的一样。
fn local_public_key(identities: &[PathBuf]) -> Result<String> {
    let agent = agent_keys();
    let mut locked = None;
    for private in identities {
        let public = public_path(private);
        if !private.is_file() || !public.is_file() {
            continue;
        }
        let Ok(key) = read_public_key(&public) else {
            continue;
        };
        if unlocked(private) || key_in(&agent, &key) {
            return Ok(key);
        }
        locked.get_or_insert(private.clone());
    }
    // 默认的那串里有 id_ed25519；配置里自己写了 IdentityFile 的，就建在写的第一个位置。
    let private = identities
        .iter()
        .find(|path| path.file_name().is_some_and(|name| name == "id_ed25519"))
        .or(identities.first())
        .filter(|path| !path.exists())
        .cloned();
    let Some(private) = private else {
        return Err(match locked {
            Some(path) => anyhow::anyhow!(
                "{} 设了口令：先运行 ssh-add {} 把它交给 ssh-agent，再连",
                path.display(),
                path.display()
            ),
            None => anyhow::anyhow!(
                "找不到能用的 SSH 公钥（{} 这类要有对应的 .pub）",
                identities
                    .first()
                    .map_or("~/.ssh/id_ed25519".into(), |path| path
                        .display()
                        .to_string())
            ),
        });
    };
    if let Some(dir) = private.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("建不了 {}", dir.display()))?;
    }
    let mut keygen = Command::new("ssh-keygen");
    keygen
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "myriad", "-f"])
        .arg(&private)
        .stdin(Stdio::null());
    hide_console(&mut keygen);
    let output = keygen.output().map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => {
            anyhow::anyhow!("没找到 ssh-keygen 命令：先装 OpenSSH 客户端")
        }
        _ => anyhow::Error::new(error).context("ssh-keygen 启动失败"),
    })?;
    if !output.status.success() {
        bail!(
            "没能新建 SSH 密钥：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    read_public_key(&public_path(&private))
}

fn public_path(private: &Path) -> PathBuf {
    let mut name = private.as_os_str().to_owned();
    name.push(".pub");
    PathBuf::from(name)
}

/// 用空口令能读出这把私钥，就是没设口令。
fn unlocked(private: &Path) -> bool {
    let mut keygen = Command::new("ssh-keygen");
    keygen
        .args(["-y", "-P", "", "-f"])
        .arg(private)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    hide_console(&mut keygen);
    keygen.status().is_ok_and(|status| status.success())
}

/// ssh-agent 里有的公钥（`ssh-add -L`），没有 agent 就是空的。
fn agent_keys() -> String {
    let mut add = Command::new("ssh-add");
    add.arg("-L").stdin(Stdio::null()).stderr(Stdio::null());
    hide_console(&mut add);
    add.output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default()
}

/// 比较公钥本身（类型和那一长串），不管后面的注释。
fn key_in(agent: &str, key: &str) -> bool {
    fn body(line: &str) -> Vec<&str> {
        line.split_whitespace().take(2).collect()
    }
    agent.lines().any(|line| body(line) == body(key))
}

fn read_public_key(path: &Path) -> Result<String> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("读不了公钥 {}", path.display()))?;
    let key = text.lines().next().unwrap_or_default().trim();
    // 一行里只能是公钥本身：带了单引号、换行之类的，服务器上那段脚本就不对了。
    if !key.starts_with("ssh-") && !key.starts_with("ecdsa-") || key.contains(['\'', '"', '\\']) {
        bail!("{} 不像一个 SSH 公钥", path.display());
    }
    Ok(key.to_string())
}

fn ssh_missing(error: std::io::Error) -> anyhow::Error {
    match error.kind() {
        std::io::ErrorKind::NotFound => anyhow::anyhow!("没找到 ssh 命令：先装 OpenSSH 客户端"),
        _ => anyhow::Error::new(error).context("ssh 启动失败"),
    }
}

/// 服务器不认这台电脑的密钥。桌面版据此给出密码框。
#[derive(Debug)]
pub struct LoginDenied(String);

impl std::fmt::Display for LoginDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LoginDenied {}

fn ssh_failure(code: Option<i32>, stderr: &str) -> anyhow::Error {
    let explained = explain_ssh_failure(code, stderr);
    if stderr.contains("Permission denied") {
        LoginDenied(explained).into()
    } else {
        anyhow::anyhow!(explained)
    }
}

impl std::fmt::Display for SshTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.port {
            Some(port) => write!(f, "{}（端口 {port}）", self.host),
            None => f.write_str(&self.host),
        }
    }
}

/// 把 ssh 的报错翻译成人能照着做的话。
fn explain_ssh_failure(code: Option<i32>, stderr: &str) -> String {
    let hint = if stderr.contains("Permission denied") {
        "SSH 登录被拒绝：这台电脑的密钥还没装到服务器上。在桌面版里填一次密码装好，或者运行 sa connect --install-key 用户@主机"
    } else if stderr.contains("Host key verification failed")
        || stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
    {
        "服务器的指纹和上次记下的不一样，为了安全不连。确认服务器没被换过后，在终端里 ssh 一次处理"
    } else if stderr.contains("Could not resolve hostname") {
        "找不到这个主机名"
    } else if stderr.contains("timed out") || stderr.contains("Connection refused") {
        "连不上服务器的 SSH：检查地址、端口和网络"
    } else if code == Some(127) || stderr.contains("not found") {
        "服务器上找不到 sa：把它放进 PATH（比如 ~/.local/bin 或 ~/.cargo/bin）"
    } else {
        "ssh 没能执行"
    };
    if stderr.is_empty() {
        format!("{hint}。")
    } else {
        format!("{hint}。\n\nssh 的原话：{stderr}")
    }
}

/// 服务在服务器上的地址和令牌，来自 `sa url`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteService {
    pub addr: SocketAddr,
    pub token: String,
}

impl RemoteService {
    /// 取输出里最后一个 `http://地址/auth?token=令牌`。
    pub(crate) fn parse(output: &str) -> Option<Self> {
        let line = output
            .lines()
            .rev()
            .find_map(|line| line.trim().strip_prefix("http://"))?;
        let (addr, token) = line.split_once("/auth?token=")?;
        let token = token.trim();
        if token.is_empty() || !token.chars().all(|c| c.is_ascii_alphanumeric()) {
            return None;
        }
        Some(Self {
            addr: addr.parse().ok()?,
            token: token.to_string(),
        })
    }
}

/// 断开一条隧道，包括还在连的。连接还没成功就能交给别的线程，让它随时喊停。
#[derive(Clone, Default)]
pub struct TunnelStop {
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
}

impl TunnelStop {
    /// 停下来，杀掉 ssh。之后这个 `TunnelStop` 不能再开隧道。
    pub fn close(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

/// 开着的隧道。丢掉或 [`Tunnel::close`] 时断开。
pub struct Tunnel {
    url: String,
    stop: TunnelStop,
    keeper: Option<JoinHandle<()>>,
}

impl Tunnel {
    /// 连上 `target` 上的 Myriad，等到服务能应答才返回。`local_port` 不给就挑一个空着的。
    /// 连的途中 `stop.close()` 了就放弃。
    pub fn open(target: &SshTarget, local_port: Option<u16>, stop: &TunnelStop) -> Result<Self> {
        let remote = target.remote_service()?;
        let local_port = match local_port {
            Some(port) => port,
            None => free_port()?,
        };
        let local = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), local_port);
        // 端口被别的程序占着的话，下面的探测会把令牌发给它。
        TcpListener::bind(local).with_context(|| format!("本机端口 {local_port} 已经被占用"))?;
        let forward = forward_command(target, local, remote.addr);

        {
            let mut slot = stop.child.lock().unwrap();
            if stop.stopped() {
                bail!("已取消连接");
            }
            *slot = Some(spawn(forward(), true)?);
        }
        if let Err(error) = wait_ready(stop, local, &remote.token) {
            stop.close();
            return Err(error);
        }

        let keeper = {
            let (stop, name) = (stop.clone(), target.to_string());
            std::thread::spawn(move || keep_alive(&name, &stop, forward))
        };
        Ok(Self {
            url: format!("http://{local}/auth?token={}", remote.token),
            stop: stop.clone(),
            keeper: Some(keeper),
        })
    }

    /// 本机这头的地址，带令牌。
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn close(mut self) {
        self.shut();
    }

    fn shut(&mut self) {
        self.stop.close();
        if let Some(keeper) = self.keeper.take() {
            let _ = keeper.join();
        }
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.shut();
    }
}

/// 隧道的 ssh 命令。每次重连都新建一个。
fn forward_command(
    target: &SshTarget,
    local: SocketAddr,
    remote: SocketAddr,
) -> impl Fn() -> Command + Send + 'static {
    let target = target.clone();
    move || {
        let mut command = target.command();
        command
            .args([
                "-N",
                // 不借用已经开着的共享连接（ControlMaster）：不然转发挂在那条连接上，
                // 杀掉这个 ssh 也断不了。
                "-o",
                "ControlMaster=no",
                "-o",
                "ControlPath=none",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "-L",
            ])
            .arg(format!(
                "{}:{}:{}:{}",
                local.ip(),
                local.port(),
                bracket(remote.ip()),
                remote.port()
            ))
            .args(["--", &target.host]);
        command
    }
}

fn bracket(ip: IpAddr) -> String {
    match ip {
        IpAddr::V6(ip) => format!("[{ip}]"),
        IpAddr::V4(ip) => ip.to_string(),
    }
}

fn spawn(mut command: Command, capture: bool) -> Result<Child> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(if capture {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .spawn()
        .context("ssh 启动失败")
}

/// 等隧道那头的服务用这个令牌应答。ssh 先退出了就把它的报错带出来。
fn wait_ready(stop: &TunnelStop, local: SocketAddr, token: &str) -> Result<()> {
    let deadline = Instant::now() + READY_WAIT;
    loop {
        let exited = {
            let mut slot = stop.child.lock().unwrap();
            let Some(child) = slot.as_mut().filter(|_| !stop.stopped()) else {
                bail!("已取消连接");
            };
            child
                .try_wait()?
                .map(|status| (status, child.stderr.take()))
        };
        // 放开锁再读：ssh 的代理之类的子进程可能还占着 stderr，读到头要等它，别让断开也跟着等。
        if let Some((status, pipe)) = exited {
            let mut stderr = String::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_string(&mut stderr);
            }
            return Err(ssh_failure(status.code(), stderr.trim()));
        }
        match probe(local, token) {
            Probe::Ok => return Ok(()),
            Probe::Unauthorized => bail!(
                "连上了，但服务器上 Myriad 的令牌和 sa url 给的不一样：服务可能用的是另一个数据目录"
            ),
            Probe::NotYet => {}
        }
        if Instant::now() >= deadline {
            bail!("SSH 连上了，但服务器上的 Myriad 没有应答：在服务器上运行 sa serve，或者 sa service install 让它常驻");
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

enum Probe {
    Ok,
    Unauthorized,
    NotYet,
}

/// 用令牌问一下房间列表：通了是 200，令牌不对是 401，服务没在跑时 ssh 会直接断掉连接。
fn probe(local: SocketAddr, token: &str) -> Probe {
    let Ok(mut stream) = TcpStream::connect_timeout(&local, Duration::from_secs(1)) else {
        return Probe::NotYet;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let request = format!(
        "GET /api/rooms HTTP/1.0\r\nHost: {local}\r\nAuthorization: Bearer {token}\r\n\r\n"
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return Probe::NotYet;
    }
    let mut head = [0u8; 12];
    let mut read = 0;
    while read < head.len() {
        match stream.read(&mut head[read..]) {
            Ok(0) | Err(_) => return Probe::NotYet,
            Ok(n) => read += n,
        }
    }
    match &head[9..12] {
        b"200" => Probe::Ok,
        b"401" => Probe::Unauthorized,
        _ => Probe::NotYet,
    }
}

/// 隧道断了就重开，直到 `stop`。
fn keep_alive(name: &str, stop: &TunnelStop, forward: impl Fn() -> Command) {
    let child = &stop.child;
    let mut wait = RETRY_FIRST;
    loop {
        let exited = {
            let mut slot = child.lock().unwrap();
            match slot.as_mut().map(Child::try_wait) {
                None => return,
                Some(Ok(None)) => false,
                Some(_) => true,
            }
        };
        if stop.stopped() {
            return;
        }
        if !exited {
            wait = RETRY_FIRST;
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }
        tracing::warn!("到 {name} 的隧道断了，{wait:?} 后重连");
        if sleep_unless(&stop.stop, wait) {
            return;
        }
        wait = (wait * 2).min(RETRY_MAX);
        let mut slot = child.lock().unwrap();
        if stop.stopped() {
            return;
        }
        match spawn(forward(), false) {
            Ok(fresh) => *slot = Some(fresh),
            Err(error) => tracing::warn!("重连 {name} 失败：{error:#}"),
        }
    }
}

/// 睡 `total`，中途 `stop` 了就提前返回 true。
fn sleep_unless(stop: &AtomicBool, total: Duration) -> bool {
    let until = Instant::now() + total;
    while Instant::now() < until {
        if stop.load(Ordering::SeqCst) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    stop.load(Ordering::SeqCst)
}

fn free_port() -> Result<u16> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).context("本机找不到空着的端口")?;
    Ok(listener.local_addr()?.port())
}

/// 桌面版在 Windows 上没有终端窗口，ssh 不能再弹一个出来。
fn hide_console(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_that_ssh_would_read_as_options_are_refused() {
        assert!(SshTarget::parse("ubuntu@1.2.3.4", None).is_ok());
        assert!(SshTarget::parse("  my-vps ", Some(2222)).is_ok());
        assert!(SshTarget::parse("ubuntu@[::1]", None).is_ok());
        assert!(SshTarget::parse("", None).is_err());
        assert!(SshTarget::parse("-oProxyCommand=evil", None).is_err());
        assert!(SshTarget::parse("host; rm -rf /", None).is_err());
        assert!(SshTarget::parse("a b", None).is_err());
        assert!(SshTarget::parse("host", Some(0)).is_err());
    }

    #[test]
    fn the_address_is_read_from_the_last_url_line() {
        let output = "欢迎\nhttp://127.0.0.1:4517/auth?token=abc123\n";
        assert_eq!(
            RemoteService::parse(output),
            Some(RemoteService {
                addr: "127.0.0.1:4517".parse().unwrap(),
                token: "abc123".into(),
            })
        );
        let v6 = RemoteService::parse("http://[::1]:9/auth?token=x").unwrap();
        assert_eq!(v6.addr, "[::1]:9".parse().unwrap());
        assert_eq!(RemoteService::parse("没有地址"), None);
        assert_eq!(RemoteService::parse("http://127.0.0.1:1/auth?token="), None);
        assert_eq!(
            RemoteService::parse("http://127.0.0.1:1/auth?token=a&b"),
            None
        );
    }

    #[test]
    fn the_forward_never_puts_the_host_where_an_option_goes() {
        let target = SshTarget::parse("me@vps", Some(2222)).unwrap();
        let command = forward_command(
            &target,
            "127.0.0.1:5000".parse().unwrap(),
            "[::1]:4517".parse().unwrap(),
        )();
        let args: Vec<_> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let tail = &args[args.len() - 4..];
        assert_eq!(tail, ["-L", "127.0.0.1:5000:[::1]:4517", "--", "me@vps"]);
        assert!(args.windows(2).any(|w| w == ["-p", "2222"]));
        assert!(args.iter().any(|a| a == "BatchMode=yes"));
    }

    #[cfg(unix)]
    #[test]
    fn the_key_is_added_once_on_its_own_line() {
        let home = tempfile::tempdir().unwrap();
        let keys = home.path().join(".ssh/authorized_keys");
        let install = |key: &str| {
            let mut child = Command::new("sh")
                .args(["-c", INSTALL_KEY])
                .env("HOME", home.path())
                .stdin(Stdio::piped())
                .spawn()
                .unwrap();
            writeln!(child.stdin.take().unwrap(), "{key}").unwrap();
            assert!(child.wait().unwrap().success());
        };
        install("ssh-ed25519 AAAA one");
        install("ssh-ed25519 AAAA one");
        assert_eq!(
            std::fs::read_to_string(&keys).unwrap(),
            "ssh-ed25519 AAAA one\n"
        );
        std::fs::write(&keys, "ssh-rsa BBBB old").unwrap();
        install("ssh-ed25519 AAAA one");
        assert_eq!(
            std::fs::read_to_string(&keys).unwrap(),
            "ssh-rsa BBBB old\nssh-ed25519 AAAA one\n"
        );
    }

    #[test]
    fn the_resolved_ssh_config_is_read() {
        let output =
            "user me\nidentityfile ~/.ssh/id_rsa\nidentityfile /keys/work\nproxyjump none\n";
        assert_eq!(
            SshConfig::parse(output, Some(Path::new("/home/me"))),
            SshConfig {
                identities: vec!["/home/me/.ssh/id_rsa".into(), "/keys/work".into()],
                proxied: false,
            }
        );
        assert!(SshConfig::parse("proxyjump jump@bastion\n", None).proxied);
        assert!(SshConfig::parse("proxycommand nc %h %p\n", None).proxied);
    }

    #[test]
    fn keys_are_picked_by_what_ssh_would_try() {
        let dir = tempfile::tempdir().unwrap();
        let (rsa, ed) = (dir.path().join("id_rsa"), dir.path().join("id_ed25519"));
        // 都还没有：建在 id_ed25519，而不是排第一的 id_rsa。
        let made = local_public_key(&[rsa.clone(), ed.clone()]).unwrap();
        assert!(made.starts_with("ssh-ed25519 ") && ed.is_file());
        // 配置里只写了自己的 IdentityFile：用它，没有就建在那里。
        let work = dir.path().join("work");
        let mine = local_public_key(std::slice::from_ref(&work)).unwrap();
        assert!(work.is_file() && mine != made);
        assert_eq!(local_public_key(&[work.clone(), ed.clone()]).unwrap(), mine);
        // 设了口令、agent 里也没有的跳过。
        let status = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "secret", "-f"])
            .arg(&rsa)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(local_public_key(&[rsa.clone(), ed]).unwrap(), made);
        let error = local_public_key(&[rsa]).unwrap_err().to_string();
        assert!(error.contains("ssh-add"), "{error}");
        assert!(key_in("ssh-ed25519 AAAA other\n", "ssh-ed25519 AAAA me"));
        assert!(!key_in("ssh-ed25519 BBBB me\n", "ssh-ed25519 AAAA me"));
    }

    #[test]
    fn only_public_keys_are_sent() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("id.pub");
        std::fs::write(&file, "ssh-ed25519 AAAA me@pc\n").unwrap();
        assert_eq!(read_public_key(&file).unwrap(), "ssh-ed25519 AAAA me@pc");
        std::fs::write(&file, "ssh-ed25519 AAAA'; rm -rf ~ #").unwrap();
        assert!(read_public_key(&file).is_err());
        std::fs::write(&file, "-----BEGIN OPENSSH PRIVATE KEY-----").unwrap();
        assert!(read_public_key(&file).is_err());
    }

    #[test]
    fn a_refused_key_is_told_apart() {
        assert!(ssh_failure(Some(255), "Permission denied (publickey).")
            .downcast_ref::<LoginDenied>()
            .is_some());
        assert!(ssh_failure(Some(255), "Connection refused")
            .downcast_ref::<LoginDenied>()
            .is_none());
    }

    #[test]
    fn ssh_errors_say_what_to_do() {
        assert!(explain_ssh_failure(Some(255), "Permission denied (publickey).").contains("密钥"));
        assert!(explain_ssh_failure(Some(127), "sh: 1: sa: not found").contains("PATH"));
        assert!(explain_ssh_failure(Some(1), "").ends_with('。'));
    }
}
