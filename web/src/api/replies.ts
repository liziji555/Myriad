import type { Message, MessageId, RoomActivity } from "./types";
import { speakersOf } from "./presence";

/** 人说的一句话有没有人接：回复中、已回复、被人停下、回复失败（出错或写到一半服务退出）。 */
export type Reply = "replying" | "replied" | "stopped" | "failed";

/**
 * 人在房间里说的每句话现在的回复状态。接这句话的是它之后、人下一句话之前开始的 Bot 回复
 * （包括它们再交接出去的）；插话还算给它插进去的那几条回复。房间还没处理到这句、又有 Bot
 * 在说、在排队或在挑人时也算回复中。没有 Bot 接的话不标。
 */
export function repliesOf(
  messages: readonly Message[],
  activity: RoomActivity | undefined,
  steered: (id: MessageId) => readonly string[] = () => [],
): Map<MessageId, Reply> {
  const busy =
    speakersOf(activity).length > 0 || Boolean(activity?.routing) || (activity?.queued.length ?? 0) > 0;
  const handled = activity?.handled_seq ?? 0;
  // 人说的话、定时任务、外面来的事都会叫 Bot 开口：下一条这样的消息之后开始的回复不算给这一句。
  const said = messages.filter(
    (message) => message.sender.kind !== "bot" && ["chat", "routine", "event"].includes(message.kind),
  );
  // Bot 用 `ask_bot` 找别人的那句话一发出来就是写完的，不算它回答了。
  const chats = messages.filter(
    (message) => message.kind === "chat" && !(message.sender.kind === "bot" && message.mentions.length > 0),
  );
  const replies = new Map<MessageId, Reply>();
  said.forEach((said, index, all) => {
    if (said.kind !== "chat" || said.sender.kind !== "user") return;
    const next = all[index + 1];
    const steeredBots = steered(said.id);
    const answers = chats.filter(
      (message) =>
        message.sender.kind === "bot" &&
        ((message.seq > said.seq && (!next || message.seq < next.seq)) ||
          (message.seq < said.seq && steeredBots.includes(message.sender.bot_id) && lastBefore(chats, message, said))),
    );
    let reply: Reply | undefined;
    if (answers.some((message) => message.state === "streaming") || (busy && handled < said.seq)) reply = "replying";
    else if (answers.some((message) => message.state === "failed" || message.state === "interrupted")) reply = "failed";
    else if (answers.some((message) => message.state === "complete")) reply = "replied";
    else if (answers.length > 0) reply = "stopped";
    if (reply) replies.set(said.id, reply);
  });
  return replies;
}

/** 插话进的是那个 Bot 说这句话时最后一条回复，不是它更早的。 */
function lastBefore(chats: readonly Message[], reply: Message, said: Message): boolean {
  if (reply.sender.kind !== "bot") return false;
  const botId = reply.sender.bot_id;
  return !chats.some(
    (message) =>
      message.seq > reply.seq &&
      message.seq < said.seq &&
      message.sender.kind === "bot" &&
      message.sender.bot_id === botId,
  );
}
