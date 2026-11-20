import { useState, type KeyboardEvent } from "react";

interface Props {
  /** 不能发消息的原因；能发时为空。 */
  disabledReason: string | null;
  /** Bot 是否正在回复。 */
  replying: boolean;
  onSend(content: string): Promise<void>;
  onStop(): Promise<void>;
}

export function Composer({ disabledReason, replying, onSend, onStop }: Props) {
  const [text, setText] = useState("");
  const [sending, setSending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const canSend = !disabledReason && !sending && text.trim() !== "";

  const run = async (action: () => Promise<void>, onDone?: () => void) => {
    setError(null);
    try {
      await action();
      onDone?.();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    }
  };

  const send = async () => {
    if (!canSend) return;
    setSending(true);
    // 发出去之后才清空：失败的话写好的内容还在。
    await run(
      () => onSend(text),
      () => setText(""),
    );
    setSending(false);
  };

  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    // 输入法选字时的回车不算发送。
    if (event.key !== "Enter" || event.shiftKey || event.nativeEvent.isComposing) return;
    event.preventDefault();
    void send();
  };

  return (
    <footer className="sa-composer">
      {error && (
        <p className="sa-composer-error" role="alert">
          {error}
        </p>
      )}
      <div className="sa-composer-row">
        <textarea
          rows={2}
          value={text}
          disabled={disabledReason !== null}
          aria-label="输入消息"
          placeholder={disabledReason ?? "发消息给 Bot。回车发送，Shift+回车换行"}
          onChange={(event) => setText(event.target.value)}
          onKeyDown={onKeyDown}
        />
        <div className="sa-composer-actions">
          {replying && (
            <button type="button" className="sa-button sa-button-quiet" onClick={() => void run(onStop)}>
              停止
            </button>
          )}
          <button
            type="button"
            className="sa-button"
            disabled={!canSend}
            onClick={() => void send()}
          >
            {sending ? "发送中…" : "发送"}
          </button>
        </div>
      </div>
    </footer>
  );
}
