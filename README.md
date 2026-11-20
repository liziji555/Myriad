# SuperAgent

Rust 多 Bot Agent Harness：房间里的 AI 团队。一个可执行文件，自带内核、本机服务和三栏界面。

## 现在能做什么（M1）

- 在房间里和入口 Bot 对话：回复边写边显示，可以中途停止
- Bot 能用只读工具查资料：读文件、列目录、按名字找文件、按内容搜索、抓公开网页
- 会话记录落盘，重启后历史还在，Bot 记得前面聊过什么
- `sa serve`：本机服务 + 三栏界面（房间、时间线、输入框、右侧面板）
- `sa chat`：在终端里对话
- `sa doctor`：检查数据目录、配置、模型密钥、数据库、端口和前端页面

还不能做：修改文件、执行命令、审批、多个 Bot 协作、任务和交付物。这些在后面的里程碑里。

## 运行

需要 Rust（版本见 `rust-toolchain.toml`）、Node 24 和 pnpm。

```sh
pnpm --dir web install
pnpm --dir web build     # 先构建前端，它会被编进可执行文件
cargo run -p sa-cli -- serve --mock
```

启动后终端会打印一个带令牌的链接，用它打开页面。服务只监听 `127.0.0.1`。

`--mock` 用的是不联网的假模型，不需要密钥，回复按固定规则生成，用来先把流程走一遍。
发一条带反引号路径的消息（例如：读一下 `` `README.md` ``）可以看到工具调用。

### 接上真实模型

默认配置用 DeepSeek。把 API Key 放进环境变量 `DEEPSEEK_API_KEY`，去掉 `--mock` 重新启动：

```powershell
# Windows PowerShell（只对当前窗口有效）
$env:DEEPSEEK_API_KEY = "sk-..."
sa serve
```

```sh
# macOS / Linux
export DEEPSEEK_API_KEY=sk-...
sa serve
```

密钥只从环境变量读，不会写进配置文件或数据库。`config.toml` 里的每日花费上限这个版本还没有生效，
花费请在模型服务的控制台里看。换模型、换服务地址改数据目录里的 `config.toml`，
改完用 `sa doctor` 检查。

### Bot 能看到什么

Bot 的文件类工具只能读「工作目录」里面的内容，默认是启动命令时所在的目录，用 `--workdir <目录>` 指定别的。
数据目录（里面有访问令牌和数据库）即使在工作目录里也读不到。抓网页只能访问公网地址，并且不走系统代理（走代理就没法保证连的不是内网）。

### 常用命令

| 命令 | 作用 |
|------|------|
| `sa serve [--port 端口] [--open] [--workdir 目录] [--mock]` | 启动服务和界面 |
| `sa chat [--room 房间] [--workdir 目录] [--mock]` | 在终端里对话；`-m "内容"` 只发一条就退出 |
| `sa doctor` | 体检 |

所有命令都可以加 `--data-dir <目录>` 指定数据目录。`sa serve` 和 `sa chat` 用同一份数据，同一个数据目录不能同时运行两个。

数据目录里：`config.toml` 配置，`state.db` 房间和消息，`sessions/*.jsonl` 每个会话发给模型的完整记录，
`prompts/chat.md`（可选）替换内置的系统提示词。

## 开发

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace          # 同时把 Rust 类型导出到 web/src/api/generated
pnpm --dir web test
pnpm --dir web dev              # 前端热更新，/api 转发到 127.0.0.1:4517
```

## 目录

| 路径 | 作用 |
| --- | --- |
| `crates/sa-protocol` | Rust 和前端共用的类型 |
| `crates/sa-store` | SQLite 持久层和迁移 |
| `crates/sa-engine` | 领域层，唯一的状态写入者 |
| `crates/sa-llm` | 模型接口：DeepSeek（OpenAI 兼容）流式调用、重试、测试用的假模型 |
| `crates/sa-tools` | 只读工具和路径围栏 |
| `crates/sa-kernel` | 会话记录（JSONL）和「模型—工具」循环 |
| `crates/sa-orchestrator` | 房间里来了消息，让入口 Bot 跑一轮并把结果写回房间 |
| `crates/sa-server` | HTTP 接口、事件流、内嵌前端 |
| `crates/sa-cli` | 可执行文件 `sa` |
| `web` | React 前端 |

## 许可

MIT。`web/src/vendor/light-ui` 来自 Lancer（MIT），许可文件在该目录内。
