import type { BotId, Message, MessageId } from "./types";

/** 一次 Bot 之间的交接：谁用 `ask_bot` 找了谁，对方接手后的那条回复是哪条。 */
export interface Handoff {
  id: string;
  /** 交接那句话。 */
  from: MessageId;
  /** 接手的 Bot 交接之后开口的第一条回复。 */
  to: MessageId;
  fromBot: BotId;
  toBot: BotId;
  /** 接手的 Bot 还在做。 */
  live: boolean;
  /** 交接那句话是什么时候说的，用来判断是不是刚发生的。 */
  at: number;
}

/**
 * 主线上的交接连线：Bot 用 `ask_bot` 点名别的 Bot 的那句话，连到对方之后开口的第一条回复。
 * 对方接手时那条回复和交接那句是同一手（`hop` 相同）；在这之前又有人点名了它，后面的回复就算那次的。
 * 对方还没开口（排着队）或者没接这次交接的不画。
 */
export function handoffsOf(messages: readonly Message[]): Handoff[] {
  const handoffs: Handoff[] = [];
  messages.forEach((message, index) => {
    const { sender } = message;
    if (message.kind !== "chat" || sender.kind !== "bot") return;
    for (const toBot of new Set(message.mentions)) {
      if (toBot === sender.bot_id) continue;
      let reply: Message | undefined;
      for (const later of messages.slice(index + 1)) {
        if (later.kind !== "chat") continue;
        if (later.sender.kind === "bot" && later.sender.bot_id === toBot && later.hop === message.hop) {
          reply = later;
          break;
        }
        if (later.mentions.includes(toBot)) break;
      }
      if (!reply) continue;
      handoffs.push({
        id: `${message.id}>${reply.id}`,
        from: message.id,
        to: reply.id,
        fromBot: sender.bot_id,
        toBot,
        live: reply.state === "streaming",
        at: message.created_at,
      });
    }
  });
  return handoffs;
}
