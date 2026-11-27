import { useEffect, useState, type FormEvent } from "react";
import { api } from "../api/client";
import type { Bot, CreateBotRequest, Harness, HarnessProbe, ModelTier } from "../api/types";
import { AvatarStudioModal, type AvatarDraft } from "./AvatarStudio";
import { BotMascot } from "./BotMascot";

const HARNESSES: readonly { id: Harness; label: string }[] = [
  { id: "builtin", label: "内置模型（直接调用 API）" },
  { id: "claude_code", label: "Claude Code CLI" },
  { id: "codex", label: "Codex CLI" },
  { id: "open_code", label: "OpenCode CLI" },
  { id: "gemini", label: "Gemini CLI" },
  { id: "kimi", label: "Kimi Code CLI" },
  { id: "acp", label: "ACP 通用 Agent" },
  { id: "command", label: "自定义命令" },
];

const TIERS: readonly { id: ModelTier; label: string }[] = [
  { id: "cheap", label: "便宜档" },
  { id: "mid", label: "中档" },
  { id: "strong", label: "强档" },
];

const PRESETS: readonly {
  name: string;
  role: string;
  persona: string;
  tier: ModelTier;
  shape: string;
  color: string;
}[] = [
  {
    name: "前端专家",
    role: "界面与交互",
    persona: "负责界面设计、React/TypeScript 组件实现与交互细节打磨，注重视觉整洁与响应式体验。",
    tier: "strong",
    shape: "cat",
    color: "aurora",
  },
  {
    name: "后端工匠",
    role: "架构与服务",
    persona: "负责服务端架构、API 设计、并发安全与单元测试，修改后主动运行验证。",
    tier: "strong",
    shape: "hex",
    color: "cobalt",
  },
  {
    name: "代码审查",
    role: "质量把关",
    persona: "负责审查代码改动、边界条件、安全隐患与性能问题，给出直接可落地的改进建议。",
    tier: "mid",
    shape: "shield",
    color: "emerald",
  },
  {
    name: "资料调研",
    role: "情报检索",
    persona: "负责快速检索代码库与公开资料，提炼结构化结论与来源，不凭印象猜测。",
    tier: "mid",
    shape: "sprout",
    color: "cyan",
  },
];

interface Props {
  onCreate(body: CreateBotRequest, openChat: boolean): Promise<Bot>;
  onClose(): void;
}

