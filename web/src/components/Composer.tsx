import { useRef, useState, type KeyboardEvent } from "react";
import type { Bot } from "../api/types";

interface Props {
  /** 不能发消息的原因；能发时为空。 */
  disabledReason: string | null;
  /** Bot 是否正在回复。 */
  replying: boolean;
  /** 房间里可以点名的 Bot。输入 `@` 时列出来。 */
  members: readonly Bot[];
  onSend(content: string): Promise<void>;
  onStop(): Promise<void>;
}

/** 光标前面正在输入的 `@名字`：返回 `@` 的位置和已经输入的那部分名字。 */
export function mentionAt(text: string, caret: number): { start: number; query: string } | null {
  const before = text.slice(0, caret);
  const start = before.lastIndexOf("@");
  if (start < 0) return null;
  const query = before.slice(start + 1);
  return /\s/.test(query) ? null : { start, query };
}

export function Composer({ disabledReason, replying, members, onSend, onStop }: Props) {
  const input = useRef<HTMLTextAreaElement>(null);
  const [caret, setCaret] = useState(0);
  const [picked, setPicked] = useState(0);
  const [dismissed, setDismissed] = useState(false);
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

  const mention = dismissed ? null : mentionAt(text, caret);
  const options = mention
    ? members.filter((bot) => bot.name.toLowerCase().startsWith(mention.query.toLowerCase()))
    : [];
  const choice = options[Math.min(picked, options.length - 1)];

  const complete = (bot: Bot) => {
    if (!mention) return;
    const next = `${text.slice(0, mention.start)}@${bot.name} ${text.slice(caret)}`;
    const position = mention.start + bot.name.length + 2;
    setText(next);
    setCaret(position);
    setPicked(0);
    requestAnimationFrame(() => {
      input.current?.focus();
      input.current?.setSelectionRange(position, position);
    });
  };

  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (choice && !event.nativeEvent.isComposing) {
      const step = event.key === "ArrowDown" ? 1 : event.key === "ArrowUp" ? -1 : 0;
      if (step !== 0) {
        event.preventDefault();
        setPicked((Math.min(picked, options.length - 1) + step + options.length) % options.length);
        return;
      }
      if (event.key === "Enter" || event.key === "Tab") {
        event.preventDefault();
        complete(choice);
        return;
      }
      if (event.key === "Escape") {
        event.preventDefault();
        setDismissed(true);
        return;
      }
    }
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
      {choice && (
        <ul className="sa-mention-menu" role="listbox" aria-label="点名一个 Bot">
          {options.map((bot) => (
            <li
              key={bot.id}
              role="option"
              aria-selected={bot.id === choice.id}
              // 用 mousedown：点菜单时输入框不失去焦点。
              onMouseDown={(event) => {
                event.preventDefault();
                complete(bot);
              }}
            >
              <span className="sa-mention-name">@{bot.name}</span>
              <span className="sa-mention-role">{bot.persona.split("\n")[0]}</span>
            </li>
          ))}
        </ul>
      )}
      <div className="sa-composer-row">
        <textarea
          ref={input}
          rows={2}
          value={text}
          disabled={disabledReason !== null}
          aria-label="输入消息"
          placeholder={disabledReason ?? "发消息给 Bot。输入 @ 点名，回车发送，Shift+回车换行"}
          onChange={(event) => {
            setText(event.target.value);
            setCaret(event.target.selectionStart);
            setDismissed(false);
            setPicked(0);
          }}
          onSelect={(event) => setCaret(event.currentTarget.selectionStart)}
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
