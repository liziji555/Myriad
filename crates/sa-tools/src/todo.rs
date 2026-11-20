use crate::{parse_input, Tool, ToolCtx, ToolError, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

/// 一份清单最多这么多项。
const MAX_ITEMS: usize = 50;

/// 待办清单：模型把要做的几步列出来，做一步勾一步。
///
/// 清单不另外存：每次调用都交出完整的清单，它留在对话记录里，人在时间线上看得到进度。
pub struct TodoTool;

#[derive(Deserialize)]
struct Input {
    items: Vec<Item>,
}

#[derive(Deserialize)]
struct Item {
    text: String,
    status: Status,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Status {
    Pending,
    InProgress,
    Done,
}

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &'static str {
        "todo"
    }

    fn description(&self) -> String {
        "记下或更新这件事的待办清单。事情要分三步以上时先列清单，开始做某一步时把它标成 in_progress，\
         做完马上标成 done。每次都要给出完整的清单（不是只给改动的那一项），同一时间只能有一项 in_progress。\
         一两步就能做完的小事不用列。"
            .into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "description": "完整的清单，按要做的顺序排",
                    "items": {
                        "type": "object",
                        "properties": {
                            "text": { "type": "string", "description": "这一步要做什么，一句话" },
                            "status": { "type": "string", "enum": ["pending", "in_progress", "done"] }
                        },
                        "required": ["text", "status"]
                    }
                }
            },
            "required": ["items"]
        })
    }

    fn title(&self, input: &Value) -> String {
        let items = input["items"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        let done = items.iter().filter(|i| i["status"] == "done").count();
        let doing = items
            .iter()
            .find(|i| i["status"] == "in_progress")
            .and_then(|i| i["text"].as_str())
            .map(|text| {
                let short: String = text.chars().take(40).collect();
                format!("，正在做：{short}")
            })
            .unwrap_or_default();
        format!("待办 {done}/{} 完成{doing}", items.len())
    }

    /// 只是记一笔，不碰文件也不执行东西。
    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, _ctx: &ToolCtx) -> ToolResult {
        let input: Input = parse_input(input)?;
        if input.items.len() > MAX_ITEMS {
            return Err(ToolError(format!(
                "清单最多 {MAX_ITEMS} 项，现在有 {} 项。把细碎的几步合并一下。",
                input.items.len()
            )));
        }
        if input.items.iter().any(|item| item.text.trim().is_empty()) {
            return Err(ToolError::new(
                "清单里有一项是空的，每一项都要写清楚做什么。",
            ));
        }
        let doing = input
            .items
            .iter()
            .filter(|item| item.status == Status::InProgress)
            .count();
        if doing > 1 {
            return Err(ToolError(format!(
                "同一时间只能有一项 in_progress，现在有 {doing} 项。"
            )));
        }
        if input.items.is_empty() {
            return Ok("待办清单已清空。".into());
        }
        let mut out = String::from("待办清单已更新：\n");
        for item in &input.items {
            let mark = match item.status {
                Status::Pending => "[ ]",
                Status::InProgress => "[~]",
                Status::Done => "[x]",
            };
            out.push_str(&format!("{mark} {}\n", item.text.trim()));
        }
        let left = input
            .items
            .iter()
            .filter(|item| item.status != Status::Done)
            .count();
        out.push_str(&if left == 0 {
            "全部做完了。".to_string()
        } else {
            format!("还有 {left} 项没做完。")
        });
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Fence, Permission};

    fn ctx() -> (tempfile::TempDir, ToolCtx) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolCtx::new(Fence::new(dir.path(), &[]).unwrap());
        (dir, ctx)
    }

    #[tokio::test]
    async fn renders_the_list_and_needs_no_approval() {
        let (_dir, ctx) = ctx();
        let input = json!({ "items": [
            { "text": "读代码", "status": "done" },
            { "text": " 改函数 ", "status": "in_progress" },
            { "text": "跑测试", "status": "pending" }
        ]});
        assert_eq!(TodoTool.title(&input), "待办 1/3 完成，正在做： 改函数 ");
        assert_eq!(
            TodoTool.check_permission(&input, &ctx).await,
            Permission::Allow
        );
        assert_eq!(
            TodoTool.call(input, &ctx).await.unwrap(),
            "待办清单已更新：\n[x] 读代码\n[~] 改函数\n[ ] 跑测试\n还有 2 项没做完。"
        );

        let done = json!({ "items": [{ "text": "读代码", "status": "done" }] });
        assert_eq!(TodoTool.title(&done), "待办 1/1 完成");
        assert!(TodoTool
            .call(done, &ctx)
            .await
            .unwrap()
            .ends_with("全部做完了。"));
        assert_eq!(
            TodoTool.call(json!({ "items": [] }), &ctx).await.unwrap(),
            "待办清单已清空。"
        );
    }

    #[tokio::test]
    async fn rejects_lists_the_model_should_fix() {
        let (_dir, ctx) = ctx();
        let two = json!({ "items": [
            { "text": "一", "status": "in_progress" },
            { "text": "二", "status": "in_progress" }
        ]});
        let error = TodoTool.call(two, &ctx).await.unwrap_err();
        assert!(error.0.contains("只能有一项 in_progress"), "{}", error.0);

        let blank = json!({ "items": [{ "text": "  ", "status": "pending" }] });
        assert!(TodoTool
            .call(blank, &ctx)
            .await
            .unwrap_err()
            .0
            .contains("空的"));

        let unknown = json!({ "items": [{ "text": "一", "status": "later" }] });
        assert!(TodoTool.call(unknown, &ctx).await.is_err());

        let items: Vec<_> = (0..=MAX_ITEMS)
            .map(|i| json!({ "text": format!("第 {i} 步"), "status": "pending" }))
            .collect();
        let error = TodoTool
            .call(json!({ "items": items }), &ctx)
            .await
            .unwrap_err();
        assert!(error.0.contains("最多 50 项"), "{}", error.0);
        // 参数不对时标题也要给得出来。
        assert_eq!(TodoTool.title(&json!({})), "待办 0/0 完成");
    }
}
