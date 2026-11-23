import type { BotId, Message, MessageId } from "./types";

/** 一句插话：说的时候哪几个 Bot 正在回复（这句话进了它们的上下文），它们是不是还在做。 */
export interface Steer {
  botIds: BotId[];
  /** 其中还有 Bot 在做：它不会停下，下一次想之前会看到这句话。 */
  live: boolean;
}

/**
 * 哪些人说的话是插话：说的时候房间里有 Bot 正在回复。几个 Bot 同时在说时，
 * 没点名的话它们都看到；点了名的只给被点到的，被点到的不在说就不算插话。
 */
export function steersOf(messages: readonly Message[]): Map<MessageId, Steer> {
  const steers = new Map<MessageId, Steer>();
  // 每个 Bot 最近一条对话里的回复。
  const replies = new Map<BotId, Message>();
  for (const message of messages) {
    if (message.kind !== "chat") continue;
    if (message.sender.kind === "bot") {
      replies.set(message.sender.bot_id, message);
      continue;
    }
    const botIds: BotId[] = [];
    let live = false;
    for (const [botId, reply] of replies) {
      const streaming = reply.state === "streaming";
      // 那条回复在这句话之前就结束了：不是插话，是新的一轮。
      if (!streaming && reply.updated_at <= message.created_at) continue;
      if (message.mentions.length > 0 && !message.mentions.includes(botId)) continue;
      botIds.push(botId);
      live ||= streaming;
    }
    if (botIds.length > 0) steers.set(message.id, { botIds, live });
  }
  return steers;
}
