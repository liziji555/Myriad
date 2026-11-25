use crate::{ApprovalDecision, BotId, Discussion, Harness, Message, ModelTier, RoomId, Run, Task};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Health {
    pub status: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CreateRoomRequest {
    pub name: String,
}

/// 所有失败响应的统一形状。`code` 给程序判断，`message` 给人看。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct PostMessageRequest {
    pub content: String,
}

/// 改待发队列里的一句：改字、往前或往后挪一位。都不给就什么也不变。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct EditQueuedRequest {
    #[serde(default)]
    #[ts(optional)]
    pub content: Option<String>,
    /// `-1` 往前挪一位，`1` 往后挪一位。
    #[serde(default)]
    #[ts(optional)]
    pub offset: Option<i32>,
}

/// 一页消息，按 `seq` 升序。`has_more` 表示更早的还有。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct MessagePage {
    pub messages: Vec<Message>,
    pub has_more: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct StopResponse {
    /// 是否真的有一条正在写的回复被停下。
    pub stopped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DecideApprovalRequest {
    pub decision: ApprovalDecision,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CreateBotRequest {
    pub name: String,
    /// 不给就是 `mid`。
    #[serde(default)]
    #[ts(optional)]
    pub tier: Option<ModelTier>,
    #[serde(default)]
    #[ts(optional)]
    pub persona: Option<String>,
    /// 不给就是内置。
    #[serde(default)]
    #[ts(optional)]
    pub harness: Option<Harness>,
    #[serde(default)]
    #[ts(optional)]
    pub harness_command: Option<String>,
}

/// 只改给出的字段。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct UpdateBotRequest {
    #[serde(default)]
    #[ts(optional)]
    pub name: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    pub tier: Option<ModelTier>,
    #[serde(default)]
    #[ts(optional)]
    pub persona: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    pub enabled: Option<bool>,
    #[serde(default)]
    #[ts(optional)]
    pub harness: Option<Harness>,
    #[serde(default)]
    #[ts(optional)]
    pub harness_command: Option<String>,
}

/// 只改给出的字段。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct UpdateRoomRequest {
    #[serde(default)]
    #[ts(optional)]
    pub name: Option<String>,
    /// 新的入口 Bot，必须已经是房间成员。
    #[serde(default)]
    #[ts(optional)]
    pub host_bot_id: Option<BotId>,
    #[serde(default)]
    #[ts(optional)]
    pub discussion: Option<Discussion>,
    /// 轮流讨论转几圈，1 到 3。
    #[serde(default)]
    #[ts(optional)]
    pub rounds: Option<u32>,
    /// 用哪个共享目录当工作目录。空字符串表示改回服务自己的工作目录。
    #[serde(default)]
    #[ts(optional)]
    pub share: Option<String>,
}

/// 人开一个任务。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CreateTaskRequest {
    pub title: String,
    /// 要做什么。
    pub scope: String,
    #[serde(default)]
    #[ts(optional)]
    pub acceptance: Option<String>,
    /// 派给谁。不给就先放着（草稿），之后再派。
    #[serde(default)]
    #[ts(optional)]
    pub owner_bot_id: Option<BotId>,
}

/// 对任务做一个动作（继续、验收、打回、停下、取消）时附的话。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TaskActionRequest {
    /// 给负责的 Bot 的话：补充的信息、打回的原因。
    #[serde(default)]
    #[ts(optional)]
    pub note: Option<String>,
    /// 「继续」时顺便换一个负责人。
    #[serde(default)]
    #[ts(optional)]
    pub owner_bot_id: Option<BotId>,
}

/// 一个任务和它的执行历史。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TaskDetail {
    pub task: Task,
    /// 按开始时间升序。
    pub runs: Vec<Run>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CreateRoutineRequest {
    pub room_id: RoomId,
    /// 不给就是房间的入口 Bot。
    #[serde(default)]
    #[ts(optional)]
    pub bot_id: Option<BotId>,
    pub title: String,
    pub prompt: String,
    pub schedule: String,
}

/// 只改给出的字段。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct UpdateRoutineRequest {
    #[serde(default)]
    #[ts(optional)]
    pub bot_id: Option<BotId>,
    #[serde(default)]
    #[ts(optional)]
    pub title: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    pub prompt: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    pub schedule: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    pub enabled: Option<bool>,
}

/// 把一个 GitHub 仓库连到房间。Telegram 聊天用配对码连。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CreateLinkRequest {
    pub room_id: RoomId,
    /// `owner/name`。
    pub repo: String,
}

/// 一个技能：写在文件里的做事套路。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    /// 给哪些 Bot 用（Bot 的名字）。空着就是谁都能用。
    pub bots: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum McpState {
    /// 正在启动、握手。
    Connecting,
    /// 连上了，工具能用。
    Ready,
    /// 没连上。
    Failed,
}

/// 一个外部 MCP 服务现在怎么样。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct McpServerInfo {
    pub name: String,
    pub state: McpState,
    /// 它提供的工具（服务自己的工具名）。
    pub tools: Vec<String>,
    /// 没连上的原因。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub error: Option<String>,
}

