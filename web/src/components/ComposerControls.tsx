import { useEffect, useRef, useState, type ReactNode } from "react";
import { api } from "../api/client";
import { contextMeter, TIERS } from "../api/models";
import type { Bot, ContextUsage, ModelSettings, ModelTier, PermissionMode, Room } from "../api/types";
import { Icon } from "./Icon";

export const PERMISSIONS: readonly { id: PermissionMode; label: string; note: string }[] = [
  { id: "ask", label: "每次都问", note: "改工作目录外的文件、执行命令前先问你" },
  { id: "full", label: "完全权限", note: "不问直接做，做了什么记在审计里。连着 GitHub、Telegram 的房间照样问" },
];

const HARNESS_NAMES: Record<Exclude<Bot["harness"], "builtin">, string> = {
  claude_code: "Claude Code",
  codex: "Codex",
  open_code: "OpenCode",
  command: "自定义命令",
};

interface Props {
  room: Room;
  /** 房间的入口 Bot：这里换的是它用的模型。 */
  host: Bot | null;
  /** 房间里有 Bot 正在回复：停下来之后重新看一次上下文用了多少。 */
  busy: boolean;
  onPermission(mode: PermissionMode): Promise<void>;
  onTier(tier: ModelTier): Promise<void>;
  onManageModels(): void;
}

/** 输入框下面那一行：这个房间的权限档位，和入口 Bot 用的模型。 */
export function ComposerControls({ room, host, busy, onPermission, onTier, onManageModels }: Props) {
  const [open, setOpen] = useState<"permission" | "model" | null>(null);
  const [models, setModels] = useState<ModelSettings | null>(null);
  const [error, setError] = useState<string | null>(null);
  const root = useRef<HTMLDivElement>(null);

  // 打开菜单时重新读：模型页里刚保存的马上看得到。
  useEffect(() => {
    if (open === "permission") return;
    let stale = false;
    api
      .models()
      .then((settings) => !stale && setModels(settings))
      .catch(() => !stale && setModels(null));
    return () => {
      stale = true;
    };
  }, [open]);

  const [context, setContext] = useState<ContextUsage | null>(null);
  useEffect(() => {
    let stale = false;
    api
      .roomContext(room.id)
      .then((usage) => !stale && setContext(usage))
      .catch(() => !stale && setContext(null));
    return () => {
      stale = true;
    };
  }, [room.id, host?.id, host?.tier, host?.harness, busy, models]);
  const meter = context && contextMeter(context);

  useEffect(() => {
    if (!open) return;
    const close = (event: Event) => {
      if (event instanceof KeyboardEvent ? event.key === "Escape" : !root.current?.contains(event.target as Node)) {
        setOpen(null);
      }
    };
    document.addEventListener("mousedown", close);
    document.addEventListener("keydown", close);
    return () => {
      document.removeEventListener("mousedown", close);
      document.removeEventListener("keydown", close);
    };
  }, [open]);

  const pick = (action: Promise<void>) => {
    setError(null);
    setOpen(null);
    action.catch((cause: unknown) => setError(cause instanceof Error ? cause.message : String(cause)));
  };

  const permission = PERMISSIONS.find((item) => item.id === room.permission) ?? PERMISSIONS[0];
  const builtin = host?.harness === "builtin";
  const tier = TIERS.find((item) => item.id === host?.tier);
  const modelLabel = !host
    ? "没有 Bot"
    : !builtin
      ? HARNESS_NAMES[host.harness as keyof typeof HARNESS_NAMES]
      : (models?.[host.tier].model ?? `${tier?.label}档`);

  const option = (key: string, selected: boolean, title: ReactNode, note: string, onPick: () => void) => (
    <button key={key} type="button" role="menuitemradio" aria-checked={selected} className="sa-chip-option" onClick={onPick}>
      <span className="sa-chip-option-title">{title}</span>
      <span className="sa-chip-option-note">{note}</span>
    </button>
  );

  return (
    <div className="sa-composer-controls" ref={root}>
      <div className="sa-chip-wrap">
        <button
          type="button"
          className="sa-chip"
          data-warn={room.permission === "full"}
          aria-haspopup="menu"
          aria-expanded={open === "permission"}
          title="这个房间里的 Bot 做事前问不问你"
          onClick={() => setOpen(open === "permission" ? null : "permission")}
        >
          <Icon name="shield" size={14} />
          {permission.label}
        </button>
        {open === "permission" && (
          <div className="sa-chip-menu" role="menu" aria-label="权限">
            {PERMISSIONS.map((item) =>
              option(item.id, item.id === room.permission, item.label, item.note, () => pick(onPermission(item.id))),
            )}
          </div>
        )}
      </div>
      {error && (
        <span className="sa-chip-error" role="alert">
          {error}
        </span>
      )}
      {meter && (
        <span className="sa-context" data-warn={meter.percent >= 80} title={meter.note} aria-label={meter.note}>
          上下文 {meter.percent}%
        </span>
      )}
      <div className="sa-chip-wrap sa-chip-wrap-end">
        <button
          type="button"
          className="sa-chip"
          disabled={!host}
          aria-haspopup="menu"
          aria-expanded={open === "model"}
          title={host ? `${host.name} 用的模型` : undefined}
          onClick={() => setOpen(open === "model" ? null : "model")}
        >
          <Icon name="bot" size={14} />
          <span className="sa-chip-text">{modelLabel}</span>
          {builtin && tier && <span className="sa-chip-tag">{tier.label}</span>}
        </button>
        {open === "model" && host && (
          <div className="sa-chip-menu" role="menu" aria-label="模型">
            <p className="sa-menu-group">{host.name} 用的模型</p>
            {builtin ? (
              TIERS.map((item) =>
                option(
                  item.id,
                  item.id === host.tier,
                  <>
                    {models?.[item.id].model ?? "…"}
                    <span className="sa-chip-tag">{item.label}</span>
                  </>,
                  item.note,
                  () => pick(onTier(item.id)),
                ),
              )
            ) : (
              <p className="sa-menu-empty">这个 Bot 交给 {modelLabel} 来干，模型在它自己那边设。</p>
            )}
            <button
              type="button"
              role="menuitem"
              className="sa-chip-option sa-chip-option-more"
              onClick={() => {
                setOpen(null);
                onManageModels();
              }}
            >
              接自己的模型、改三个档位…
            </button>
          </div>
        )}
      </div>
    </div>
  );
}
