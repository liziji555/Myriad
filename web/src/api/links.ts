import type { Pairing, Source } from "./types";

const SOURCES: Record<Source, string> = { github: "GitHub", telegram: "Telegram" };

export const sourceName = (source: Source): string => SOURCES[source];

const TITLE = "标题：";

/**
 * 外面来的事那条消息的内容是「一句话说明、空行、原文」，原文的第一行可以是「标题：…」。拆开。
 * 标题和原文都是外人写的，只拿来显示。
 */
export function splitEvent(content: string): { summary: string; title: string; body: string } {
  const cut = content.indexOf("\n\n");
  if (cut < 0) return { summary: content.trim(), title: "", body: "" };
  const summary = content.slice(0, cut).trim();
  const rest = content.slice(cut + 2);
  if (!rest.startsWith(TITLE)) return { summary, title: "", body: rest.trim() };
  const end = rest.indexOf("\n");
  const line = end < 0 ? rest : rest.slice(0, end);
  return {
    summary,
    title: line.slice(TITLE.length).trim(),
    body: end < 0 ? "" : rest.slice(end + 1).trim(),
  };
}

/** 外面给的地址只有 http(s) 的才做成链接：别的（比如 `javascript:`）不能点。 */
export function safeUrl(url: string | undefined): string | undefined {
  if (!url) return undefined;
  try {
    const parsed = new URL(url);
    return parsed.protocol === "https:" || parsed.protocol === "http:" ? parsed.href : undefined;
  } catch {
    return undefined;
  }
}

/** 在和机器人的私聊里发这句话来配对。 */
export const pairingCommand = (pairing: Pairing): string => `/pair ${pairing.code}`;

const botName = (pairing: Pairing): string | undefined =>
  pairing.bot_username && /^\w+$/.test(pairing.bot_username) ? pairing.bot_username : undefined;

/**
 * 在群里发这句话来配对。群里的机器人默认只收得到点了它名字的命令，所以要写成 `/pair@机器人`。
 * 还不知道机器人叫什么时没有。
 */
export function pairingGroupCommand(pairing: Pairing): string | undefined {
  const name = botName(pairing);
  return name ? `/pair@${name} ${pairing.code}` : undefined;
}

/** 点开就在和机器人的私聊里配对的链接。还不知道机器人叫什么时没有。 */
export function pairingLink(pairing: Pairing): string | undefined {
  const name = botName(pairing);
  return name ? `https://t.me/${name}?start=${encodeURIComponent(pairing.code)}` : undefined;
}

/** 配对码还剩几分钟（向上取整）。过期了是 0。 */
export function minutesLeft(pairing: Pairing, now: number = Date.now()): number {
  return Math.max(0, Math.ceil((pairing.expires_at - now) / 60_000));
}

/** 填的仓库像不像 `owner/name`。对不对最后由服务端判断。 */
export function looksLikeRepo(text: string): boolean {
  return /^[\w.-]+\/[\w.-]+$/.test(text.trim());
}

/** 从粘贴进来的仓库地址里取出 `owner/name`；本来就是 `owner/name` 的原样返回。 */
export function repoFromInput(text: string): string {
  const trimmed = text.trim();
  const match = /^(?:https?:\/\/)?github\.com\/([\w.-]+)\/([\w.-]+?)(?:\.git)?(?:[/?#].*)?$/i.exec(trimmed);
  return match ? `${match[1]}/${match[2]}` : trimmed;
}
