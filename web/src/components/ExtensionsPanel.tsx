import { useCallback, useEffect, useState } from "react";
import { api } from "../api/client";
import { EXTENSIONS_CHANGED, mcpDot, mcpStatus, skillAudience } from "../api/extensions";
import type { Extensions } from "../api/types";

/** 右栏的「扩展」页：Bot 有哪些技能，接了哪些外部 MCP 服务。支持热重载 config.toml 中的 MCP 与钩子。 */
export function ExtensionsPanel() {
  const [found, setFound] = useState<Extensions | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    setBusy(true);
    try {
      setFound(await api.extensions());
      setError(null);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    }
    setBusy(false);
  }, []);

  const reload = useCallback(async () => {
    setBusy(true);
    setNotice(null);
    try {
      const next = await api.reloadExtensions();
      setFound(next);
      setError(null);
      const time = new Date().toLocaleTimeString("zh-CN", {
        hour: "2-digit",
        minute: "2-digit",
        hour12: false,
      });
      setNotice(`已热重载配置 · ${time}`);
      window.dispatchEvent(new Event(EXTENSIONS_CHANGED));
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    }
    setBusy(false);
  }, []);

  useEffect(() => {
    void load();
    const onChanged = () => void load();
    window.addEventListener(EXTENSIONS_CHANGED, onChanged);
    return () => window.removeEventListener(EXTENSIONS_CHANGED, onChanged);
  }, [load]);

  // 有服务正在连的时候，过一会儿自己再看一眼。
  const connecting = found?.mcp.some((server) => server.state === "connecting") ?? false;
  useEffect(() => {
    if (!connecting) return;
    const timer = setTimeout(() => void load(), 2000);
    return () => clearTimeout(timer);
  }, [connecting, found, load]);

  if (!found) {
    return <p className="sa-panel-empty">{error ?? "正在加载…"}</p>;
  }

  return (
    <div className="sa-members">
      {error && (
        <p className="sa-form-error" role="alert">
          {error}
        </p>
      )}
      <section className="sa-links-section" aria-labelledby="sa-ext-skills">
        <h3 id="sa-ext-skills" className="sa-section-title">
          技能
        </h3>
        {found.skills.length === 0 ? (
          <p className="sa-panel-empty">
            还没有技能。技能是事先写好的做事套路：在数据目录的 skills 里放「名字/SKILL.md」或者「名字.md」，下一轮对话
            Bot 就能用。
          </p>
        ) : (
          <ul className="sa-list" aria-label="技能">
            {found.skills.map((skill) => (
              <li key={skill.name} className="sa-member">
                <div className="sa-row sa-row-static">
                  <span className="sa-row-label">{skill.name}</span>
                  <span className="sa-row-meta">{skillAudience(skill)}</span>
                </div>
                <p className="sa-member-role">{skill.description}</p>
              </li>
            ))}
          </ul>
        )}
        {found.skill_problems.map((problem) => (
          <p key={problem} className="sa-routine-last" data-result="failed">
            没认出来：{problem}
          </p>
        ))}
      </section>

      <section className="sa-links-section" aria-labelledby="sa-ext-mcp">
        <h3 id="sa-ext-mcp" className="sa-section-title">
          MCP 服务
        </h3>
        {found.mcp.length === 0 ? (
          <p className="sa-panel-empty">
            还没有接外部 MCP 服务。在 config.toml 里加一节 [mcp.名字] 后点下方「刷新 / 热重载」即可生效（受信任服务可配 trust_annotations = true 自动放行只读工具）。
          </p>
        ) : (
          <ul className="sa-list" aria-label="MCP 服务">
            {found.mcp.map((server) => (
              <li key={server.name} className="sa-member">
                <div className="sa-row sa-row-static">
                  <span className="sa-dot" data-state={mcpDot(server)} aria-hidden="true" />
                  <span className="sa-row-label">{server.name}</span>
                  <span className="sa-row-meta">{mcpStatus(server)}</span>
                </div>
                {server.error && (
                  <p className="sa-routine-last" data-result="failed">
                    {server.error}
                  </p>
                )}
                {server.tools.length > 0 && <p className="sa-member-role">{server.tools.join("、")}</p>}
              </li>
            ))}
          </ul>
        )}
      </section>
      <div className="sa-ext-actions">
        <button type="button" className="sa-button sa-button-quiet" disabled={busy} onClick={() => void reload()}>
          {busy ? "正在热重载…" : "刷新 / 热重载"}
        </button>
        {notice && (
          <span className="sa-row-meta" role="status">
            {notice}
          </span>
        )}
      </div>
    </div>
  );
}
