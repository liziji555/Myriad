import type { ToolStep } from "./types";

/** 一步的结果默认只露出开头这么多行、这么多字，其余的点「查看完整」再看。 */
export const PREVIEW_LINES = 4;
export const PREVIEW_CHARS = 400;

/** 结果的开头几行；`cut` 表示后面还有没露出来的。 */
export function outputPreview(output: string): { text: string; cut: boolean } {
  const lines = output.split("\n");
  let text = lines.slice(0, PREVIEW_LINES).join("\n");
  if (text.length > PREVIEW_CHARS) text = text.slice(0, PREVIEW_CHARS);
  return { text, cut: text.length < output.length };
}

/** 一步用的是哪一类工具，决定过程行前面画什么图标。 */
export type StepIcon = "read" | "search" | "edit" | "command" | "git" | "web" | "bot" | "plug" | "tool";

/** 按工具名猜它是哪一类；不认识的给通用图标，不出错。 */
export function stepIcon(tool: string): StepIcon {
  const name = tool.toLowerCase();
  if (name.startsWith("mcp")) return "plug";
  if (/web|fetch|http|url/.test(name)) return "web";
  if (/git|push|(^|_)pr($|_)|pull_request/.test(name)) return "git";
  if (/grep|search|glob|find/.test(name)) return "search";
  if (/write|edit|patch|create/.test(name)) return "edit";
  if (/bash|shell|command|exec|run/.test(name)) return "command";
  if (/read|list|cat|view/.test(name)) return "read";
  if (/agent|spawn|handoff|delegate|message/.test(name)) return "bot";
  return "tool";
}

/** 一串步骤用到了哪几类工具，按第一次出现的先后，同一类只算一次。收起来的那一行用它画图标。 */
export function stepKinds(tools: readonly string[]): StepIcon[] {
  return [...new Set(tools.map(stepIcon))];
}

/** 一步现在怎么样了，给人看的说法。 */
export const STEP_LABEL: Record<ToolStep["state"], string> = {
  waiting: "等你批准",
  running: "进行中",
  ok: "完成",
  error: "失败",
  cancelled: "已停止",
  denied: "没有批准",
};
