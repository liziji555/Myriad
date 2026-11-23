import { useState, type KeyboardEvent } from "react";
import type { EditQueuedRequest, QueuedId, QueuedInput } from "../api/types";

interface Props {
  items: readonly QueuedInput[];
  /** 改字或挪一位，做成了返回 `true`。出错由外面显示。 */
  onEdit(id: QueuedId, edit: EditQueuedRequest): Promise<boolean>;
  onCancel(id: QueuedId): Promise<boolean>;
}

/** 待发队列：输入框上面一排，排在前面的先发。已经发出去的就不在这里了，改不了也取消不了。 */
export function QueueList({ items, onEdit, onCancel }: Props) {
  const [editing, setEditing] = useState<{ id: QueuedId; text: string } | null>(null);
  if (items.length === 0) return null;
  // 正在改的那句发出去了：编辑框跟着消失。
  const open = editing && items.some((item) => item.id === editing.id) ? editing : null;

  const save = async () => {
    if (!open || open.text.trim() === "") return;
    // 没改成（比如太长）：编辑框留着，改好的字不丢。
    if (await onEdit(open.id, { content: open.text })) setEditing(null);
  };
  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.nativeEvent.isComposing) return;
    if (event.key === "Escape") {
      event.preventDefault();
      setEditing(null);
    } else if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      void save();
    }
  };

  return (
    <section className="sa-queue" aria-label="待发队列">
      <p className="sa-queue-title">
        待发 {items.length} 句 · 正在说的 Bot 都说完了按顺序发，停止不会清掉
      </p>
      <ol className="sa-queue-list">
        {items.map((item, index) => (
          <li key={item.id} className="sa-queue-item">
            <span className="sa-queue-index">{index + 1}</span>
            {open?.id === item.id ? (
              <textarea
                className="sa-queue-edit"
                aria-label="改这一句"
                rows={2}
                autoFocus
                value={open.text}
                onChange={(event) => setEditing({ id: item.id, text: event.target.value })}
                onKeyDown={onKeyDown}
              />
            ) : (
              <span className="sa-queue-text">{item.content}</span>
            )}
            <span className="sa-queue-actions">
              {open?.id === item.id ? (
                <>
                  <button type="button" disabled={open.text.trim() === ""} onClick={() => void save()}>
                    保存
                  </button>
                  <button type="button" onClick={() => setEditing(null)}>
                    不改了
                  </button>
                </>
              ) : (
                <>
                  <button
                    type="button"
                    aria-label="往前挪"
                    title="往前挪"
                    disabled={index === 0}
                    onClick={() => void onEdit(item.id, { offset: -1 })}
                  >
                    ↑
                  </button>
                  <button
                    type="button"
                    aria-label="往后挪"
                    title="往后挪"
                    disabled={index === items.length - 1}
                    onClick={() => void onEdit(item.id, { offset: 1 })}
                  >
                    ↓
                  </button>
                  <button type="button" onClick={() => setEditing({ id: item.id, text: item.content })}>
                    改
                  </button>
                  <button type="button" onClick={() => void onCancel(item.id)}>
                    取消
                  </button>
                </>
              )}
            </span>
          </li>
        ))}
      </ol>
    </section>
  );
}
