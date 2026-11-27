import type { PresenceState } from "./presence";
import type { Bot } from "./types";

/** 12 种 Myriad 原创参数化角色造型（灵感源自 Grok Bot / DS-Bot，224x224 居中坐标系）。 */
export type MascotShapeId =
  | "orb"
  | "pebble"
  | "squircle"
  | "cloud"
  | "teardrop"
  | "hex"
  | "capsule"
  | "star"
  | "sprout"
  | "cat"
  | "shield"
  | "prism";

export type MascotColorId =
  | "obsidian"
  | "cobalt"
  | "cyan"
  | "emerald"
  | "amber"
  | "coral"
  | "rose"
  | "violet"
  | "indigo"
  | "mint"
  | "slate"
  | "aurora";

export type MascotMotionClass = "soft" | "bouncy" | "angular";

export type MascotState =
  | "idle"
  | "thinking"
  | "searching"
  | "working"
  | "sending"
  | "waiting"
  | "done"
  | "error"
  | "sleeping";

export interface MascotShapeDef {
  id: MascotShapeId;
  label: string;
  motion: MascotMotionClass;
  /** 224×224（中心 112,112）归一化 SVG 路径。 */
  path: string;
  /** 眼睛中心微调（相对于 112,112）。 */
  eyeOffsetY?: number;
}

export interface MascotColorDef {
  id: MascotColorId;
  label: string;
  /** 渐变起/中/止色（浅色模式 & 通用）。 */
  stops: readonly [string, string, string];
  /** 深色模式下可选覆盖色（如中枢 obsidian 自动切珍珠白）。 */
  darkStops?: readonly [string, string, string];
  /** 眼睛颜色（浅色 / 深色）。 */
  eyeLight: string;
  eyeDark: string;
  /** 强调光晕色（用于流光、微粒、选中框）。 */
  accent: string;
}

/**
 * 12 款自建参数化角色轮廓（全部光滑闭合、统一视觉重量）。
 * 坐标系：viewBox="-18 -18 260 260"，主体边界约在 [18..206] 之间，预留弹跳/倾角呼吸空间不裁切。
 */
export const MASCOT_SHAPES: readonly MascotShapeDef[] = [
  {
    id: "orb",
    label: "灵珠",
    motion: "soft",
    path: "M 112 20 C 166 20 206 56 204 112 C 202 169 164 205 112 204 C 57 203 19 168 20 112 C 21 57 59 20 112 20 Z",
  },
  {
    id: "pebble",
    label: "鹅卵",
    motion: "soft",
    path: "M 116 22 C 172 24 208 64 202 120 C 196 174 156 204 106 202 C 52 200 18 164 22 108 C 26 54 66 20 116 22 Z",
  },
  {
    id: "squircle",
    label: "方圆",
    motion: "bouncy",
    path: "M 112 22 C 180 22 202 44 202 112 C 202 180 180 202 112 202 C 44 202 22 180 22 112 C 22 44 44 22 112 22 Z",
  },
  {
    id: "cloud",
    label: "云团",
    motion: "soft",
    path: "M 78 40 C 96 18 134 18 150 42 C 178 38 204 60 202 88 C 214 112 204 146 182 158 C 180 188 152 206 122 198 C 102 210 70 202 58 178 C 30 176 14 148 22 120 C 12 94 26 62 52 54 C 58 44 68 40 78 40 Z",
    eyeOffsetY: 4,
  },
  {
    id: "teardrop",
    label: "水滴",
    motion: "soft",
    path: "M 112 16 C 142 46 200 94 200 138 C 200 180 162 206 112 206 C 62 206 24 180 24 138 C 24 94 82 46 112 16 Z",
    eyeOffsetY: 16,
  },
  {
    id: "hex",
    label: "六棱",
    motion: "angular",
    path: "M 112 18 C 124 18 184 52 192 64 C 200 76 200 148 192 160 C 184 172 124 206 112 206 C 100 206 40 172 32 160 C 24 148 24 76 32 64 C 40 52 100 18 112 18 Z",
  },
  {
    id: "capsule",
    label: "胶囊",
    motion: "bouncy",
    path: "M 112 18 C 160 18 188 46 188 94 L 188 130 C 188 178 160 206 112 206 C 64 206 36 178 36 130 L 36 94 C 36 46 64 18 112 18 Z",
  },
  {
    id: "star",
    label: "星芒",
    motion: "angular",
    path: "M 112 16 C 126 16 136 52 148 60 C 160 68 198 66 204 80 C 210 94 182 118 180 132 C 178 146 194 182 182 192 C 170 202 136 182 112 182 C 88 182 54 202 42 192 C 30 182 46 146 44 132 C 42 118 14 94 20 80 C 26 66 64 68 76 60 C 88 52 98 16 112 16 Z",
    eyeOffsetY: 8,
  },
  {
    id: "sprout",
    label: "萌芽",
    motion: "bouncy",
    path: "M 112 40 C 110 22 128 12 142 18 C 146 32 134 44 118 46 C 168 48 202 82 202 128 C 202 178 164 206 112 206 C 60 206 22 178 22 128 C 22 82 58 46 106 46 C 92 42 82 32 86 20 C 98 16 110 26 112 40 Z",
    eyeOffsetY: 14,
  },
  {
    id: "cat",
    label: "猫耳",
    motion: "bouncy",
    path: "M 54 24 C 66 20 82 36 92 44 C 105 42 119 42 132 44 C 142 36 158 20 170 24 C 184 28 188 60 186 78 C 198 94 204 114 202 134 C 198 180 162 206 112 206 C 62 206 26 180 22 134 C 20 114 26 94 38 78 C 36 60 40 28 54 24 Z",
    eyeOffsetY: 14,
  },
  {
    id: "shield",
    label: "徽章",
    motion: "angular",
    path: "M 112 20 C 154 20 192 34 196 54 C 200 74 198 124 184 156 C 170 188 134 206 112 206 C 90 206 54 188 40 156 C 26 124 24 74 28 54 C 32 34 70 20 112 20 Z",
    eyeOffsetY: -2,
  },
  {
    id: "prism",
    label: "棱镜",
    motion: "angular",
    path: "M 112 16 C 128 16 204 92 204 112 C 204 132 128 208 112 208 C 96 208 20 132 20 112 C 20 92 96 16 112 16 Z",
  },
];

