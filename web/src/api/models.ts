import type { ModelSettings, ModelTier, ProviderKind, SaveModelsRequest, TierChoice } from "./types";

export const KINDS: readonly { id: ProviderKind; label: string; hint: string }[] = [
  { id: "openai_compat", label: "OpenAI 兼容", hint: "https://api.deepseek.com" },
  { id: "anthropic", label: "Anthropic", hint: "https://api.anthropic.com" },
];

export const TIERS: readonly { id: ModelTier; label: string; note: string }[] = [
  { id: "strong", label: "强", note: "难的活：规划、写代码" },
  { id: "mid", label: "中", note: "日常对话和大部分活" },
  { id: "cheap", label: "省", note: "起标题、压缩这类小活" },
];

/** 面板里正在改的一个模型服务。`api_key` 是这次新填的，空着表示沿用存着的。 */
export interface ProviderDraft {
  name: string;
  kind: ProviderKind;
  base_url: string;
  api_key: string;
  has_key: boolean;
  /** 已经存在服务器上的。存过之后名字不能改：密钥是按名字存的。 */
  saved: boolean;
  /** 存着的地址。存着的密钥只给这个地址用，地址改了要重新填。 */
  saved_url: string | null;
}

/** 不重新填密钥行不行：有存着的，而且地址没改。 */
export function keepsKey(provider: ProviderDraft): boolean {
  return provider.has_key && provider.base_url.trim() === provider.saved_url;
}

export interface ModelsDraft {
  revision: string;
  providers: ProviderDraft[];
  cheap: TierChoice;
  mid: TierChoice;
  strong: TierChoice;
}

export function draftOf(settings: ModelSettings): ModelsDraft {
  return {
    revision: settings.revision,
    providers: settings.providers.map((provider) => ({
      ...provider,
      api_key: "",
      saved: true,
      saved_url: provider.base_url,
    })),
    cheap: settings.cheap,
    mid: settings.mid,
    strong: settings.strong,
  };
}

export function providerInput(provider: ProviderDraft) {
  return {
    name: provider.name.trim(),
    kind: provider.kind,
    base_url: provider.base_url.trim(),
    api_key: provider.api_key.trim() || null,
  };
}

export function saveRequest(draft: ModelsDraft): SaveModelsRequest {
  return {
    revision: draft.revision,
    providers: draft.providers.map(providerInput),
    cheap: draft.cheap,
    mid: draft.mid,
    strong: draft.strong,
  };
}

/** 现在这样能不能保存；不能的话是为什么。 */
export function draftProblem(draft: ModelsDraft): string | null {
  const names = new Set<string>();
  for (const provider of draft.providers) {
    const name = provider.name.trim();
    // 存过的名字改不了，手写在 config.toml 里的也照样留着。
    if (!provider.saved && !/^[a-z0-9_-]{1,24}$/.test(name)) return "模型服务的名字只能用小写字母、数字、横线和下划线";
    if (names.has(name)) return `有两个模型服务都叫「${name}」`;
    names.add(name);
    if (!/^https?:\/\/\S+$/.test(provider.base_url.trim())) return `「${name}」还没有填地址`;
  }
  for (const { id, label } of TIERS) {
    const choice = draft[id];
    const provider = draft.providers.find((p) => p.name.trim() === choice.provider);
    if (!provider) return `「${label}」档还没有选模型服务`;
    if (choice.model.trim() === "") return `「${label}」档还没有填模型的名字`;
    if (!keepsKey(provider) && provider.api_key.trim() === "") {
      return provider.has_key
        ? `「${provider.name.trim()}」的地址改了，要重新填 API Key`
        : `「${provider.name.trim()}」还没有填 API Key`;
    }
  }
  return null;
}
