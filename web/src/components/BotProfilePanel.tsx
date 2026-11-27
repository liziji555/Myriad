import { useEffect, useRef, useState, type FormEvent } from "react";
import { api } from "../api/client";
import type { BotStopTarget, GlobalBotSignal } from "../api/presence";
import type { Team } from "../api/useWorkspace";
import type { Bot, BotId, Harness, HarnessProbe, ModelTier, RoomId } from "../api/types";
import { AvatarStudioModal } from "./AvatarStudio";
import { BotMascot } from "./BotMascot";

const HARNESSES: readonly { id: Harness; label: string }[] = [
  { id: "builtin", label: "内置模型（直接调 API）" },
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

export interface BotProfileFocusRequest {
  botId: BotId;
  seq: number;
}

interface Props {
  roomId: RoomId;
  /** 当前展示的 Bot 列表（1v1 对话时为该 Bot，群聊房间时为房间成员列表）。 */
  candidates: readonly Bot[];
  /** 所有 Bot（用于跨房间打开档案及判断是否仅剩最后一位主控）。 */
  allBots: readonly Bot[];
  focusRequest: BotProfileFocusRequest | null;
  signals: Readonly<Record<BotId, GlobalBotSignal>>;
  team: Team;
  onOpenBotChat(bot: Bot): void;
  onOpenMembers(): void;
  onStopTarget(botId: BotId, target: BotStopTarget): void;
}

/**
 * Bot 专属档案面板（借鉴 DS-Bot DetailsDrawer & Grok Bot AgentSettingsPanel）：
 * - 76px 可盯鼠标、可点击戳一戳的动态大形象 + 一键打开「形象工作室」切换造型/配色/上传头像；
 * - 直接编辑名字、职责短标签、执行引擎（Harness）/档位、人设分工、启停与主控身份。
 */
export function BotProfilePanel({
  roomId,
  candidates,
  allBots,
  focusRequest,
  signals,
  team,
  onOpenBotChat,
  onOpenMembers,
  onStopTarget,
}: Props) {
  const [selectedId, setSelectedId] = useState<BotId | null>(
    focusRequest?.botId ?? candidates[0]?.id ?? allBots[0]?.id ?? null,
  );
  const prevRoomIdRef = useRef<RoomId>(roomId);
  const bot =
    allBots.find((b) => b.id === selectedId) ??
    candidates.find((b) => b.id === selectedId) ??
    candidates[0] ??
    allBots[0] ??
    null;

  const [name, setName] = useState(bot?.name ?? "");
  const [role, setRole] = useState(bot?.role ?? "");
  const [persona, setPersona] = useState(bot?.persona ?? "");
  const [tier, setTier] = useState<ModelTier>(bot?.tier ?? "mid");
  const [harness, setHarness] = useState<Harness>(bot?.harness ?? "builtin");
  const [command, setCommand] = useState(bot?.harness_command ?? "");
  const [enabled, setEnabled] = useState(bot?.enabled ?? true);
  const [studioOpen, setStudioOpen] = useState(false);
  const [probes, setProbes] = useState<readonly HarnessProbe[]>([]);
  const [busy, setBusy] = useState(false);
  const [savedTip, setSavedTip] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  // 响应显式「打开某位 Bot 档案」请求（支持侧栏点击房间外的任意 Bot）。
  useEffect(() => {
    if (!focusRequest) return;
    if (allBots.some((b) => b.id === focusRequest.botId)) {
      setSelectedId(focusRequest.botId);
    }
  }, [focusRequest?.seq, focusRequest?.botId, allBots]);

  // 切换房间时跟随新房间的默认成员，不因同房间数组重算而覆盖用户手动切换。
  useEffect(() => {
    if (prevRoomIdRef.current !== roomId) {
      prevRoomIdRef.current = roomId;
      setSelectedId(candidates[0]?.id ?? allBots[0]?.id ?? null);
    }
  }, [roomId, candidates, allBots]);

  // 若当前查看的 Bot 被删除，自动回落到有效 Bot。
  useEffect(() => {
    if (selectedId && !allBots.some((b) => b.id === selectedId)) {
      setSelectedId(candidates[0]?.id ?? allBots[0]?.id ?? null);
    }
  }, [selectedId, allBots, candidates]);

  useEffect(() => {
    if (!bot) return;
    setName(bot.name);
    setRole(bot.role);
    setPersona(bot.persona);
    setTier(bot.tier);
    setHarness(bot.harness);
    setCommand(bot.harness_command);
    setEnabled(bot.enabled);
    setError(null);
    setSavedTip(null);
  }, [bot?.id, bot?.name, bot?.role, bot?.persona, bot?.tier, bot?.harness, bot?.harness_command, bot?.enabled]);

  useEffect(() => {
    let alive = true;
    api.listHarnesses().then(
      (list) => alive && setProbes(list),
      () => {},
    );
    return () => {
      alive = false;
    };
  }, []);

  if (!bot) {
    return <p className="sa-panel-empty">还没有可查看的 Bot。</p>;
  }

  const signal = signals[bot.id];
  const stopTarget = signal?.stopTarget;
  const masterCount = allBots.filter((b) => b.is_master).length;
  const soleMaster = bot.is_master && masterCount <= 1 && allBots.length > 1;
  const canPromoteMaster = bot.is_master || (bot.harness === "builtin" && harness === "builtin");
  const switcherBots = candidates.some((item) => item.id === bot.id)
    ? candidates
    : [bot, ...candidates];

  const saveProfile = async (event: FormEvent) => {
    event.preventDefault();
    if (busy) return;
    setBusy(true);
    setError(null);
    setSavedTip(null);
    try {
      await team.updateBot(bot.id, {
        name: name.trim(),
        role: role.trim(),
        persona: persona.trim(),
        tier,
        harness,
        harness_command: harness === "builtin" ? "" : command.trim(),
        enabled,
      });
      setSavedTip("已保存档案");
      window.setTimeout(() => setSavedTip(null), 2000);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setBusy(false);
    }
  };

  const toggleMaster = async () => {
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      await team.updateBot(bot.id, { is_master: !bot.is_master });
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setBusy(false);
    }
  };

  const removeBot = async () => {
    if (busy) return;
    if (!window.confirm(`确定删除 Bot「${bot.name}」吗？它发过的消息记录会保留。`)) return;
    setBusy(true);
    setError(null);
    try {
      await team.deleteBot(bot.id);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="sa-bot-profile">
      {studioOpen && (
        <AvatarStudioModal
          botId={bot.id}
          botName={bot.name}
          isMaster={bot.is_master}
          initial={{
            avatar_shape: bot.avatar_shape,
            avatar_color: bot.avatar_color,
            avatar_image: bot.avatar_image,
          }}
          onSave={async (draft) => {
            await team.updateBot(bot.id, draft);
          }}
          onClose={() => setStudioOpen(false)}
        />
      )}

      {switcherBots.length > 1 && (
        <div className="sa-profile-switcher" role="tablist" aria-label="切换查看的 Bot">
          {switcherBots.map((item) => (
            <button
              key={item.id}
              type="button"
              role="tab"
              className="sa-profile-pill"
              aria-selected={item.id === bot.id}
              onClick={() => setSelectedId(item.id)}
            >
              <BotMascot
                id={item.id}
                name={item.name}
                isMaster={item.is_master}
                shape={item.avatar_shape}
                color={item.avatar_color}
                image={item.avatar_image}
                state={signals[item.id]?.mascotState ?? (item.enabled ? "idle" : "sleeping")}
                size={20}
                showBadge={false}
              />
              <span>{item.name}</span>
            </button>
          ))}
        </div>
      )}

      {/* 顶部大形象展示区：可盯鼠标、可戳 360° 弹跳、一键打开形象工作室 */}
      <section className="sa-profile-hero">
        <BotMascot
          id={bot.id}
          name={bot.name}
          isMaster={bot.is_master}
          shape={bot.avatar_shape}
          color={bot.avatar_color}
          image={bot.avatar_image}
          state={signal?.mascotState ?? (bot.enabled ? "idle" : "sleeping")}
          activeTool={signal?.activeTool}
          size={76}
          gaze
          interactive
        />
        <div className="sa-profile-hero-meta">
          <div className="sa-profile-hero-title">
            <h3>{bot.name}</h3>
            {bot.is_master && <span className="sa-master-badge">中枢主控</span>}
          </div>
          <p className="sa-profile-hero-sub">
            {bot.role || (bot.is_master ? "主控中枢" : "专职 Bot")} · {signal?.text ?? (bot.enabled ? "空闲" : "休眠")}
          </p>
          <div className="sa-profile-hero-actions">
            <button
              type="button"
              className="sa-button sa-button-quiet"
              onClick={() => setStudioOpen(true)}
            >
              定制形象
            </button>
            <button
              type="button"
              className="sa-button sa-button-quiet"
              onClick={() => onOpenBotChat(bot)}
            >
              1v1 对话
            </button>
            {stopTarget && (
              <button
                type="button"
                className="sa-button sa-button-quiet"
                onClick={() => onStopTarget(bot.id, stopTarget)}
              >
                停止
              </button>
            )}
          </div>
        </div>
      </section>

      <form className="sa-form" onSubmit={saveProfile}>
        <div className="sa-create-row">
          <label className="sa-field">
            名字
            <input maxLength={24} value={name} onChange={(e) => setName(e.target.value)} />
          </label>
          <label className="sa-field">
            职责标签
            <input
              maxLength={24}
              value={role}
              placeholder="例如：主控中枢、前端专家"
              onChange={(e) => setRole(e.target.value)}
            />
          </label>
        </div>

        <div className="sa-create-row">
          <label className="sa-field">
            执行引擎
            <select value={harness} onChange={(e) => setHarness(e.target.value as Harness)}>
              {HARNESSES.map((item) => {
                const probe = probes.find((p) => p.harness === item.id);
                const suffix = probe ? (probe.available ? " · 已安装" : " · 未检测到") : "";
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

        {harness !== "builtin" && (
          <label className="sa-field">
            启动命令（留空使用默认）
            <input
              value={command}
              placeholder={harness === "acp" ? "例如：gemini --experimental-acp" : "留空使用默认命令"}
              onChange={(e) => setCommand(e.target.value)}
            />
          </label>
        )}

        <label className="sa-field">
          分工与人设（Persona）
          <textarea
            rows={4}
            value={persona}
            placeholder="写清楚它的专长、行为准则和协作方式…"
            onChange={(e) => setPersona(e.target.value)}
          />
        </label>

        <label className="sa-check">
          <input
            type="checkbox"
            checked={enabled}
            onChange={(e) => setEnabled(e.target.checked)}
          />
          <span>启用该 Bot（停用后进入闭眼休眠态，不再响应消息）</span>
        </label>

        {error && (
          <p className="sa-form-error" role="alert">
            {error}
          </p>
        )}
        {savedTip && <p className="sa-links-hint">{savedTip}</p>}

        <div className="sa-profile-footer">
          <button type="submit" className="sa-button" disabled={busy}>
            {busy ? "保存中…" : "保存修改"}
          </button>
          <button
            type="button"
            className="sa-button sa-button-quiet"
            disabled={busy || soleMaster || !canPromoteMaster}
            title={
              soleMaster
                ? "团队至少需保留一位中枢主控"
                : !canPromoteMaster
                  ? "仅内置模型引擎（builtin）的 Bot 可设为主控中枢"
                  : undefined
            }
            onClick={() => void toggleMaster()}
          >
            {bot.is_master ? "取消主控" : "设为主控"}
          </button>
          <button type="button" className="sa-link" onClick={onOpenMembers}>
            房间与工位 →
          </button>
          {!soleMaster && (
            <button
              type="button"
              className="sa-link sa-danger-link"
              disabled={busy}
              onClick={() => void removeBot()}
            >
              删除
            </button>
          )}
        </div>
      </form>
    </div>
  );
}
