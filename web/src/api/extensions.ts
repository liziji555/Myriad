import type { McpServerInfo, SkillInfo } from "./types";

/** 扩展热重载完成后广播，让 App 同步刷新 `/` 技能命令列表。 */
export const EXTENSIONS_CHANGED = "sa:extensions-changed";

const STATES: Record<McpServerInfo["state"], string> = {
  connecting: "正在连接",
  ready: "已连上",
  failed: "没连上",
};

/** 一个 MCP 服务现在怎么样，一句话。 */
export function mcpStatus(server: McpServerInfo): string {
  if (server.state === "ready") return `${STATES.ready} · ${server.tools.length} 个工具`;
  return STATES[server.state];
}

/** 状态点的颜色：连上了是绿的，没连上是灰的，正在连是黄的。 */
export function mcpDot(server: McpServerInfo): "idle" | "warn" | "off" {
  if (server.state === "ready") return "idle";
  return server.state === "connecting" ? "warn" : "off";
}

/** 技能给谁用。 */
export function skillAudience(skill: SkillInfo): string {
  return skill.bots.length === 0 ? "所有 Bot" : skill.bots.join("、");
}
