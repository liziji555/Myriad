//! 把任务的成果送到外面去：推送任务分支、开 PR。
//!
//! 这两件事发生在这台电脑之外，做了就收不回来，所以每一次都要人批准，没有「总是允许」。
//! 做过的事记在 effects 表里：同一个任务不会开出两个 PR。

use crate::github::{self, NewPr};
use crate::room::db;
use crate::{workdir, Shared};
use async_trait::async_trait;
use sa_engine::Workdir;
use sa_protocol::{ArtifactKind, RunId, Task, TaskId};
use sa_tools::{Permission, Tool, ToolCtx, ToolError, ToolResult};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

/// 「这个任务的这个提交推送过」记在 effects 表里的名字。
pub(crate) fn push_key(task_id: &TaskId, sha: &str) -> String {
    format!("git_push:{task_id}:{sha}")
}

/// 这一次运行里，工具要用到的任务信息。
#[derive(Clone)]
pub(crate) struct Delivery {
    pub shared: Arc<Shared>,
    pub task: Task,
    pub run_id: RunId,
    pub workdir: Workdir,
}

impl Delivery {
    fn branch(&self) -> &str {
        self.workdir.branch.as_deref().unwrap_or_default()
    }

    fn push_key(&self, sha: &str) -> String {
        push_key(&self.task.id, sha)
    }

    fn pr_key(&self) -> String {
        format!("open_pr:{}", self.task.id)
    }

    async fn effect(&self, key: String) -> Result<Option<Value>, String> {
        let engine = self.shared.engine.clone();
        db(move || engine.effect(&key))
            .await
            .map_err(|e| e.to_string())
    }

    async fn record(&self, kind: &'static str, key: String, result: Value) {
        let (engine, id) = (self.shared.engine.clone(), self.task.id.clone());
        if let Err(error) = db(move || engine.record_effect(&id, kind, &key, &result)).await {
            tracing::error!(task = %self.task.id, "做过的事没记上（{kind}）：{error}");
        }
    }
}

/// `git_push`：把任务分支推到 origin。
pub(crate) struct GitPushTool(pub Delivery);

#[async_trait]
impl Tool for GitPushTool {
    fn name(&self) -> &'static str {
        "git_push"
    }

    fn description(&self) -> String {
        "把任务分支推送到远端 origin。每次都要用户批准。\
         只有任务明确要求推送或者开 PR 时才用。\
         推送之前会先把任务目录里现在的改动提交到任务分支，不用自己提交。"
            .into()
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    fn title(&self, _input: &Value) -> String {
        format!("推送分支 {}", self.0.branch())
    }

    async fn check_permission(&self, _input: &Value, _ctx: &ToolCtx) -> Permission {
        // 卡片上要写全：origin 配了几个推送地址，就会推到几个地方。
        match workdir::origin_urls(&self.0.workdir).await {
            Ok(urls) => {
                let urls: Vec<String> = urls.iter().map(|url| github::redact(url)).collect();
                Permission::Ask(format!(
                    "要把分支 {} 推送到 {}",
                    self.0.branch(),
                    urls.join(" 和 ")
                ))
            }
            Err(reason) => Permission::Deny(reason),
        }
    }

    async fn call(&self, _input: Value, ctx: &ToolCtx) -> ToolResult {
        let delivery = &self.0;
        // 推的是任务分支。任务目录被切到了别的分支上的话，改动不在任务分支里：
        // 这时推上去的是没更新的任务分支，却会报成推好了。所以先拦下，说清楚怎么回来。
        if let Some(expected) = &delivery.workdir.branch {
            let current = workdir::current_branch(&delivery.workdir).await;
            if current.as_deref() != Some(expected.as_str()) {
                let now = current.map_or("不在任何分支上".to_string(), |b| {
                    format!("在分支 {b} 上")
                });
                return Err(ToolError(format!(
                    "没有推送：任务目录现在{now}，不是任务分支 {expected}。任务已经有自己的分支，不用另建。\
                     先执行 git checkout {expected}，把别的分支上的提交合过来（git merge <那个分支>），再调用 git_push。"
                )));
            }
        }
        workdir::commit_and_diff(&delivery.workdir, &delivery.task.title)
            .await
            .map_err(|reason| ToolError(format!("任务目录里的改动没能提交：{reason}")))?;
        let sha = workdir::head(&delivery.workdir).await.map_err(ToolError)?;
        if sha == delivery.workdir.base_ref {
            return Err(ToolError::new(
                "任务分支上还没有任何改动，没有东西可以推送。先把活干了。",
            ));
        }
        workdir::push(&delivery.workdir, &ctx.cancel)
            .await
            .map_err(|reason| ToolError(format!("没有推上去：{reason}")))?;
        let done = json!({ "branch": delivery.branch(), "sha": sha });
        delivery
            .record("git_push", delivery.push_key(&sha), done)
            .await;
        let short: String = sha.chars().take(7).collect();
        Ok(format!(
            "分支 {} 已经推送到 origin（{short}）。",
            delivery.branch()
        ))
    }
}

/// `open_pr`：在 GitHub 上给任务分支开一个 PR。
pub(crate) struct OpenPrTool(pub Delivery);

#[derive(Deserialize)]
struct OpenPrInput {
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    base: Option<String>,
}

/// 这次开 PR 具体要做什么。
enum Plan {
    /// 这个任务已经开过了。
    Existing(String),
    Open {
        repo: String,
        title: String,
        body: String,
        base: String,
    },
}

