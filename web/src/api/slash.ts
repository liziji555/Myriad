/** 输入框里以 `/` 开头的命令：打开一个面板，或者做一个动作。 */
export interface SlashCommand {
  id: string;
  /** 分在哪一组，例如「面板」「动作」。 */
  group: string;
  title: string;
  run(): void;
}

/** 正在输入一个命令时，返回 `/` 后面已经输入的部分；不是在输入命令时返回 `null`。 */
export function slashQuery(text: string): string | null {
  if (!text.startsWith("/")) return null;
  const query = text.slice(1);
  return /\s/.test(query) ? null : query;
}

/** 名字里有输入的那几个字的命令，顺序不变（同一组的挨在一起）。 */
export function matchCommands<T extends SlashCommand>(commands: readonly T[], query: string): T[] {
  const wanted = query.trim().toLowerCase();
  return commands.filter((command) => command.title.toLowerCase().includes(wanted));
}

/** 按组排好，给菜单画分组标题用。组的先后按第一次出现的先后。 */
export function groupCommands<T extends SlashCommand>(commands: readonly T[]): { group: string; items: T[] }[] {
  const groups: { group: string; items: T[] }[] = [];
  for (const command of commands) {
    const found = groups.find((entry) => entry.group === command.group);
    if (found) found.items.push(command);
    else groups.push({ group: command.group, items: [command] });
  }
  return groups;
}