export const MASCOT_COLORS: readonly MascotColorDef[] = [
  {
    id: "obsidian",
    label: "中枢黑白",
    stops: ["#545966", "#1f232c", "#0b0d12"],
    darkStops: ["#ffffff", "#e2e8f0", "#94a3b8"],
    eyeLight: "#f8fafc",
    eyeDark: "#11141a",
    accent: "#475569",
  },
  {
    id: "cobalt",
    label: "钴蓝",
    stops: ["#7bb6ff", "#2f74f5", "#1c4ed8"],
    eyeLight: "#ffffff",
    eyeDark: "#ffffff",
    accent: "#2f74f5",
  },
  {
    id: "cyan",
    label: "青瓷",
    stops: ["#67e8f9", "#0ea5c6", "#0e7490"],
    eyeLight: "#ffffff",
    eyeDark: "#ffffff",
    accent: "#0ea5c6",
  },
  {
    id: "emerald",
    label: "翠绿",
    stops: ["#6ee7b7", "#10b981", "#047857"],
    eyeLight: "#ffffff",
    eyeDark: "#ffffff",
    accent: "#10b981",
  },
  {
    id: "amber",
    label: "琥珀",
    stops: ["#fde047", "#f59e0b", "#c2690a"],
    eyeLight: "#ffffff",
    eyeDark: "#ffffff",
    accent: "#f59e0b",
  },
  {
    id: "coral",
    label: "珊瑚",
    stops: ["#fda4af", "#f45d6c", "#be123c"],
    eyeLight: "#ffffff",
    eyeDark: "#ffffff",
    accent: "#f45d6c",
  },
  {
    id: "rose",
    label: "樱粉",
    stops: ["#f9a8d4", "#ec4899", "#be185d"],
    eyeLight: "#ffffff",
    eyeDark: "#ffffff",
    accent: "#ec4899",
  },
  {
    id: "violet",
    label: "紫罗兰",
    stops: ["#c4b5fd", "#8b5cf6", "#6d28d9"],
    eyeLight: "#ffffff",
    eyeDark: "#ffffff",
    accent: "#8b5cf6",
  },
  {
    id: "indigo",
    label: "靛蓝",
    stops: ["#a5b4fc", "#6366f1", "#4338ca"],
    eyeLight: "#ffffff",
    eyeDark: "#ffffff",
    accent: "#6366f1",
  },
  {
    id: "mint",
    label: "薄荷",
    stops: ["#99f6e4", "#14b8a6", "#0f766e"],
    eyeLight: "#ffffff",
    eyeDark: "#ffffff",
    accent: "#14b8a6",
  },
  {
    id: "slate",
    label: "银灰",
    stops: ["#cbd5e1", "#64748b", "#334155"],
    eyeLight: "#ffffff",
    eyeDark: "#ffffff",
    accent: "#64748b",
  },
  {
    id: "aurora",
    label: "极光",
    stops: ["#a7f3d0", "#38bdf8", "#818cf8"],
    eyeLight: "#ffffff",
    eyeDark: "#ffffff",
    accent: "#38bdf8",
  },
];