impl OpenPrTool {
    async fn plan(&self, input: &Value) -> Result<Plan, String> {
        let delivery = &self.0;
        if let Some(done) = delivery.effect(delivery.pr_key()).await? {
            if let Some(url) = done["url"].as_str() {
                return Ok(Plan::Existing(url.to_string()));
            }
        }
        let input: OpenPrInput =
            serde_json::from_value(input.clone()).map_err(|e| format!("参数不对：{e}"))?;
        let title = input.title.trim().to_string();
        if title.is_empty() {
            return Err("title 是空的。写一句话说明这个 PR 做了什么。".into());
        }
        let Some(github) = &delivery.shared.settings.github else {
            return Err("这里没有配置 GitHub，开不了 PR。".into());
        };
        if github.token.as_deref().is_none_or(str::is_empty) {
            return Err(github::NO_TOKEN.into());
        }
        let repo = match &github.repo {
            Some(repo) => repo.clone(),
            None => {
                let urls = workdir::origin_urls(&delivery.workdir).await?;
                github::parse_repo(&urls[0]).ok_or_else(|| {
                    "从 origin 的地址里认不出 GitHub 仓库。请用户在 config.toml 的 [github] 里写上 repo = \"owner/name\"。".to_string()
                })?
            }
        };
        let base = match input.base.map(|base| base.trim().to_string()) {
            Some(base) if !base.is_empty() => base,
            _ => workdir::default_base(&delivery.workdir)
                .await
                .ok_or_else(|| "不知道要合到哪个分支。用 base 参数写明。".to_string())?,
        };
        if base == delivery.branch() {
            return Err("base 不能是任务分支自己。".into());
        }
        // PR 里显示的是推上去的东西：还没推、或者推完之后又改过，先让它推。
        workdir::commit_and_diff(&delivery.workdir, &delivery.task.title)
            .await
            .map_err(|reason| format!("任务目录里的改动没能提交：{reason}"))?;
        let sha = workdir::head(&delivery.workdir).await?;
        if delivery.effect(delivery.push_key(&sha)).await?.is_none() {
            return Err("任务分支上最新的改动还没有推送。先调用 git_push，再开 PR。".into());
        }
        Ok(Plan::Open {
            repo,
            title,
            body: input.body,
            base,
        })
    }
}

#[async_trait]
impl Tool for OpenPrTool {
    fn name(&self) -> &'static str {
        "open_pr"
    }

    fn description(&self) -> String {
        "在 GitHub 上给任务分支开一个 PR。每次都要用户批准。只有任务明确要求开 PR 时才用，\
         而且要先用 git_push 把最新的改动推上去。一个任务只开一个 PR：已经开过的话返回原来那个的地址，\
         之后的改动再 git_push 一次就会出现在 PR 里。\
         title 一句话说明做了什么；body 写改了什么、怎么验证的；base 是要合进去的分支，不写就用用户所在的分支。"
            .into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "PR 的标题" },
                "body": { "type": "string", "description": "PR 的说明" },
                "base": { "type": "string", "description": "合到哪个分支；不写用用户所在的分支" }
            },
            "required": ["title"]
        })
    }

    fn title(&self, input: &Value) -> String {
        format!("开 PR：{}", input["title"].as_str().unwrap_or_default())
    }

    async fn check_permission(&self, input: &Value, _ctx: &ToolCtx) -> Permission {
        match self.plan(input).await {
            // 不会再对外面做任何事，只是把地址报回去。
            Ok(Plan::Existing(_)) => Permission::Allow,
            Ok(Plan::Open {
                repo, title, base, ..
            }) => Permission::Ask(format!(
                "要在 GitHub 仓库 {repo} 上开一个 PR「{title}」，把 {} 合进 {base}",
                self.0.branch()
            )),
            Err(reason) => Permission::Deny(reason),
        }
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolResult {
        let delivery = &self.0;
        let (repo, title, body, base) = match self.plan(&input).await.map_err(ToolError)? {
            Plan::Existing(url) => {
                return Ok(format!(
                    "这个任务已经开过 PR：{url}\n后来的改动用 git_push 推上去就会出现在这个 PR 里。"
                ));
            }
            Plan::Open {
                repo,
                title,
                body,
                base,
            } => (repo, title, body, base),
        };
        let Some(github) = &delivery.shared.settings.github else {
            return Err(ToolError::new("这里没有配置 GitHub，开不了 PR。"));
        };
        let pr = NewPr {
            repo: &repo,
            title: &title,
            body: &body,
            head: delivery.branch(),
            base: &base,
        };
        let opened = github::open_pr(github, pr, &ctx.cancel)
            .await
            .map_err(ToolError)?;
        let done = json!({ "url": opened.url, "number": opened.number });
        delivery.record("open_pr", delivery.pr_key(), done).await;
        let (engine, id, run_id) = (
            delivery.shared.engine.clone(),
            delivery.task.id.clone(),
            delivery.run_id.clone(),
        );
        let (url, summary) = (opened.url.clone(), format!("#{} {title}", opened.number));
        let added =
            db(move || engine.add_artifact(&id, &run_id, ArtifactKind::Pr, &url, &summary)).await;
        if let Err(error) = added {
            tracing::error!(task = %delivery.task.id, "PR 没记进交付物：{error}");
        }
        Ok(format!(
            "PR 开好了：{}\n在 report 的 summary 里把这个地址告诉用户。",
            opened.url
        ))
    }
}