/// Bot 多出来的本事：技能和外部 MCP 服务的工具。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Extensions {
    pub skills: Vec<SkillInfo>,
    /// 技能目录里没认出来的文件和原因。
    pub skill_problems: Vec<String>,
    pub mcp: Vec<McpServerInfo>,
}

/// 模型服务说的是哪种接口。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ProviderKind {
    /// OpenAI 的 Chat Completions，以及兼容它的服务。
    OpenaiCompat,
    /// Anthropic 的 Messages，以及兼容它的中转。
    Anthropic,
}

/// 一个模型服务。密钥不往外给，只说有没有。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ModelProvider {
    pub name: String,
    pub kind: ProviderKind,
    pub base_url: String,
    pub has_key: bool,
}

/// 一个档位用哪个模型服务的哪个模型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TierChoice {
    pub provider: String,
    pub model: String,
    /// 是否开启思考模式。不写就用模型服务自己的默认值。
    pub thinking: Option<bool>,
    /// 上下文窗口（token）。不写就用模型服务报的大小。
    pub context_window: Option<u32>,
}

/// 现在接了哪些模型服务，三个档位各用哪个模型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ModelSettings {
    pub providers: Vec<ModelProvider>,
    pub cheap: TierChoice,
    pub mid: TierChoice,
    pub strong: TierChoice,
}

/// 要保存或者要测试的一个模型服务。`api_key` 不写表示沿用已经存着的那个。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ModelProviderInput {
    pub name: String,
    pub kind: ProviderKind,
    pub base_url: String,
    #[serde(default)]
    pub api_key: Option<String>,
}

/// 整份模型设置：没写进来的模型服务会被删掉。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SaveModelsRequest {
    pub providers: Vec<ModelProviderInput>,
    pub cheap: TierChoice,
    pub mid: TierChoice,
    pub strong: TierChoice,
}

/// 用这个模型服务的这个模型说一句话，看通不通。不保存。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TestModelRequest {
    pub provider: ModelProviderInput,
    pub model: String,
}

/// `message` 是给人看的：通了是模型回的话，没通是原因。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TestModelResult {
    pub ok: bool,
    pub message: String,
}

/// 模板里的一个 Bot：只有名字、档位、分工、是否启用。没有 ID、时间和任何密钥，换一台机器也能用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct BotTemplate {
    pub name: String,
    pub tier: ModelTier,
    #[serde(default)]
    pub persona: String,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "is_builtin")]
    pub harness: Harness,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub harness_command: String,
}

fn is_builtin(harness: &Harness) -> bool {
    *harness == Harness::Builtin
}

fn enabled_by_default() -> bool {
    true
}

/// 模板里的房间：叫什么、谁是入口、有哪些成员（都写 Bot 的名字）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct RoomTemplate {
    pub name: String,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub members: Vec<String>,
}

/// 一份团队模板：几个 Bot，可以带上它们所在的房间。导出成文件，在别处导入。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TeamTemplate {
    /// 模板格式的版本，现在是 1。
    pub version: u32,
    #[serde(default)]
    pub bots: Vec<BotTemplate>,
    #[serde(default)]
    pub room: Option<RoomTemplate>,
}

/// 一份模板写成的文件：建议的文件名和内容（TOML）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TemplateFile {
    pub name: String,
    pub text: String,
}

/// 导入模板：把模板文件的内容原样交上来。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ImportTemplateRequest {
    pub text: String,
    /// 已经有同名的 Bot 时，照模板改它的档位、分工、是否启用。不写就是不改。
    #[serde(default)]
    #[ts(optional)]
    pub update_existing: Option<bool>,
}

/// 导入的结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ImportReport {
    /// 新建的 Bot。
    pub created: Vec<crate::Bot>,
    /// 已经有同名的 Bot，没有动它们。
    pub skipped: Vec<String>,
    /// 已经有同名的 Bot，导入时说了要照模板改，改了的这些。
    #[serde(default)]
    pub updated: Vec<crate::Bot>,
    /// 模板里带了房间时新建的房间。
    pub room: Option<crate::Room>,
}

/// `sa share` 来服务这里取下一件要在共享目录里做的事。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharePoll {
    pub name: String,
    /// 这次运行的 `sa share` 的编号。换了编号就是另一次运行，接手同名的共享。
    pub session: String,
    /// 目录在那台电脑上的位置，给人看的。
    pub root: String,
    pub read_only: bool,
    /// 这边允许执行命令时用哪种命令行（`bash`、`PowerShell`）。没有就是不让执行命令。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    /// 交出去了、这边还在做的活。不在这里面又没交回的，就是丢了。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub running: Vec<u64>,
}

/// 交给 `sa share` 去做的一件事：在共享目录里调用一个工具，或者停下还在做的那件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShareCall {
    pub id: u64,
    pub tool: String,
    pub input: serde_json::Value,
}

/// `sa share` 交回来的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareReply {
    pub name: String,
    pub session: String,
    pub id: u64,
    pub ok: bool,
    pub text: String,
    /// 还没做完、先报一段进展（harness 的一步、说的一段话）。最后的结果不带它。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub progress: bool,
}
