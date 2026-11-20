//! 真的启动一个进程当 MCP 服务：`sa mcp demo-server`。

use sa_tools::mcp::{McpServer, McpSpec};
use serde_json::json;
use std::process::Command;
use tokio_util::sync::CancellationToken;

const SA: &str = env!("CARGO_BIN_EXE_sa");

fn demo(env: Vec<(String, String)>) -> McpSpec {
    McpSpec {
        name: "demo".into(),
        command: SA.into(),
        args: vec!["mcp".into(), "demo-server".into()],
        env,
        cwd: None,
        auto_approve: vec![],
    }
}

#[tokio::test]
async fn a_spawned_server_is_listed_called_and_started_again_after_it_exits() {
    let server = McpServer::spawned(demo(vec![]));
    let tools = server.connect().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(names, ["echo", "add", "fail", "wait", "quit"]);

    let never = CancellationToken::new();
    let sum = server.call("add", json!({ "a": 40, "b": 2 }), &never).await;
    assert_eq!(sum.unwrap(), "42");
    // 进程自己退出了：这一次失败，下一次调用时重新启动。
    assert!(server.call("quit", json!({}), &never).await.is_err());
    let back = server
        .call("echo", json!({ "text": "回来了" }), &never)
        .await;
    assert_eq!(back.unwrap(), "回来了");
    server.close().await;
}

#[test]
fn sa_mcp_test_reports_what_connects_and_what_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        let output = Command::new(SA)
            .arg("--data-dir")
            .arg(dir.path())
            .args(args)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout).into_owned()
            + &String::from_utf8_lossy(&output.stderr);
        (output.status.success(), text)
    };
    let (ok, text) = run(&["mcp", "list"]);
    assert!(ok && text.contains("还没有配置 MCP 服务"), "{text}");

    // 路径用单引号写：Windows 的反斜杠不用转义。
    let config = format!(
        "{}\n[mcp.demo]\ncommand = '{SA}'\nargs = [\"mcp\", \"demo-server\"]\n\n\
         [mcp.broken]\ncommand = \"sa-no-such-mcp-server\"\n\n\
         [mcp.off]\ncommand = \"whatever\"\nenabled = false\n",
        include_str!("../src/default-config.toml")
    );
    std::fs::write(dir.path().join("config.toml"), config).unwrap();

    let (ok, text) = run(&["mcp", "list"]);
    assert!(ok, "{text}");
    assert!(text.contains("[启用]  demo"), "{text}");
    assert!(text.contains("[停用]  off  whatever"), "{text}");

    let (ok, text) = run(&["mcp", "test", "demo"]);
    assert!(ok, "{text}");
    assert!(text.contains("✓ demo：连上了，5 个工具"), "{text}");
    assert!(text.contains("echo  把 text 原样说回来"), "{text}");

    // 有一个连不上：说清楚是哪个，退出码不是 0。
    let (ok, text) = run(&["mcp", "test"]);
    assert!(!ok, "{text}");
    assert!(
        text.contains("✓ demo") && text.contains("✗ broken：找不到命令"),
        "{text}"
    );
    let (ok, text) = run(&["mcp", "test", "off"]);
    assert!(!ok && text.contains("停用"), "{text}");
    let (ok, text) = run(&["mcp", "test", "nope"]);
    assert!(!ok && text.contains("没有叫「nope」"), "{text}");
}
