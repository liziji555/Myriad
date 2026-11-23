import type { BotId, Discussion, Message, MessageId } from "./types";

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
 * 对方排着队时又被别的 Bot 找，它开口的那条回复同时接下这几次交接。
 * 对方开口之前人又点了它的名（或者房间不是普通方式时人说了没点名的话，大家重新排队），
 * 后面的回复算回答人的，这次交接不画；对方还没开口（排着队）的也先不画。
 * 对方自己用 `ask_bot` 找别人的那句不算它的回复。
 */
export function handoffsOf(messages: readonly Message[], discussion: Discussion = "normal"): Handoff[] {
  const handoffs: Handoff[] = [];
  messages.forEach((message, index) => {
    const { sender } = message;
    if (message.kind !== "chat" || sender.kind !== "bot") return;
    for (const toBot of new Set(message.mentions)) {
      if (toBot === sender.bot_id) continue;
      let reply: Message | undefined;
      for (const later of messages.slice(index + 1)) {
        if (later.kind !== "chat") continue;
        if (later.sender.kind === "bot") {
          if (later.sender.bot_id === toBot && later.mentions.length === 0) {
            reply = later;
            break;
          }
          continue;
        }
        if (later.mentions.includes(toBot)) break;
        if (discussion !== "normal" && later.mentions.length === 0) break;
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
