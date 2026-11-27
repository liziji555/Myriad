import { useRef, useState } from "react";
import {
  MASCOT_COLORS,
  MASCOT_SHAPES,
  resolveMascotColor,
  resolveMascotShape,
  type MascotColorId,
  type MascotShapeId,
  type MascotState,
} from "../api/mascotShapes";
import { BotMascot } from "./BotMascot";

const PREVIEW_STATES: readonly { id: MascotState; label: string }[] = [
  { id: "idle", label: "空闲" },
  { id: "thinking", label: "思考" },
  { id: "searching", label: "检索" },
  { id: "working", label: "工作中" },
  { id: "sending", label: "派活" },
  { id: "waiting", label: "等审批" },
  { id: "done", label: "完成" },
  { id: "error", label: "报错" },
  { id: "sleeping", label: "休眠" },
];

export interface AvatarDraft {
  avatar_shape: string;
  avatar_color: string;
  avatar_image: string;
}

/** 将上传图片居中裁剪并压缩到 160×160 WebP/PNG data URL（保证小于 256 KB）。 */
async function cropAvatarFile(file: File): Promise<string> {
  const dataUrl = await new Promise<string>((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(String(reader.result ?? ""));
    reader.onerror = () => reject(new Error("读取图片失败"));
    reader.readAsDataURL(file);
  });
  const img = await new Promise<HTMLImageElement>((resolve, reject) => {
    const el = new Image();
    el.onload = () => resolve(el);
    el.onerror = () => reject(new Error("无法解析该图片"));
    el.src = dataUrl;
  });
  const side = 160;
  const canvas = document.createElement("canvas");
  canvas.width = side;
  canvas.height = side;
  const ctx = canvas.getContext("2d");
  if (!ctx) throw new Error("浏览器不支持图片裁剪");
  const minDim = Math.min(img.width, img.height);
  const sx = (img.width - minDim) / 2;
  const sy = (img.height - minDim) / 2;
  ctx.drawImage(img, sx, sy, minDim, minDim, 0, 0, side, side);
  const webp = canvas.toDataURL("image/webp", 0.88);
  if (webp.startsWith("data:image/webp;base64,") && webp.length < 250_000) return webp;
  const png = canvas.toDataURL("image/png");
  if (png.length < 250_000) return png;
  return canvas.toDataURL("image/jpeg", 0.85);
}

interface Props {
  botId: string;
  botName: string;
  isMaster?: boolean;
  initial: AvatarDraft;
  onSave(draft: AvatarDraft): Promise<void> | void;
  onClose(): void;
}

/**
 * 形象工作室弹窗：支持自由切换 12 种自建动态角色造型 × 12 种渐变配色，
 * 实时切换 9 种生命态预览，或上传自定义图片头像。
 */