/** 独立创建 Bot 弹窗（集成自建动态角色形象选择与快速角色模板）。 */
export function BotCreateModal({ onCreate, onClose }: Props) {
  const [previewId] = useState(() => `draft-${Math.random().toString(36).slice(2, 8)}`);
  const [name, setName] = useState("");
  const [role, setRole] = useState("");
  const [persona, setPersona] = useState("");
  const [tier, setTier] = useState<ModelTier>("mid");
  const [harness, setHarness] = useState<Harness>("builtin");
  const [command, setCommand] = useState("");
  const [avatar, setAvatar] = useState<AvatarDraft>({
    avatar_shape: "",
    avatar_color: "",
    avatar_image: "",
  });
  const [studioOpen, setStudioOpen] = useState(false);
  const [openChat, setOpenChat] = useState(true);
  const [probes, setProbes] = useState<readonly HarnessProbe[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    api.listHarnesses().then(
      (found) => alive && setProbes(found),
      () => {},
    );
    return () => {
      alive = false;
    };
  }, []);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !busy && !studioOpen) onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [busy, studioOpen, onClose]);

  const activeProbe = probes.find((p) => p.harness === harness);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (busy) return;
    if (!name.trim()) {
      setError("请给新 Bot 起一个名字。");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await onCreate(
        {
          name: name.trim(),
          role: role.trim() || undefined,
          persona: persona.trim(),
          tier,
          harness,
          harness_command:
            harness === "acp" || harness === "command" ? command.trim() || undefined : undefined,
          avatar_shape: avatar.avatar_shape || undefined,
          avatar_color: avatar.avatar_color || undefined,
          avatar_image: avatar.avatar_image || undefined,
        },
        openChat,
      );
      onClose();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div
      className="sa-palette-backdrop"
      onMouseDown={(event) => event.target === event.currentTarget && !busy && !studioOpen && onClose()}
    >
      <div className="sa-connect-panel sa-create-modal" role="dialog" aria-modal="true" aria-label="新建 Bot">
        <header className="sa-connect-head">
          <h2>新建 Bot</h2>
          <button type="button" className="sa-link" disabled={busy} onClick={onClose}>
            关闭
          </button>
        </header>

        {studioOpen && (
          <AvatarStudioModal
            botId={previewId}
            botName={name.trim() || "新 Bot"}
            initial={avatar}
            onSave={(draft) => setAvatar(draft)}
            onClose={() => setStudioOpen(false)}
          />
        )}

        <div className="sa-create-hero">
          <BotMascot
            id={previewId}
            name={name.trim() || "新 Bot"}
            shape={avatar.avatar_shape}
            color={avatar.avatar_color}
            image={avatar.avatar_image}
            state="idle"
            size={64}
            gaze
            interactive
          />
          <div className="sa-create-hero-copy">
            <strong>{name.trim() || "给新 Bot 选个专属形象"}</strong>
            <small>12 款自建动态角色造型 × 12 种渐变，也支持上传图片</small>
            <button
              type="button"
              className="sa-button sa-button-quiet"
              onClick={() => setStudioOpen(true)}
            >
              切换造型与配色
            </button>
          </div>
        </div>

        <div className="sa-preset-row" aria-label="快速角色模板">
          <span className="sa-preset-label">快速填充：</span>
          {PRESETS.map((preset) => (
            <button
              key={preset.name}
              type="button"
              className="sa-chip"
              onClick={() => {
                setName(preset.name);
                setRole(preset.role);
                setPersona(preset.persona);
                setTier(preset.tier);
                setAvatar({
                  avatar_shape: preset.shape,
                  avatar_color: preset.color,
                  avatar_image: "",
                });
              }}
            >
              {preset.name}
            </button>
          ))}
        </div>

        <form className="sa-form" onSubmit={submit}>
          <div className="sa-create-row">
            <label className="sa-field">
              Bot 名字
              <input
                autoFocus
                maxLength={24}
                value={name}
                placeholder="例如：前端专家、代码审查"
                onChange={(e) => setName(e.target.value)}
              />
            </label>
            <label className="sa-field">
              职责标签（可选）
              <input
                maxLength={24}
                value={role}
                placeholder="例如：界面与交互"
                onChange={(e) => setRole(e.target.value)}
              />
            </label>
          </div>

          <div className="sa-create-row">
            <label className="sa-field">
              执行引擎（Harness）
              <select value={harness} onChange={(e) => setHarness(e.target.value as Harness)}>
                {HARNESSES.map((item) => {
                  const p = probes.find((x) => x.harness === item.id);
                  const suffix = p ? (p.available ? " · 已安装" : " · 未检测到") : "";
                  return (
                    <option key={item.id} value={item.id}>
                      {item.label}
                      {suffix}
                    </option>
                  );
                })}
              </select>
            </label>

            {harness === "builtin" && (
              <label className="sa-field">
                模型档位
                <select value={tier} onChange={(e) => setTier(e.target.value as ModelTier)}>
                  {TIERS.map((t) => (
                    <option key={t.id} value={t.id}>
                      {t.label}
                    </option>
                  ))}
                </select>
              </label>
            )}
          </div>

          {activeProbe && harness !== "builtin" && (
            <p className="sa-links-hint">
              {activeProbe.available
                ? `已探测到 ${activeProbe.path ?? activeProbe.program}${activeProbe.version ? `（${activeProbe.version}）` : ""}`
                : `当前机器 PATH 中未检测到 ${activeProbe.program}，也可以在下方填写自定义路径。`}
            </p>
          )}

          {(harness === "acp" || harness === "command") && (
            <label className="sa-field">
              启动命令
              <input
                value={command}
                placeholder={harness === "acp" ? "例如：gemini --experimental-acp" : "例如：my-agent --json"}
                onChange={(e) => setCommand(e.target.value)}
              />
            </label>
          )}

          <label className="sa-field">
            职责与分工（可选）
            <textarea
              rows={3}
              value={persona}
              placeholder="写清楚它擅长什么、应该怎么配合你或主控工作…"
              onChange={(e) => setPersona(e.target.value)}
            />
          </label>

          <label className="sa-check">
            <input type="checkbox" checked={openChat} onChange={(e) => setOpenChat(e.target.checked)} />
            <span>创建后直接进入与该 Bot 的 1 对 1 对话</span>
          </label>

          {error && (
            <p className="sa-form-error" role="alert">
              {error}
            </p>
          )}

          <div className="sa-connect-actions">
            <button type="button" className="sa-button sa-button-quiet" disabled={busy} onClick={onClose}>
              取消
            </button>
            <button type="submit" className="sa-button" disabled={busy}>
              {busy ? "创建中…" : "创建 Bot"}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}
