import { describe, expect, it } from "vitest";
import { draftAfter, groupCommands, matchCommands, skillPrompt, slashQuery, type SlashCommand } from "./slash";

const command = (id: string, group: string, title: string): SlashCommand => ({ id, group, title, run: () => {} });
const commands = [
  command("tasks", "面板", "打开任务"),
  command("new-room", "动作", "新建房间"),
  command("usage", "面板", "看用量和花费"),
  command("stop", "动作", "停止正在进行的回复"),
];

describe("slash", () => {
  it("只有以 / 开头、还没打空格时才算在输入命令", () => {
    expect(slashQuery("/")).toBe("");
    expect(slashQuery("/任务")).toBe("任务");
    expect(slashQuery("/任务 然后")).toBeNull();
    expect(slashQuery("看 /etc/hosts")).toBeNull();
    expect(slashQuery("")).toBeNull();
  });

  it("按名字里的字找，不分大小写", () => {
    expect(matchCommands(commands, "").length).toBe(4);
    expect(matchCommands(commands, "用量").map((c) => c.id)).toEqual(["usage"]);
    expect(matchCommands(commands, "没有这个")).toEqual([]);
    expect(matchCommands([command("x", "动作", "Open PR")], "open").length).toBe(1);
  });

  it("同一组的挨在一起，组按第一次出现的先后", () => {
    const groups = groupCommands(commands);
    expect(groups.map((g) => g.group)).toEqual(["面板", "动作"]);
    expect(groups[0].items.map((c) => c.id)).toEqual(["tasks", "usage"]);
    expect(groups[1].items.map((c) => c.id)).toEqual(["new-room", "stop"]);
  });

  it("说明里的字也能找到", () => {
    const skill = { ...command("skill:review", "技能", "review"), hint: "审查一个 PR" };
    expect(matchCommands([skill], "审查").length).toBe(1);
  });

  it("选了技能把话放到草稿前面，选了动作草稿不动", () => {
    const skill = { ...command("skill:review", "技能", "review"), insert: skillPrompt("review") };
    expect(draftAfter(skill, "")).toBe("用「review」技能：");
    expect(draftAfter(skill, "看看 #55")).toBe("用「review」技能：看看 #55");
    expect(draftAfter(commands[0], "写了一半")).toBe("写了一半");
  });
});
