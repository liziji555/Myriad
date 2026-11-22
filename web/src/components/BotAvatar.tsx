import { hueOf, initialOf } from "../api/avatar";
import type { PresenceState } from "../api/presence";

/**
 * Bot 的头像：一个带颜色的圆，里面是名字的第一个字。
 * 同一个 Bot 颜色不变；给了状态时外面一圈跟着状态变（在忙、等你、停用）。
 */
export function BotAvatar({
  id,
  name,
  state,
  className,
}: {
  id: string;
  name: string;
  state?: PresenceState | "streaming";
  className?: string;
}) {
  return (
    <span
      className={className ? `sa-avatar ${className}` : "sa-avatar"}
      data-hue={hueOf(id)}
      data-state={state}
      aria-hidden="true"
    >
      {initialOf(name)}
    </span>
  );
}
