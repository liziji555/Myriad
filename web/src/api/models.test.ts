import { describe, expect, it } from "vitest";
import { draftOf, draftProblem, saveRequest } from "./models";
import type { ModelSettings } from "./types";

const choice = (model: string) => ({ provider: "deepseek", model, thinking: null, context_window: null });
const settings: ModelSettings = {
  revision: "r1",
  providers: [{ name: "deepseek", kind: "openai_compat", base_url: "https://api.deepseek.com", has_key: true }],
  cheap: choice("a"),
  mid: choice("b"),
  strong: choice("c"),
};

describe("models", () => {
  it("没重新填的密钥不往回发", () => {
    const draft = draftOf(settings);
    expect(draftProblem(draft)).toBeNull();
    expect(saveRequest(draft).providers[0].api_key).toBeNull();
    draft.providers[0].api_key = " sk-1 ";
    expect(saveRequest(draft).providers[0].api_key).toBe("sk-1");
    expect(saveRequest(draft).revision).toBe("r1");
  });

  it("地址改了要重新填密钥", () => {
    const draft = draftOf(settings);
    draft.providers[0].base_url = "https://elsewhere.example";
    expect(draftProblem(draft)).toContain("要重新填 API Key");
    draft.providers[0].api_key = "sk-2";
    expect(draftProblem(draft)).toBeNull();
  });

  it("说得出为什么不能保存", () => {
    const draft = draftOf(settings);
    draft.providers.push({ name: "My Model", kind: "anthropic", base_url: "", api_key: "", has_key: false, saved: false, saved_url: null });
    expect(draftProblem(draft)).toContain("小写字母");
    draft.providers[1].name = "mine";
    expect(draftProblem(draft)).toContain("还没有填地址");
    draft.providers[1].base_url = "https://example.com";
    draft.strong = { ...draft.strong, provider: "mine" };
    expect(draftProblem(draft)).toContain("还没有填 API Key");
    draft.providers[1].api_key = "k";
    draft.mid = { ...draft.mid, model: " " };
    expect(draftProblem(draft)).toContain("「中」档还没有填模型的名字");
  });
});
