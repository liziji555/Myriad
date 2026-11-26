import type { ReactNode } from "react";

export type IconName =
  | "search"
  | "panel"
  | "tasks"
  | "sun"
  | "moon"
  | "auto"
  | "plus"
  | "send"
  | "stop"
  | "alert"
  | "read"
  | "edit"
  | "command"
  | "git"
  | "web"
  | "bot"
  | "plug"
  | "tool"
  | "sidebar"
  | "clock"
  | "users"
  | "link"
  | "chart"
  | "shield"
  | "file"
  | "graph"
  | "server"
  | "split"
  | "play"
  | "undo";

/** 16×16 的线条图标，颜色跟着文字走。 */
const PATHS: Record<IconName, ReactNode> = {
  search: (
    <>
      <circle cx="7" cy="7" r="4.25" />
      <path d="m10.2 10.2 3.3 3.3" />
    </>
  ),
  panel: (
    <>
      <rect x="2" y="3" width="12" height="10" rx="2.5" />
      <path d="M10 3v10" />
    </>
  ),
  tasks: (
    <>
      <path d="m2.5 4.5 1.2 1.2 2-2.2M2.5 10.5l1.2 1.2 2-2.2" />
      <path d="M8.5 4.8h5M8.5 10.8h5" />
    </>
  ),
  sun: (
    <>
      <circle cx="8" cy="8" r="2.75" />
      <path d="M8 1.75v1.5M8 12.75v1.5M1.75 8h1.5M12.75 8h1.5M3.6 3.6l1.05 1.05M11.35 11.35l1.05 1.05M3.6 12.4l1.05-1.05M11.35 4.65l1.05-1.05" />
    </>
  ),
  moon: <path d="M13.2 9.6A5.6 5.6 0 0 1 6.4 2.8a5.6 5.6 0 1 0 6.8 6.8Z" />,
  auto: (
    <>
      <circle cx="8" cy="8" r="5.75" />
      <path d="M8 2.25v11.5A5.75 5.75 0 0 0 8 2.25Z" fill="currentColor" stroke="none" />
    </>
  ),
  plus: <path d="M8 3v10M3 8h10" />,
  send: <path d="M8 13V3.5M3.75 7.5 8 3.25l4.25 4.25" />,
  stop: <rect x="4.25" y="4.25" width="7.5" height="7.5" rx="1.5" fill="currentColor" stroke="none" />,
  alert: (
    <>
      <path d="M8 2.3 14 13H2L8 2.3Z" />
      <path d="M8 6.5v3M8 11.2v.1" />
    </>
  ),
  read: (
    <>
      <path d="M4 2.5h5l3 3v8H4v-11Z" />
      <path d="M9 2.5v3h3M6 8.5h4M6 11h4" />
    </>
  ),
  edit: (
    <>
      <path d="m3 13 .6-2.8 6.9-6.9 2.2 2.2-6.9 6.9L3 13Z" />
      <path d="m9.3 4.5 2.2 2.2" />
    </>
  ),
  command: (
    <>
      <rect x="2" y="3" width="12" height="10" rx="2.5" />
      <path d="m5 6.5 2 1.75-2 1.75M8.5 10.25h2.5" />
    </>
  ),
  git: (
    <>
      <circle cx="4.5" cy="4" r="1.5" />
      <circle cx="4.5" cy="12" r="1.5" />
      <circle cx="11.5" cy="6" r="1.5" />
      <path d="M4.5 5.5v5M11.5 7.5c0 2-2.5 2-4.5 2.2-1.4.2-2.5.5-2.5.8" />
    </>
  ),
  web: (
    <>
      <circle cx="8" cy="8" r="5.75" />
      <path d="M2.25 8h11.5M8 2.25c1.7 1.6 2.5 3.5 2.5 5.75S9.7 12.15 8 13.75C6.3 12.15 5.5 10.25 5.5 8S6.3 3.85 8 2.25Z" />
    </>
  ),
  bot: (
    <>
      <rect x="3" y="5.5" width="10" height="7.5" rx="2.5" />
      <path d="M8 5.5V3M6.25 9v.5M9.75 9v.5" />
    </>
  ),
  plug: <path d="M6 2.5v3M10 2.5v3M4.5 5.5h7v2.25a3.5 3.5 0 0 1-7 0V5.5ZM8 11.25v2.25" />,
  tool: (
    <>
      <circle cx="8" cy="8" r="1.75" />
      <path d="M8 2.25v2M8 11.75v2M2.25 8h2M11.75 8h2" />
    </>
  ),
  sidebar: (
    <>
      <rect x="2" y="3" width="12" height="10" rx="2.5" />
      <path d="M6 3v10" />
    </>
  ),
  clock: (
    <>
      <circle cx="8" cy="8" r="5.75" />
      <path d="M8 4.75V8l2.25 1.5" />
    </>
  ),
  users: (
    <>
      <circle cx="6" cy="5.75" r="2.25" />
      <path d="M2.25 13c.4-2.2 1.9-3.5 3.75-3.5S9.35 10.8 9.75 13" />
      <path d="M10.5 3.75a2.25 2.25 0 0 1 0 4M11.25 9.6c1.3.4 2.2 1.6 2.5 3.4" />
    </>
  ),
  link: (
    <>
      <path d="M7 9a2.75 2.75 0 0 0 3.9.1l2-2a2.75 2.75 0 0 0-3.9-3.9l-.7.7" />
      <path d="M9 7a2.75 2.75 0 0 0-3.9-.1l-2 2a2.75 2.75 0 0 0 3.9 3.9l.7-.7" />
    </>
  ),
  chart: <path d="M2.5 13.5h11M4.5 11V8M8 11V4.5M11.5 11V6.5" />,
  shield: (
    <>
      <path d="M8 2 13 3.75v3.9c0 3-2.1 5.2-5 6.35-2.9-1.15-5-3.35-5-6.35v-3.9L8 2Z" />
      <path d="m5.75 8 1.5 1.5 3-3" />
    </>
  ),
  file: (
    <>
      <path d="M2.5 4.5a1.5 1.5 0 0 1 1.5-1.5h2.5l1.5 1.5h4a1.5 1.5 0 0 1 1.5 1.5v5.5a1.5 1.5 0 0 1-1.5 1.5H4a1.5 1.5 0 0 1-1.5-1.5v-7Z" />
    </>
  ),
  server: (
    <>
      <rect x="2.5" y="2.75" width="11" height="4.5" rx="1.25" />
      <rect x="2.5" y="8.75" width="11" height="4.5" rx="1.25" />
      <path d="M5 5h.01M5 11h.01" />
    </>
  ),
  graph: (
    <>
      <circle cx="4" cy="4" r="1.5" />
      <circle cx="12" cy="8" r="1.5" />
      <circle cx="4" cy="12" r="1.5" />
      <path d="M5.4 4.6 10.6 7.4M5.4 11.4l5.2-2.8" />
    </>
  ),
  split: (
    <>
      <rect x="2" y="2.75" width="12" height="10.5" rx="2" />
      <path d="M8.5 2.75v10.5" />
    </>
  ),
  play: <path d="M5 3.5 12.5 8 5 12.5V3.5Z" fill="currentColor" stroke="none" />,
  undo: (
    <>
      <path d="M4.25 6.25H2.5V4.5" />
      <path d="M2.75 6.25A5.5 5.5 0 1 1 3.75 11.5" />
    </>
  ),
};

export function Icon({ name, size = 16 }: { name: IconName; size?: number }) {
  return (
    <svg
      className="sa-icon"
      width={size}
      height={size}
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.4"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      {PATHS[name]}
    </svg>
  );
}
