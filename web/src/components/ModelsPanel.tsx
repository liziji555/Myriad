import { useEffect, useState } from "react";
import { api } from "../api/client";
import {
  KINDS,
  TIERS,
  draftOf,
  draftProblem,
  providerInput,
  saveRequest,
  type ModelsDraft,
  type ProviderDraft,
} from "../api/models";
import type { ModelTier, ProviderKind, TestModelResult, TierChoice } from "../api/types";

/** 右栏的「模型」页：接哪些模型服务，三个档位各用哪个模型。保存后马上生效。 */
export function ModelsPanel() {
  const [draft, setDraft] = useState<ModelsDraft | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const [busy, setBusy] = useState(false);
  const [testing, setTesting] = useState<ModelTier | null>(null);
  const [results, setResults] = useState<Partial<Record<ModelTier, TestModelResult>>>({});

  useEffect(() => {
    api
      .models()
      .then((settings) => setDraft(draftOf(settings)))
      .catch((cause: unknown) => setError(cause instanceof Error ? cause.message : String(cause)));
  }, []);

  if (!draft) {
    return <p className="sa-panel-empty">{error ?? "正在加载…"}</p>;
  }

  const change = (next: ModelsDraft) => {
    setDraft(next);
    setSaved(false);
    setResults({});
  };
  const setProvider = (index: number, patch: Partial<ProviderDraft>) => {
    const old = draft.providers[index].name.trim();
    const providers = draft.providers.map((p, i) => (i === index ? { ...p, ...patch } : p));
    const next = { ...draft, providers };
    // 改了名字，用着它的档位跟着改。
    const name = patch.name?.trim();
    if (name !== undefined && name !== old) {
      for (const { id } of TIERS) {
        if (next[id].provider === old) next[id] = { ...next[id], provider: name };
      }
    }
    change(next);
  };
  const setTier = (tier: ModelTier, patch: Partial<TierChoice>) =>
    change({ ...draft, [tier]: { ...draft[tier], ...patch } });

  const problem = draftProblem(draft);

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      setDraft(draftOf(await api.saveModels(saveRequest(draft))));
      setSaved(true);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    }
    setBusy(false);
  };

  const test = async (tier: ModelTier) => {
    const choice = draft[tier];
    const provider = draft.providers.find((p) => p.name.trim() === choice.provider);
    if (!provider) return;
    setTesting(tier);
    let result: TestModelResult;
    try {
      result = await api.testModel({ provider: providerInput(provider), model: choice.model });
    } catch (cause) {
      result = { ok: false, message: cause instanceof Error ? cause.message : String(cause) };
    }
    setResults((old) => ({ ...old, [tier]: result }));
    setTesting(null);
  };

  return (
    <div className="sa-members">
      <section className="sa-links-section" aria-labelledby="sa-models-providers">
        <h3 id="sa-models-providers" className="sa-section-title">
          模型服务
        </h3>
        <p className="sa-links-hint">
          接口选「OpenAI 兼容」能接 OpenAI、DeepSeek 和大多数中转；选「Anthropic」接 Claude 官方和兼容它的中转。API Key
          只存在服务器上，这里不会再显示出来。
        </p>
        {draft.providers.map((provider, index) => (
          <fieldset key={index} className="sa-form sa-model-provider" aria-label={`模型服务 ${provider.name}`}>
            <label className="sa-field">
              <span>名字</span>
              <input
                value={provider.name}
                maxLength={24}
                spellCheck={false}
                placeholder="比如 deepseek"
                disabled={provider.saved}
                title={provider.saved ? "存过的模型服务不能改名；要换名字就删掉它再加一个" : undefined}
                onChange={(event) => setProvider(index, { name: event.target.value })}
              />
            </label>
            <label className="sa-field">
              <span>接口</span>
              <select
                value={provider.kind}
                onChange={(event) => setProvider(index, { kind: event.target.value as ProviderKind })}
              >
                {KINDS.map(({ id, label }) => (
                  <option key={id} value={id}>
                    {label}
                  </option>
                ))}
              </select>
            </label>
            <label className="sa-field">
              <span>地址</span>
              <input
                value={provider.base_url}
                spellCheck={false}
                placeholder={KINDS.find((kind) => kind.id === provider.kind)?.hint}
                onChange={(event) => setProvider(index, { base_url: event.target.value })}
              />
            </label>
            <label className="sa-field">
              <span>API Key</span>
              <input
                type="password"
                value={provider.api_key}
                autoComplete="off"
                placeholder={provider.has_key ? "已经存着，不改就留空" : "还没有填"}
                onChange={(event) => setProvider(index, { api_key: event.target.value })}
              />
            </label>
            <button
              type="button"
              className="sa-link sa-link-danger"
              onClick={() => change({ ...draft, providers: draft.providers.filter((_, i) => i !== index) })}
            >
              删掉这个模型服务
            </button>
          </fieldset>
        ))}
        <button
          type="button"
          className="sa-button sa-button-quiet"
          onClick={() =>
            change({
              ...draft,
              providers: [
                ...draft.providers,
                { name: "", kind: "openai_compat", base_url: "", api_key: "", has_key: false, saved: false },
              ],
            })
          }
        >
          添加模型服务
        </button>
      </section>

      <section className="sa-links-section" aria-labelledby="sa-models-tiers">
        <h3 id="sa-models-tiers" className="sa-section-title">
          三个档位
        </h3>
        <p className="sa-links-hint">每个 Bot 选一个档位，这里定每个档位实际用哪个模型。</p>
        {TIERS.map(({ id, label, note }) => {
          const choice = draft[id];
          const result = results[id];
          return (
            <fieldset key={id} className="sa-form sa-model-provider" aria-label={`「${label}」档`}>
              <legend className="sa-model-tier">
                {label}
                <span>{note}</span>
              </legend>
              <label className="sa-field">
                <span>模型服务</span>
                <select value={choice.provider} onChange={(event) => setTier(id, { provider: event.target.value })}>
                  {!draft.providers.some((p) => p.name.trim() === choice.provider) && <option value="">请选择</option>}
                  {draft.providers
                    .filter((p) => p.name.trim() !== "")
                    .map((p) => (
                      <option key={p.name} value={p.name.trim()}>
                        {p.name.trim()}
                      </option>
                    ))}
                </select>
              </label>
              <label className="sa-field">
                <span>模型</span>
                <input
                  value={choice.model}
                  spellCheck={false}
                  placeholder="模型服务那边的模型名字"
                  onChange={(event) => setTier(id, { model: event.target.value })}
                />
              </label>
              <button
                type="button"
                className="sa-button sa-button-quiet"
                disabled={testing !== null || choice.model.trim() === ""}
                onClick={() => void test(id)}
              >
                {testing === id ? "正在测试…" : "测试"}
              </button>
              {result && (
                <p className="sa-routine-last" data-result={result.ok ? "ok" : "failed"} role="status">
                  {result.ok ? `通了，模型回的是：${result.message}` : `没通：${result.message}`}
                </p>
              )}
            </fieldset>
          );
        })}
      </section>

      {(error ?? problem) && (
        <p className="sa-form-error" role="alert">
          {error ?? problem}
        </p>
      )}
      {saved && (
        <p className="sa-links-hint" role="status">
          已保存，下一条消息就用新的设置。
        </p>
      )}
      <div className="sa-form-actions">
        <button type="button" className="sa-button" disabled={busy || problem !== null} onClick={() => void save()}>
          {busy ? "保存中…" : "保存"}
        </button>
      </div>
    </div>
  );
}
