/** 一条消息在「同一个任务的一串」里排第几：开头、中间、结尾。不在串里的没有。 */
export type ChainPart = "start" | "mid" | "end";

interface Linkable {
  id: string;
  task_id?: string | null;
}

/**
 * 挨着的几条消息说的是同一个任务（开任务 → 做的过程 → 交结果）时，把它们连成一串。
 * 只传画出来的消息：中间隔了别的话就断开。
 */
export function chainParts(shown: readonly Linkable[]): Map<string, ChainPart> {
  const parts = new Map<string, ChainPart>();
  shown.forEach((message, index) => {
    const task = message.task_id;
    if (!task) return;
    const before = shown[index - 1]?.task_id === task;
    const after = shown[index + 1]?.task_id === task;
    if (before && after) parts.set(message.id, "mid");
    else if (after) parts.set(message.id, "start");
    else if (before) parts.set(message.id, "end");
  });
  return parts;
}
