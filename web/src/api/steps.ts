/** 一步的结果默认只露出开头这么多行、这么多字，其余的点「查看完整」再看。 */
export const PREVIEW_LINES = 4;
export const PREVIEW_CHARS = 400;

/** 结果的开头几行；`cut` 表示后面还有没露出来的。 */
export function outputPreview(output: string): { text: string; cut: boolean } {
  const lines = output.split("\n");
  let text = lines.slice(0, PREVIEW_LINES).join("\n");
  if (text.length > PREVIEW_CHARS) text = text.slice(0, PREVIEW_CHARS);
  return { text, cut: text.length < output.length };
}
