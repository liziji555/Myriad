//! 测试用的假 harness：按苦乐自定义命令的格式说话，不调任何模型。
//!
//! 从标准输入读这一轮的话，看里面有没有这些词：
//! - 「写」：在当前目录写一个 `harness-<会话号>.txt`
//! - 「命令失败」：报一步失败的命令，但这一轮照样做完
//! - 「出错」：报错，退出码 1
//! - 「慢」：每一步之间停 1 秒，一共 20 秒（用来测停止和排队）
//! - 「子进程」：开一个睡 60 秒的子进程，把它的 pid 写进 `child.pid`（用来测停止时一起结束）
//! - 「后台」：开一个睡 60 秒、占着标准输出的子进程，自己马上退出，pid 写进 `bg.pid`

use serde_json::json;
use std::io::Read;
use std::time::Duration;

fn say(value: serde_json::Value) {
    println!("{value}");
}

fn main() {
    let mut prompt = String::new();
    let _ = std::io::stdin().read_to_string(&mut prompt);
    let session = std::env::var("SA_HARNESS_SESSION")
        .unwrap_or_else(|_| format!("fake-{}", std::process::id()));
    let resumed = std::env::var("SA_HARNESS_SESSION").is_ok();
    say(json!({"type": "session", "id": session}));
    let pause = if prompt.contains('慢') {
        Duration::from_secs(1)
    } else {
        Duration::ZERO
    };

    say(json!({"type": "step", "id": "1", "tool": "read", "title": "读 目录"}));
    let listed: Vec<String> = std::fs::read_dir(".")
        .map(|dir| {
            dir.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    say(json!({"type": "step_end", "id": "1", "ok": true, "output": listed.join("\n")}));

    if prompt.contains("子进程") {
        // 故意不等它：测试要看停止时苦乐能不能把它一起杀掉。
        #[allow(clippy::zombie_processes)]
        let child = std::process::Command::new(if cfg!(windows) { "ping" } else { "sleep" })
            .args(if cfg!(windows) {
                vec!["-n", "61", "127.0.0.1"]
            } else {
                vec!["60"]
            })
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("开不了子进程");
        std::fs::write("child.pid", child.id().to_string()).expect("写不了 child.pid");
    }
    if prompt.contains("后台") {
        // 故意不等它，也不关它的输出：测试要看 harness 退出后苦乐不会一直等这个管道。
        #[allow(clippy::zombie_processes)]
        let child = std::process::Command::new(if cfg!(windows) { "ping" } else { "sleep" })
            .args(if cfg!(windows) {
                vec!["-n", "61", "127.0.0.1"]
            } else {
                vec!["60"]
            })
            .spawn()
            .expect("开不了子进程");
        std::fs::write("bg.pid", child.id().to_string()).expect("写不了 bg.pid");
    }
    if prompt.contains('慢') {
        for step in 0..20 {
            let id = format!("slow{step}");
            say(
                json!({"type": "step", "id": id, "tool": "shell", "title": format!("等一下 {step}")}),
            );
            std::thread::sleep(pause);
            say(json!({"type": "step_end", "id": id, "ok": true, "output": ""}));
        }
    }
    if prompt.contains('写') {
        let name = format!("harness-{session}.txt");
        say(json!({"type": "step", "id": "2", "tool": "write", "title": format!("写 {name}")}));
        std::fs::write(&name, format!("{}\n", prompt.trim())).expect("写不了文件");
        say(json!({"type": "step_end", "id": "2", "ok": true, "output": "写好了"}));
    }
    if prompt.contains("命令失败") {
        say(json!({"type": "step", "id": "3", "tool": "shell", "title": "cargo test"}));
        say(json!({"type": "step_end", "id": "3", "ok": false, "output": "1 个测试没过"}));
    }
    if prompt.contains("出错") {
        say(json!({"type": "error", "message": "假 harness 按要求出错了"}));
        std::process::exit(1);
    }
    let again = if resumed {
        "（接着上次的会话）"
    } else {
        ""
    };
    say(json!({"type": "text", "text": format!("假 harness 做完了{again}。")}));
}
