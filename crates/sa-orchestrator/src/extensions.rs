//! Bot 多出来的本事：技能（写在文件里的做事套路）和外部 MCP 服务的工具。
//!
//! 技能每一轮开始时从磁盘上重新读，改了文件不用重启。
//! MCP 服务在后台连，连上了它的工具才出现在 Bot 的工具表里；没连上不耽误别的。

use crate::{Orchestrator, Shared};
use sa_protocol::{Extensions, McpServerInfo, McpState, SkillInfo};
use sa_tools::mcp::{McpError, McpServer, McpSpec};
use sa_tools::{SkillSet, SkillTool, Tool};
use std::path::PathBuf;
use std::sync::Arc;

/// 一个外部 MCP 服务，和它现在怎么样。
pub(crate) struct Mcp {
    pub server: Arc<McpServer>,
    pub info: McpServerInfo,
}

/// 一个 Bot 这一轮能用的技能：提示词里的那一段，和读全文用的工具。没有技能时两样都没有。
pub(crate) struct BotSkills {
    pub prompt: String,
    pub tool: Option<SkillTool>,
}

impl Mcp {
    /// 现在的样子。连上之后服务又退出了、重新启动也没成的，不能还说「已连上」。
    fn now(&self) -> McpServerInfo {
        let mut info = self.info.clone();
        if info.state == McpState::Ready {
            if let Some(problem) = self.server.problem() {
                info.state = McpState::Failed;
                info.error = Some(problem);
            }
        }
        info
    }
}

impl Shared {
    /// 现在每个 Bot 都有的工具。
    pub(crate) fn tools(&self) -> sa_tools::ToolRegistry {
        self.tools.read().unwrap().clone()
    }

    pub(crate) async fn skills_for(&self, bot_name: &str) -> BotSkills {
        let none = BotSkills {
            prompt: String::new(),
            tool: None,
        };
        let Some(dir) = self.skills_dir.lock().unwrap().clone() else {
            return none;
        };
        let (name, load_dir) = (bot_name.to_string(), dir.clone());
        let read = move || SkillSet::load(&load_dir).prompt_block(&name);
        let prompt = tokio::task::spawn_blocking(read).await.unwrap_or_default();
        if prompt.is_empty() {
            return none;
        }
        BotSkills {
            prompt,
            tool: Some(SkillTool {
                dir,
                bot_name: bot_name.to_string(),
            }),
        }
    }

    fn set_mcp(&self, name: &str, change: impl FnOnce(&mut McpServerInfo)) {
        let mut all = self.mcp.lock().unwrap();
        if let Some(mcp) = all.iter_mut().find(|mcp| mcp.info.name == name) {
            change(&mut mcp.info);
        }
    }
}

impl Orchestrator {
    /// 技能放在 `dir` 里。目录可以还不存在。
    pub fn use_skills(&self, dir: PathBuf) {
        *self.shared.skills_dir.lock().unwrap() = Some(dir);
    }

    /// 跨会话持久记忆目录（`memory/`）。启用后自动为每个 Bot 注入记忆索引并注册记忆工具。
    pub fn use_memory(&self, dir: PathBuf) {
        *self.shared.memory.lock().unwrap() = Some(Arc::new(sa_tools::MemoryStore::new(dir)));
    }

    /// 启用轮次级文件检查点（`checkpoints/`），自动快照内置 Bot 修改前的文件内容并支持安全回滚。
    pub fn use_checkpoints(&self, dir: PathBuf) {
        *self.shared.checkpoints.lock().unwrap() =
            Some(Arc::new(sa_tools::CheckpointStore::new(dir)));
    }

    /// 启用生命周期钩子（`PreToolUse` / `PostToolUse` / `Stop`）。
    pub fn use_hooks(&self, rules: Vec<sa_tools::HookRule>) {
        if rules.is_empty() {
            *self.shared.hooks.lock().unwrap() = None;
        } else {
            let workdir = self.shared.settings.fence.root().to_path_buf();
            *self.shared.hooks.lock().unwrap() =
                Some(Arc::new(sa_tools::HookRunner::new(rules, workdir)));
        }
    }

