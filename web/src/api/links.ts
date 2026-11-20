import type { Pairing, Source } from "./types";

const SOURCES: Record<Source, string> = { github: "GitHub", telegram: "Telegram" };

export const sourceName = (source: Source): string => SOURCES[source];

/** 外面来的事那条消息的内容是「一句话说明、空行、原文」。拆开。 */
export function splitEvent(content: string): { summary: string; body: string } {
  const cut = content.indexOf("\n\n");
  if (cut < 0) return { summary: content.trim(), body: "" };
  return { summary: content.slice(0, cut).trim(), body: content.slice(cut + 2).trim() };
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

/** 在 Telegram 聊天里发这句话来配对。 */
export const pairingCommand = (pairing: Pairing): string => `/pair ${pairing.code}`;

/** 点开就在和机器人的私聊里配对的链接。还不知道机器人叫什么时没有。 */
export function pairingLink(pairing: Pairing): string | undefined {
  const name = pairing.bot_username;
  if (!name || !/^\w+$/.test(name)) return undefined;
  return `https://t.me/${name}?start=${encodeURIComponent(pairing.code)}`;
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
