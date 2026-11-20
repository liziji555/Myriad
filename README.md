# SuperAgent

Rust 多 Bot Agent Harness：房间里的 AI 团队。一个可执行文件，自带内核、本机服务和三栏界面。

## 现在能做什么（M0）

- `sa serve`：启动本机服务，建好数据库、默认 Bot「助手」和默认房间「开发房间」
- `sa doctor`：检查数据目录、配置、模型密钥、数据库、端口和前端页面
- 三栏界面：房间列表、Bot 列表、新建房间、右侧任务 / 成员 / 文件 / 活动面板
- 事件全部落库，页面通过事件流实时更新，断线后从上次的位置续上

还不能做：对话、模型调用、工具、任务执行。这些在后面的里程碑里。

## 运行

需要 Rust（版本见 `rust-toolchain.toml`）、Node 24 和 pnpm。

```sh
pnpm --dir web install
pnpm --dir web build     # 先构建前端，它会被编进可执行文件
cargo run -p sa-cli -- serve
```

启动后终端会打印一个带令牌的链接，用它打开页面。服务只监听 `127.0.0.1`。

常用参数：`--data-dir <目录>` 指定数据目录，`serve --port <端口>` 换端口，`serve --open` 自动开浏览器。

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
| `crates/sa-server` | HTTP 接口、事件流、内嵌前端 |
| `crates/sa-cli` | 可执行文件 `sa` |
| `web` | React 前端 |

## 许可

MIT。`web/src/vendor/light-ui` 来自 Lancer（MIT），许可文件在该目录内。
