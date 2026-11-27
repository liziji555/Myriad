import type { MascotState } from "../api/mascotShapes";
import type { PresenceState } from "../api/presence";
import { useBotById } from "./BotDirectoryContext";
import { BotMascot } from "./BotMascot";

/**
 * 统一的 Bot 形象入口：自动从 BotDirectory 读取该 Bot 的专属造型、配色、自定义图片与主控标识，
 * 渲染 Myriad 自建参数化动态角色模型，彻底替代旧版单字圆圈。
 */
export function BotAvatar({
  id,
  name,
  state,
  activeTool,
  size,
  gaze,
  interactive,
  showBadge,
  className,
}: {
  id: string;
  name: string;
  state?: PresenceState | "streaming" | MascotState;
  activeTool?: string;
  size?: number;
  gaze?: boolean;
  interactive?: boolean;
  showBadge?: boolean;
  className?: string;
}) {
  const bot = useBotById(id);
  const resolvedSize =
    size ??
    (className?.includes("sa-master-avatar")
      ? 48
      : className?.includes("sa-row-avatar")
        ? 30
        : 24);
  return (
    <BotMascot
      id={id}
      name={name}
      isMaster={bot?.is_master ?? false}
      shape={bot?.avatar_shape}
      color={bot?.avatar_color}
      image={bot?.avatar_image}
      state={state}
      activeTool={activeTool}
      size={resolvedSize}
      gaze={gaze}
      interactive={interactive}
      showBadge={showBadge}
      className={className}
    />
  );
}
