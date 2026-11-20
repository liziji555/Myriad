//! 在 GitHub 上开 PR。只用到一个接口，所以直接发请求，不引 SDK。

use serde_json::{json, Value};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub const DEFAULT_API_BASE: &str = "https://api.github.com";
pub const NO_TOKEN: &str = "没有 GitHub 令牌，开不了 PR。请用户设置环境变量 GITHUB_TOKEN（或者 config.toml 里 [github] token_env 指的那个）后重启。分支已经推送的话，用户也可以自己在 GitHub 上开。";
const TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = concat!("superagent/", env!("CARGO_PKG_VERSION"));

/// 怎么连 GitHub。
#[derive(Debug, Clone, Default)]
pub struct GitHub {
    /// 接口地址；空的话用 [`DEFAULT_API_BASE`]。GitHub Enterprise 是 `https://主机/api/v3`。
    pub api_base: String,
    /// 访问令牌。没有就开不了 PR。
    pub token: Option<String>,
    /// `owner/name`。不写的话从 `origin` 的地址里认。
    pub repo: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    pub url: String,
    pub number: u64,
}

/// 从远端地址里认出 `owner/name`。认不出来（比如是本地路径）返回 `None`。
pub fn parse_repo(remote: &str) -> Option<String> {
    let remote = remote.trim().trim_end_matches('/');
    let remote = remote.strip_suffix(".git").unwrap_or(remote);
    let path = match remote.split_once("://") {
        // https://github.com/owner/name、ssh://git@github.com:22/owner/name
        Some((_, rest)) => rest.split_once('/')?.1,
        // git@github.com:owner/name
        None => {
            let (host, path) = remote.split_once(':')?;
            // `C:\repo`、`./a:b` 这样的是本地路径。
            if host.len() < 2 || host.contains(['/', '\\']) {
                return None;
            }
            path
        }
    };
    let mut parts = path.trim_matches('/').split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    let plain = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    };
    (parts.next().is_none() && plain(owner) && plain(name)).then(|| format!("{owner}/{name}"))
}

/// 给人看的远端地址：去掉里面可能带着的账号和密码。
pub fn redact(remote: &str) -> String {
    match remote.split_once("://") {
        Some((scheme, rest)) => {
            let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
            let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
            format!("{scheme}://{host}/{path}")
        }
        None => remote.to_string(),
    }
}

pub struct NewPr<'a> {
    pub repo: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub head: &'a str,
    pub base: &'a str,
}

/// 开一个 PR。这条分支已经有开着的 PR 时，返回那一个，不再开新的。
pub async fn open_pr(
    github: &GitHub,
    pr: NewPr<'_>,
    cancel: &CancellationToken,
) -> Result<PullRequest, String> {
    let Some(token) = github.token.as_deref().filter(|token| !token.is_empty()) else {
        return Err(NO_TOKEN.into());
    };
    let base = match github.api_base.trim_end_matches('/') {
        "" => DEFAULT_API_BASE,
        base => base,
    };
    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| format!("发不了请求：{e}"))?;
    let call = |request: reqwest::RequestBuilder| {
        let request = request
            .bearer_auth(token)
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28");
        async move {
            let sent = tokio::select! {
                sent = request.send() => sent,
                _ = cancel.cancelled() => return Err("被停下了，不确定 PR 有没有开出来".to_string()),
            };
            let response = sent.map_err(|e| format!("连不上 GitHub：{}", e.without_url()))?;
            let status = response.status();
            let body: Value = response.json().await.unwrap_or(Value::Null);
            Ok((status, body))
        }
    };

    let url = format!("{base}/repos/{}/pulls", pr.repo);
    let wanted = json!({ "title": pr.title, "body": pr.body, "head": pr.head, "base": pr.base });
    let (status, body) = call(http.post(&url).json(&wanted)).await?;
    if status.is_success() {
        return read_pr(&body).ok_or_else(|| "GitHub 的回复里没有 PR 的地址".to_string());
    }
    let said = explain(&body);
    // 这条分支已经有 PR 了（比如上次开出来了但没来得及记下）：找到它，当作这次的结果。
    if status.as_u16() == 422 && said.contains("already exists") {
        let owner = pr.repo.split('/').next().unwrap_or_default();
        let head = encode(&format!("{owner}:{}", pr.head));
        let (status, body) = call(http.get(format!("{url}?state=open&head={head}"))).await?;
        if let Some(found) = body.as_array().and_then(|list| list.first()) {
            if status.is_success() {
                return read_pr(found).ok_or_else(|| "GitHub 的回复里没有 PR 的地址".to_string());
            }
        }
    }
    Err(match status.as_u16() {
        401 => "GitHub 不认这个令牌（401）。请用户检查 GITHUB_TOKEN。".into(),
        403 => format!("GitHub 拒绝了（403）：{said}。令牌可能没有这个仓库的写权限。"),
        404 => format!(
            "GitHub 上找不到仓库 {}（404）。仓库名不对，或者令牌看不到它。",
            pr.repo
        ),
        code => format!("GitHub 没有开成 PR（{code}）：{said}"),
    })
}

