# SuperAgent

Rust 多 Bot Agent Harness：房间里的 AI 团队。一个可执行文件，自带内核、本机服务和三栏界面。

## 现在能做什么（M2）

- 在房间里和入口 Bot 对话：回复边写边显示，可以中途停止
- Bot 能用只读工具查资料：读文件、列目录、按名字找文件、按内容搜索、抓公开网页
- Bot 能动手：写文件、改文件、执行命令（Windows 用 PowerShell，其他系统用 bash）、列待办清单
- 有风险的操作先问你：审批卡上可以选「允许一次」「总是允许」「拒绝」
- 「停止」会连同命令启动的子进程一起结束
- 对话太长时自动把前面的部分压缩成摘要，之后接着聊
- 会话记录落盘，重启后历史还在，Bot 记得前面聊过什么
- `sa serve`：本机服务 + 三栏界面（房间、时间线、输入框、右侧面板）
- `sa chat`：在终端里对话
- `sa doctor`：检查数据目录、配置、模型密钥、数据库、端口和前端页面

还不能做：多个 Bot 协作、任务和交付物、定时和看门狗、Telegram / GitHub / MCP。这些在后面的里程碑里。

## 运行

需要 Rust（版本见 `rust-toolchain.toml`）、Node 24 和 pnpm。

```sh
pnpm --dir web install
pnpm --dir web build     # 先构建前端，它会被编进可执行文件
cargo run -p sa-cli -- serve --mock
```

启动后终端会打印一个带令牌的链接，用它打开页面。服务只监听 `127.0.0.1`。

`--mock` 用的是不联网的假模型，不需要密钥，回复按固定规则生成，用来先把流程走一遍。
假模型看消息里反引号前面的动词决定调哪个工具：

| 发这样的消息 | 假模型会做什么 |
|------|------|
| 读一下 `` `README.md` `` | 读文件 |
| 写入 `` `笔记.md` ``：第一行 | 写文件（工作目录里面不用批准） |
| 修改 `` `笔记.md` ``：第一行 => 改过了 | 把文件里的「第一行」换成「改过了」 |
| 运行 `` `git status` `` | 执行命令（会弹审批卡） |

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

也可以用别的 OpenAI 兼容服务。以 OpenCode Go 为例，在数据目录的 `config.toml` 里加一段，
再把 `[tiers]` 里三个档位的 `provider` 和 `model` 改过去，密钥放进 `OPENCODE_API_KEY`：

```toml
[providers.opencode-go]
kind = "openai_compat"
base_url = "https://opencode.ai/inference/go/v1"
api_key_env = "OPENCODE_API_KEY"
session_header = "x-opencode-session"   # 这个网关要求每个会话带一个固定的请求头

[tiers]
cheap = { provider = "opencode-go", model = "deepseek-v4.1-flash", thinking = false }
mid = { provider = "opencode-go", model = "deepseek-v4.1-flash" }
strong = { provider = "opencode-go", model = "deepseek-v4.1-flash" }
```

密钥只从环境变量读，不会写进配置文件或数据库。`config.toml` 里的每日花费上限这个版本还没有生效，
花费请在模型服务的控制台里看。换模型、换服务地址改数据目录里的 `config.toml`，
改完用 `sa doctor` 检查。

### Bot 能看到什么

Bot 的文件类工具只能读「工作目录」里面的内容，默认是启动命令时所在的目录，用 `--workdir <目录>` 指定别的。
数据目录（里面有访问令牌和数据库）即使在工作目录里也读不到。抓网页只能访问公网地址，并且不走系统代理（走代理就没法保证连的不是内网）。

### Bot 能改什么、什么时候问你

| 操作 | 怎么处理 |
|------|------|
| 读文件、搜索、抓网页、待办清单 | 直接执行 |
| 在工作目录里面写文件、改文件 | 直接执行 |
| 写工作目录之外的文件 | 弹审批卡 |
| 执行命令 | 弹审批卡 |
| 写数据目录 | 一律拒绝，不问 |

审批卡在界面的时间线里；`sa chat` 里是一行提示，输入 `y`（允许一次）、`a`（总是允许）或 `n`（拒绝）。
10 分钟没处理按拒绝算；服务重启时还在等的卡片作废。

「总是允许」只在这个房间里有效，并且只对一模一样的命令、同一个文件有效；它不是通配规则。
已经保存的许可可以通过接口查看和撤销：`GET /api/rooms/{房间}/rules`、`DELETE /api/rules/{id}`。

这个版本没有系统级沙箱：命令一旦批准，就以你的身份运行，能做你能做的任何事。批准之前请看清楚命令。

命令默认最多跑 120 秒（Bot 可以要求最多 600 秒），超时或点「停止」会结束整棵进程树。
工具返回的内容太长时，交给模型的是截短的版本，完整内容存在数据目录的 `spill/` 里。

### 对话太长怎么办

对话快占满模型的上下文窗口时，Bot 会先把前面的部分总结成一份摘要（时间线上显示「对话太长，压缩前面的内容」），
之后带着摘要和最近几条原文接着聊。房间里的消息和 `sessions/*.jsonl` 里的记录一条都不会删。

窗口大小默认按 1,000,000 token 算。模型的窗口更小、或者想早点压缩，在 `config.toml` 的档位里写
`context_window`，例如 `mid = { provider = "deepseek", model = "deepseek-flash", context_window = 128000 }`。
压缩线 = 窗口 − min(最大输出, 20,000) − 13,000。

### 常用命令

| 命令 | 作用 |
|------|------|
| `sa serve [--port 端口] [--open] [--workdir 目录] [--mock]` | 启动服务和界面 |
| `sa chat [--room 房间] [--workdir 目录] [--mock]` | 在终端里对话；`-m "内容"` 只发一条就退出 |
| `sa doctor` | 体检 |

所有命令都可以加 `--data-dir <目录>` 指定数据目录。`sa serve` 和 `sa chat` 用同一份数据，同一个数据目录不能同时运行两个。

数据目录里：`config.toml` 配置，`state.db` 房间和消息，`sessions/*.jsonl` 每个会话发给模型的完整记录，`spill/` 被截短的工具结果的完整内容，
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
| `crates/sa-tools` | 工具（读、搜、抓网页、写、改、执行命令、待办）和路径围栏 |
| `crates/sa-kernel` | 会话记录（JSONL）、「模型—工具」循环、审批关口、自动压缩 |
| `crates/sa-orchestrator` | 房间里来了消息，让入口 Bot 跑一轮并把结果写回房间 |
| `crates/sa-server` | HTTP 接口、事件流、内嵌前端 |
| `crates/sa-cli` | 可执行文件 `sa` |
| `web` | React 前端 |

## 许可

MIT。`web/src/vendor/light-ui` 来自 Lancer（MIT），许可文件在该目录内。