const CHROMATIC_COLORS = MASCOT_COLORS.filter((c) => c.id !== "obsidian");

/** FNV-1a 确定性哈希，保证每个 Bot 拥有稳定专属造型、配色与错峰动画相位。 */
export function hashSeed(seed: string): number {
  let h = 2166136261 >>> 0;
  for (let i = 0; i < seed.length; i++) {
    h ^= seed.charCodeAt(i);
    h = Math.imul(h, 16777619) >>> 0;
  }
  return h >>> 0;
}

export function resolveMascotShape(
  id: string,
  shape?: string | null,
  isMaster?: boolean,
): MascotShapeDef {
  if (shape) {
    const exact = MASCOT_SHAPES.find((item) => item.id === shape);
    if (exact) return exact;
  }
  if (isMaster) return MASCOT_SHAPES[0]!;
  const h = hashSeed(`shape:${id}`);
  return MASCOT_SHAPES[h % MASCOT_SHAPES.length]!;
}

export function resolveMascotColor(
  id: string,
  color?: string | null,
  isMaster?: boolean,
): MascotColorDef {
  if (color) {
    const exact = MASCOT_COLORS.find((item) => item.id === color);
    if (exact) return exact;
  }
  if (isMaster) return MASCOT_COLORS[0]!;
  const h = hashSeed(`color:${id}`);
  return CHROMATIC_COLORS[h % CHROMATIC_COLORS.length]!;
}

export function mascotTimingFor(id: string): {
  delaySec: number;
  speed: number;
  gazeDelayMs: number;
} {
  const h = hashSeed(`timing:${id}`);
  const delaySec = -((h % 40) * 0.11);
  const speed = 0.9 + ((h >>> 8) % 26) * 0.01;
  const gazeDelayMs = 140 + ((h >>> 16) % 220);
  return { delaySec, speed, gazeDelayMs };
}

/** 根据 Myriad 运行状态与当前正在执行的工具名，映射到具体的动态角色状态。 */
export function mascotStateFromPresence(
  state?: PresenceState | "streaming" | MascotState,
  activeTool?: string,
): MascotState {
  if (!state) return "idle";
  if (
    state === "idle" ||
    state === "thinking" ||
    state === "searching" ||
    state === "working" ||
    state === "sending" ||
    state === "waiting" ||
    state === "done" ||
    state === "error" ||
    state === "sleeping"
  ) {
    return state;
  }
  if (state === "off") return "sleeping";
  if (state === "approval" || state === "asking") return "waiting";
  if (state === "queued") return "thinking";
  if (state === "writing" || state === "streaming") return "thinking";
  if (state === "tool") {
    const tool = (activeTool ?? "").toLowerCase();
    if (/read|grep|glob|list|search|fetch|web|memory_read|memory_list/.test(tool)) {
      return "searching";
    }
    if (/ask_bot|create_bot|update_bot|create_room|send_to_bot/.test(tool)) {
      return "sending";
    }
    return "working";
  }
  return "idle";
}

export function botLookOf(bot: Pick<Bot, "id" | "is_master" | "avatar_shape" | "avatar_color" | "avatar_image">) {
  return {
    shape: resolveMascotShape(bot.id, bot.avatar_shape, bot.is_master),
    color: resolveMascotColor(bot.id, bot.avatar_color, bot.is_master),
    image: bot.avatar_image || "",
  };
}