/// 放进网址查询参数里：字母、数字和 `-_.~` 之外的都转成 `%XX`。
fn encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn read_pr(body: &Value) -> Option<PullRequest> {
    Some(PullRequest {
        url: body["html_url"].as_str()?.to_string(),
        number: body["number"].as_u64()?,
    })
}

/// GitHub 报错时说的话：`message` 加上 `errors` 里每一条的说明。
fn explain(body: &Value) -> String {
    let mut said = body["message"].as_str().unwrap_or("没有说原因").to_string();
    for error in body["errors"].as_array().into_iter().flatten() {
        let detail = error["message"].as_str().or_else(|| error.as_str());
        if let Some(detail) = detail {
            said.push_str(&format!("；{detail}"));
        }
    }
    said.chars().take(500).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_is_read_from_the_usual_remote_forms() {
        for remote in [
            "https://github.com/liziji555/superagent.git",
            "https://github.com/liziji555/superagent",
            "https://user:secret@github.com/liziji555/superagent.git/",
            "git@github.com:liziji555/superagent.git",
            "ssh://git@github.com/liziji555/superagent.git",
            "ssh://git@github.com:22/liziji555/superagent",
        ] {
            assert_eq!(
                parse_repo(remote).as_deref(),
                Some("liziji555/superagent"),
                "{remote}"
            );
        }
        for remote in [
            "/home/me/origin.git",
            r"C:\repos\origin.git",
            "../origin",
            "https://github.com/only-owner",
            "https://gitlab.com/group/sub/project.git",
            "",
        ] {
            assert_eq!(parse_repo(remote), None, "{remote}");
        }
    }

    #[test]
    fn credentials_in_a_remote_are_not_shown() {
        assert_eq!(
            redact("https://user:secret@github.com/o/r.git"),
            "https://github.com/o/r.git"
        );
        assert_eq!(redact("git@github.com:o/r.git"), "git@github.com:o/r.git");
        assert_eq!(redact("/srv/origin.git"), "/srv/origin.git");
        assert_eq!(encode("me:sa/task-1 x"), "me%3Asa%2Ftask-1%20x");
    }

    /// 假的 GitHub：开 PR 一律回 `status` 和 `body`；查 PR 回一个已有的，并记下查询参数。
    async fn fake(
        status: u16,
        body: Value,
    ) -> (GitHub, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use axum::extract::RawQuery;
        use axum::http::StatusCode;
        use axum::routing::post;
        let queries = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
        let seen = queries.clone();
        let app = axum::Router::new().route(
            "/repos/me/proj/pulls",
            post(move || async move { (StatusCode::from_u16(status).unwrap(), axum::Json(body)) })
                .get(move |RawQuery(query): RawQuery| async move {
                    seen.lock().unwrap().push(query.unwrap_or_default());
                    axum::Json(
                        json!([{ "html_url": "https://github.test/me/proj/pull/3", "number": 3 }]),
                    )
                }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let github = GitHub {
            api_base: format!("http://{address}/"),
            token: Some("t0ken".into()),
            repo: None,
        };
        (github, queries)
    }

    const PR: NewPr<'static> = NewPr {
        repo: "me/proj",
        title: "标题",
        body: "",
        head: "sa/task-1",
        base: "main",
    };

    /// 上次开出来了但没来得及记下：再开时 GitHub 说已经有了，就把那一个找回来。
    #[tokio::test]
    async fn an_existing_pr_for_the_branch_is_adopted() {
        let said = json!({
            "message": "Validation Failed",
            "errors": [{ "message": "A pull request already exists for me:sa/task-1." }]
        });
        let (github, queries) = fake(422, said).await;
        let found = open_pr(&github, PR, &CancellationToken::new()).await;
        let expected = PullRequest {
            url: "https://github.test/me/proj/pull/3".into(),
            number: 3,
        };
        assert_eq!(found, Ok(expected));
        assert_eq!(
            *queries.lock().unwrap(),
            ["state=open&head=me%3Asa%2Ftask-1"]
        );
    }

    #[tokio::test]
    async fn failures_say_what_went_wrong_without_the_token() {
        let (github, queries) = fake(401, json!({ "message": "Bad credentials" })).await;
        let error = open_pr(&github, PR, &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.contains("401") && !error.contains("t0ken"), "{error}");

        let said = json!({ "message": "Validation Failed", "errors": [{ "message": "No commits between main and sa/task-1" }] });
        let (github, _) = fake(422, said).await;
        let error = open_pr(&github, PR, &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.contains("No commits between"), "{error}");
        assert!(queries.lock().unwrap().is_empty());

        let no_token = GitHub::default();
        let error = open_pr(&no_token, PR, &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error, NO_TOKEN);
    }
}