export function AvatarStudioModal({
  botId,
  botName,
  isMaster = false,
  initial,
  onSave,
  onClose,
}: Props) {
  const defaultShape = resolveMascotShape(botId, initial.avatar_shape, isMaster).id;
  const defaultColor = resolveMascotColor(botId, initial.avatar_color, isMaster).id;
  const [shape, setShape] = useState<MascotShapeId>(defaultShape);
  const [color, setColor] = useState<MascotColorId>(defaultColor);
  const [image, setImage] = useState(initial.avatar_image);
  const [tab, setTab] = useState<"mascot" | "upload">(initial.avatar_image ? "upload" : "mascot");
  const [previewState, setPreviewState] = useState<MascotState>("idle");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const fileRef = useRef<HTMLInputElement>(null);

  const handleFile = async (file: File | undefined) => {
    if (!file) return;
    setError(null);
    try {
      const encoded = await cropAvatarFile(file);
      setImage(encoded);
      setTab("upload");
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    }
  };

  const handleSave = async () => {
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      await onSave({
        avatar_shape: shape,
        avatar_color: color,
        avatar_image: tab === "upload" ? image : "",
      });
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
      onMouseDown={(event) => event.target === event.currentTarget && !busy && onClose()}
    >
      <div
        className="sa-connect-panel sa-studio-modal"
        role="dialog"
        aria-modal="true"
        aria-label="自定义 Bot 形象"
      >
        <header className="sa-connect-head">
          <h2>{botName ? `「${botName}」的专属形象` : "定制 Bot 专属形象"}</h2>
          <button type="button" className="sa-link" disabled={busy} onClick={onClose}>
            关闭
          </button>
        </header>

        {/* 顶部实时动态预览台：可盯鼠标、可戳 360° 弹跳、可切换 9 种生命态预览 */}
        <div className="sa-studio-stage">
          <div className="sa-studio-preview">
            <BotMascot
              id={botId}
              name={botName || "Bot"}
              isMaster={isMaster}
              shape={shape}
              color={color}
              image={tab === "upload" ? image : ""}
              state={previewState}
              size={84}
              gaze
              interactive
            />
            <span className="sa-studio-hint">点我戳一下 / 移动鼠标</span>
          </div>
          <div className="sa-studio-states" aria-label="预览角色状态动效">
            {PREVIEW_STATES.map((item) => (
              <button
                key={item.id}
                type="button"
                className="sa-studio-state-btn"
                aria-pressed={previewState === item.id}
                onClick={() => setPreviewState(item.id)}
              >
                {item.label}
              </button>
            ))}
          </div>
        </div>

        <div className="sa-studio-tabs" role="tablist">
          <button
            type="button"
            role="tab"
            className="sa-studio-tab"
            aria-selected={tab === "mascot"}
            onClick={() => setTab("mascot")}
          >
            动态角色模型
          </button>
          <button
            type="button"
            role="tab"
            className="sa-studio-tab"
            aria-selected={tab === "upload"}
            onClick={() => setTab("upload")}
          >
            上传自定义图片
          </button>
        </div>

        {tab === "mascot" ? (
          <>
            <p className="sa-studio-section-label">选择造型（12 款）</p>
            <div className="sa-shape-grid">
              {MASCOT_SHAPES.map((item) => (
                <button
                  key={item.id}
                  type="button"
                  className="sa-shape-card"
                  aria-pressed={shape === item.id}
                  onClick={() => setShape(item.id)}
                >
                  <BotMascot
                    id={`${botId}-${item.id}`}
                    name={item.label}
                    shape={item.id}
                    color={color}
                    size={34}
                    showBadge={false}
                  />
                  <span>{item.label}</span>
                </button>
              ))}
            </div>

            <p className="sa-studio-section-label">选择渐变配色（12 色）</p>
            <div className="sa-color-grid">
              {MASCOT_COLORS.map((item) => (
                <button
                  key={item.id}
                  type="button"
                  className="sa-color-swatch"
                  title={item.label}
                  aria-label={item.label}
                  aria-pressed={color === item.id}
                  style={{
                    background: `linear-gradient(135deg, ${item.stops[0]}, ${item.stops[1]}, ${item.stops[2]})`,
                  }}
                  onClick={() => setColor(item.id)}
                />
              ))}
            </div>
          </>
        ) : (
          <div className="sa-upload-drop">
            <input
              ref={fileRef}
              type="file"
              accept="image/png,image/jpeg,image/webp"
              hidden
              onChange={(e) => void handleFile(e.target.files?.[0])}
            />
            <p className="sa-links-hint">
              支持 PNG / JPEG / WebP，自动居中裁成 160×160 方形头像并本地保存。
            </p>
            <div className="sa-connect-actions">
              <button
                type="button"
                className="sa-button sa-button-quiet"
                onClick={() => fileRef.current?.click()}
              >
                选择图片…
              </button>
              {image && (
                <button
                  type="button"
                  className="sa-button sa-button-quiet"
                  onClick={() => setImage("")}
                >
                  清除图片
                </button>
              )}
            </div>
          </div>
        )}

        {error && (
          <p className="sa-form-error" role="alert">
            {error}
          </p>
        )}

        <div className="sa-connect-actions">
          <button
            type="button"
            className="sa-button sa-button-quiet"
            disabled={busy}
            onClick={onClose}
          >
            取消
          </button>
          <button type="button" className="sa-button" disabled={busy} onClick={() => void handleSave()}>
            {busy ? "保存中…" : "应用形象"}
          </button>
        </div>
      </div>
    </div>
  );
}
