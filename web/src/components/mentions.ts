export interface Piece {
  text: string;
  /** 这一段是不是点名（`@名字`）。 */
  mention: boolean;
}

/**
 * 把消息正文按 `@名字` 切开，方便把点名高亮出来。
 * `names` 是这条消息实际点到的 Bot 的名字：没点到的 `@某某` 只是普通文字。不分大小写，长名字优先。
 */
export function splitMentions(content: string, names: readonly string[]): Piece[] {
  const wanted = names.filter((name) => name !== "").sort((a, b) => b.length - a.length);
  if (wanted.length === 0) return [{ text: content, mention: false }];
  const lower = content.toLowerCase();
  const pieces: Piece[] = [];
  let plain = 0;
  let at = lower.indexOf("@");
  while (at >= 0) {
    const name = wanted.find((candidate) => lower.startsWith(candidate.toLowerCase(), at + 1));
    if (name) {
      const end = at + 1 + name.length;
      if (at > plain) pieces.push({ text: content.slice(plain, at), mention: false });
      pieces.push({ text: content.slice(at, end), mention: true });
      plain = end;
      at = lower.indexOf("@", end);
    } else {
      at = lower.indexOf("@", at + 1);
    }
  }
  if (plain < content.length) pieces.push({ text: content.slice(plain), mention: false });
  return pieces;
}
