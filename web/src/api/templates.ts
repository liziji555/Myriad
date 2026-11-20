import type { ImportReport } from "./types";

/** 模板文件最大多少字节。和服务端的上限一致，超过时不用上传就能告诉人。 */
export const MAX_TEMPLATE_BYTES = 1024 * 1024;

/** 导入之后给人看的一句话。 */
export function importSummary(report: ImportReport): string {
  const parts: string[] = [];
  if (report.created.length > 0) {
    parts.push(`新建了 ${report.created.map((bot) => bot.name).join("、")}`);
  }
  if (report.skipped.length > 0) {
    parts.push(`已经有 ${report.skipped.join("、")}，没有动`);
  }
  if (report.room) parts.push(`新建了房间「${report.room.name}」`);
  return parts.length > 0 ? `${parts.join("；")}。` : "模板是空的，没有新建任何东西。";
}

/** 让浏览器把一段文字存成文件。 */
export function saveTextFile(name: string, text: string): void {
  const url = URL.createObjectURL(new Blob([text], { type: "text/plain;charset=utf-8" }));
  const link = document.createElement("a");
  link.href = url;
  link.download = name;
  document.body.append(link);
  link.click();
  link.remove();
  URL.revokeObjectURL(url);
}
