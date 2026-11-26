import { ApiRequestError } from "./client";
import type { CheckpointInfo, RewindReport } from "./types";

/** 任意入口完成检查点回滚后广播此事件，让时间线和文件面板同步刷新。 */
export const CHECKPOINTS_CHANGED = "sa:checkpoints-changed";

/** 按消息 ID（即检查点 ID）索引，方便时间线 O(1) 查找某条回复是否改动了文件。 */
export function checkpointsByMessage(list: readonly CheckpointInfo[]): Map<string, CheckpointInfo> {
  const map = new Map<string, CheckpointInfo>();
  for (const cp of list) {
    map.set(cp.id, cp);
  }
  return map;
}

/** 后端拒绝回滚且提示可通过 `force=true` 强制覆盖（外部编辑冲突）。 */
export function isRewindConflict(cause: unknown): boolean {
  const message = cause instanceof Error ? cause.message : String(cause);
  if (message.includes("force=true") || message.includes("被外部修改过")) {
    return true;
  }
  return cause instanceof ApiRequestError && cause.status === 409 && message.includes("冲突");
}

/** 把回滚报告格式化为简明中文摘要。 */
export function formatRewindSummary(report: RewindReport): string {
  const parts: string[] = [];
  if (report.restored.length > 0) parts.push(`恢复 ${report.restored.length} 个文件`);
  if (report.removed.length > 0) parts.push(`删除 ${report.removed.length} 个新建文件`);
  if (parts.length === 0) return "已回滚，没有需要变更的文件。";
  return `已回滚：${parts.join("，")}。`;
}
