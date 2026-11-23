import type { BotId, Message, MessageId } from "./types";

/** 一句插话：说的时候是哪个 Bot 正在回复，它是不是还在做。 */
export interface Steer {
  botId: BotId;
  /** 那个 Bot 还在做：它不会停下，下一次想之前会看到这句话。 */
  live: boolean;
}

/**
 * 哪些人说的话是插话：说的时候房间里有 Bot 正在回复，而且没有点名别的 Bot
 * （点名别的 Bot 会先打断再交接，不算插话）。
 */
export function steersOf(messages: readonly Message[]): Map<MessageId, Steer> {
  const steers = new Map<MessageId, Steer>();
  let reply: Message | undefined;
  for (const message of messages) {
    if (message.kind !== "chat") continue;
    if (message.sender.kind === "bot") {
      reply = message;
      continue;
    }
    if (!reply) continue;
    const { sender } = reply;
    if (sender.kind !== "bot") continue;
    // 那条回复在这句话之前就结束了：不是插话，是新的一轮。
    const live = reply.state === "streaming";
    if (!live && reply.updated_at <= message.created_at) continue;
    const named = message.mentions[0];
    if (named !== undefined && named !== sender.bot_id) continue;
    steers.set(message.id, { botId: sender.bot_id, live });
  }
  return steers;
}
