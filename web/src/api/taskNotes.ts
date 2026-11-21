/** 任务卡开头的那句话（服务端写的）。别的任务消息是人对任务做的决定：验收、打回、取消。 */
const OPENED = "开了任务「";

/** 这条任务消息是不是「人对任务做了决定」的那一句；是的话返回那句话，开任务的卡片返回 `null`。 */
export function taskDecision(content: string): string | null {
  return content.startsWith(OPENED) ? null : content;
}
