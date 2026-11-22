import { useEffect, useLayoutEffect, useRef, useState, type KeyboardEvent } from "react";
import { loadDraft, saveDraft, withExample } from "../api/drafts";
import { groupCommands, matchCommands, slashQuery, type SlashCommand } from "../api/slash";
import type { Bot } from "../api/types";
import { BotAvatar } from "./BotAvatar";
import { Icon } from "./Icon";

/** 输入框最高长到这么高，再多就在里面滚。 */
const MAX_INPUT_PX = 220;

interface Props {
  /** 哪个房间的输入框。写了一半的话按房间记着。 */
  roomId: string;
  /** 别处（空房间里的示例）要填进来的一句话。填完之后调用 `onPrefilled`，同一句不会填两次。 */
  prefill: string | null;
  onPrefilled(): void;
  /** 输入 `/` 时列出来的命令。 */
  commands: readonly SlashCommand[];
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

export function Composer({
  roomId,
  prefill,
  onPrefilled,
  commands,
  disabledReason,
  replying,
  members,
  onSend,
  onStop,
}: Props) {
  const input = useRef<HTMLTextAreaElement>(null);
  const [caret, setCaret] = useState(0);
  const [picked, setPicked] = useState(0);
  const [dismissed, setDismissed] = useState(false);
  const [text, setDraft] = useState(() => loadDraft(roomId));
  /** 改输入框里的内容，同时记成这个房间的草稿。 */
  const setText = (next: string) => {
    setDraft(next);
    saveDraft(roomId, next);
  };
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

  // 输入框跟着内容长高。
  useLayoutEffect(() => {
    const el = input.current;
    if (!el) return;
    el.style.height = "auto";
    const wanted = el.scrollHeight;
    el.style.height = `${Math.min(wanted, MAX_INPUT_PX)}px`;
    // 没到顶之前不出滚动条。
    el.style.overflowY = wanted > MAX_INPUT_PX ? "auto" : "hidden";
  }, [text]);

  useEffect(() => {
    if (prefill === null) return;
    const next = withExample(text, prefill);
    setText(next);
    setCaret(next.length);
    onPrefilled();
    requestAnimationFrame(() => {
      input.current?.focus();
      input.current?.setSelectionRange(next.length, next.length);
    });
    // 只跟着要填的那句话走。
  }, [prefill]);

  const slash = dismissed ? null : slashQuery(text);
  const commandOptions = slash === null ? [] : matchCommands(commands, slash);
  const mention = dismissed || slash !== null ? null : mentionAt(text, caret);
  const options = mention
    ? members.filter((bot) => bot.name.toLowerCase().startsWith(mention.query.toLowerCase()))
    : [];
  const count = slash === null ? options.length : commandOptions.length;
  const index = Math.min(picked, count - 1);
  const choice = slash === null ? options[index] : undefined;
  const command = slash === null ? undefined : commandOptions[index];
  /** 菜单开着但一个都对不上：告诉人没有，不是悄悄消失。 */
  const noMatch =
    count === 0 &&
    (slash !== null ||
      // 邮箱、包名里的 `@` 不是在点名：只有单独起头的 `@` 才提示。
      (mention !== null && mention.query !== "" && (mention.start === 0 || /\s/.test(text[mention.start - 1]))));

  const runCommand = (picked: SlashCommand) => {
    setText("");
    setPicked(0);
    picked.run();
  };

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
    if (count > 0 && !event.nativeEvent.isComposing) {
      const step = event.key === "ArrowDown" ? 1 : event.key === "ArrowUp" ? -1 : 0;
      if (step !== 0) {
        event.preventDefault();
        setPicked((index + step + count) % count);
        return;
      }
      // 命令只认单独的回车：Shift+回车照常换行，Tab 照常挪走焦点。点名的菜单回车和 Tab 都是补全。
      const plainEnter = event.key === "Enter" && !event.shiftKey;
      if (command ? plainEnter : event.key === "Enter" || event.key === "Tab") {
        event.preventDefault();
        if (command) runCommand(command);
        else if (choice) complete(choice);
        return;
      }
    }
    if ((count > 0 || noMatch) && event.key === "Escape" && !event.nativeEvent.isComposing) {
      event.preventDefault();
      setDismissed(true);
      return;
    }
    // 输入法选字时的回车不算发送。
    if (event.key !== "Enter" || event.shiftKey || event.nativeEvent.isComposing) return;
    event.preventDefault();
    void send();
  };

  return (
    <footer className="sa-composer">
      <div className="sa-composer-inner">
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
                <BotAvatar id={bot.id} name={bot.name} />
                <span className="sa-mention-name">@{bot.name}</span>
                <span className="sa-mention-role">{bot.persona.split("\n")[0]}</span>
              </li>
            ))}
          </ul>
        )}
        {command && (
          <div className="sa-mention-menu" role="listbox" aria-label="命令">
            {groupCommands(commandOptions).map(({ group, items }) => (
              <div key={group} role="group" aria-label={group}>
                <p className="sa-menu-group" aria-hidden="true">
                  {group}
                </p>
                {items.map((item) => (
                  <div
                    key={item.id}
                    className="sa-menu-item"
                    role="option"
                    aria-selected={item.id === command.id}
                    onMouseDown={(event) => {
                      event.preventDefault();
                      runCommand(item);
                    }}
                  >
                    {item.title}
                  </div>
                ))}
              </div>
            ))}
          </div>
        )}
        {noMatch && (
          <p className="sa-mention-menu sa-menu-empty" role="status">
            {slash === null ? "没有匹配的 Bot" : "没有匹配的命令"}，按 Esc 关掉
          </p>
        )}
        <div className="sa-composer-box" data-disabled={disabledReason !== null}>
          <textarea
            ref={input}
            rows={1}
            value={text}
            disabled={disabledReason !== null}
            aria-label="输入消息"
            placeholder={disabledReason ?? "说点什么，或者交代一件事…"}
            onChange={(event) => {
              setText(event.target.value);
              setCaret(event.target.selectionStart);
              setDismissed(false);
              setPicked(0);
            }}
            onSelect={(event) => setCaret(event.currentTarget.selectionStart)}
            onKeyDown={onKeyDown}
          />
          <div className="sa-composer-bar">
            <p className="sa-composer-hint">
              <kbd className="sa-kbd">@</kbd> 点名 <kbd className="sa-kbd">/</kbd> 命令{" "}
              <kbd className="sa-kbd">Shift</kbd>
              <kbd className="sa-kbd">回车</kbd> 换行
            </p>
            <div className="sa-composer-actions">
              {replying && (
                <button type="button" className="sa-stop" onClick={() => void run(onStop)}>
                  <Icon name="stop" size={14} />
                  停止
                </button>
              )}
              <button
                type="button"
                className="sa-send"
                disabled={!canSend}
                aria-label={sending ? "发送中…" : "发送"}
                title={sending ? "发送中…" : "发送（回车）"}
                data-sending={sending}
                onClick={() => void send()}
              >
                <Icon name="send" />
              </button>
            </div>
          </div>
        </div>
      </div>
    </footer>
  );
}