    /// 设置回复因 `MaxTokens` 截断时的自动续写次数上限。
    pub fn use_truncated_recoveries(&self, count: usize) {
        self.shared
            .max_truncated_recoveries
            .store(count, std::sync::atomic::Ordering::Relaxed);
    }

    /// 记录当前配置的 MCP 服务规格（透传注入到 Claude Code / Codex / ACP 等外部 CLI Harness）。
    pub fn set_mcp_specs(&self, specs: Vec<McpSpec>) {
        *self.shared.mcp_specs.lock().unwrap() = specs;
    }

    /// 给每个 Bot 多加一个工具。已经有同名的就不加，返回 `false`。
    pub fn add_tool(&self, tool: impl Tool + 'static) -> bool {
        let mut tools = self.shared.tools.write().unwrap();
        if tools.get(tool.name()).is_some() {
            return false;
        }
        tools.register(tool);
        true
    }

    /// 连一个外部 MCP 服务，连上了就把它的工具加给每个 Bot。返回加了哪些工具（服务自己的工具名）。
    pub async fn connect_mcp(&self, server: Arc<McpServer>) -> Result<Vec<String>, McpError> {
        let name = server.name().to_string();
        {
            let mut all = self.shared.mcp.lock().unwrap();
            if all.iter().any(|mcp| mcp.info.name == name) {
                return Err(McpError(format!("已经有一个叫「{name}」的 MCP 服务了")));
            }
            all.push(Mcp {
                server: server.clone(),
                info: McpServerInfo {
                    name: name.clone(),
                    state: McpState::Connecting,
                    tools: vec![],
                    error: None,
                },
            });
        }
        let connected = tokio::select! {
            connected = server.connect() => connected,
            _ = self.shared.shutdown.cancelled() => Err(McpError("服务正在关闭".into())),
        };
        match connected {
            Ok(infos) => {
                let mut added = vec![];
                for tool in server.tools(&infos) {
                    let remote = tool.remote_name().to_string();
                    if self.add_tool(tool) {
                        added.push(remote);
                    }
                }
                tracing::info!("MCP 服务「{name}」连上了，{} 个工具", added.len());
                self.shared.set_mcp(&name, |info| {
                    info.state = McpState::Ready;
                    info.tools = added.clone();
                });
                Ok(added)
            }
            Err(error) => {
                tracing::warn!("MCP 服务「{name}」没连上：{error}");
                server.close().await;
                self.shared.set_mcp(&name, |info| {
                    info.state = McpState::Failed;
                    info.error = Some(error.0.clone());
                });
                Err(error)
            }
        }
    }

    /// 在后台把配置里的 MCP 服务都连上。马上返回；哪个没连上只记下原因，不耽误别的。
    pub fn start_mcp(self: &Arc<Self>, specs: Vec<McpSpec>) {
        self.set_mcp_specs(specs.clone());
        for spec in specs {
            let this = self.clone();
            tokio::spawn(async move {
                let _ = this.connect_mcp(Arc::new(McpServer::new(spec))).await;
            });
        }
    }

    /// 现在有哪些技能、哪些 MCP 服务。要读磁盘，别在异步线程上直接调。
    pub fn extensions(&self) -> Extensions {
        let dir = self.shared.skills_dir.lock().unwrap().clone();
        let set = dir.map(|dir| SkillSet::load(&dir)).unwrap_or_default();
        let skills = set.skills.into_iter().map(|skill| SkillInfo {
            name: skill.name,
            description: skill.description,
            bots: skill.bots,
        });
        let mcp = self.shared.mcp.lock().unwrap();
        Extensions {
            skills: skills.collect(),
            skill_problems: set.problems,
            mcp: mcp.iter().map(Mcp::now).collect(),
        }
    }
}

/// 服务要关了：断开所有 MCP 服务，它们的进程一起结束。
pub(crate) async fn shutdown(shared: &Shared) {
    let servers: Vec<Arc<McpServer>> = {
        let all = shared.mcp.lock().unwrap();
        all.iter().map(|mcp| mcp.server.clone()).collect()
    };
    for server in servers {
        server.close().await;
    }
}
