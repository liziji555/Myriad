import { useEffect, useRef, useState } from "react";
import { clampPanelWidth, panelWidthAt, PANEL_MAX, PANEL_MIN, PANEL_STEP } from "../api/layout";
import type { TaskBoard } from "../api/tasks";
import type { LinkActions, RoutineActions, TaskActions, Team } from "../api/useWorkspace";
import type { Bot, BotId, CheckpointInfo, Link, RewindReport, Room, Routine, TaskId } from "../api/types";
import type { ExecCard } from "../api/execution";
import { AuditPanel } from "./AuditPanel";
import { ExecutionPanel } from "./ExecutionPanel";
import { ExtensionsPanel } from "./ExtensionsPanel";
import { FilesPanel } from "./FilesPanel";
import { Icon, type IconName } from "./Icon";
import { ModelsPanel } from "./ModelsPanel";
import { LinksPanel } from "./LinksPanel";
import { MembersPanel } from "./MembersPanel";
import { RoutinesPanel } from "./RoutinesPanel";
import { TaskGraph } from "./TaskGraph";
import { TasksPanel } from "./TasksPanel";
import { UsagePanel } from "./UsagePanel";

export type PanelTab =
  | "execution"
  | "tasks"
  | "routines"
  | "members"
  | "links"
  | "models"
  | "extensions"
  | "usage"
  | "audit"
  | "files"
  | "activity";

const TABS: readonly { id: PanelTab; label: string; icon: IconName }[] = [
  { id: "execution", label: "执行态势", icon: "graph" },
  { id: "tasks", label: "房间任务", icon: "tasks" },
  { id: "activity", label: "任务流线图", icon: "graph" },
  { id: "routines", label: "定时任务", icon: "clock" },
  { id: "members", label: "队员和成员", icon: "users" },
  { id: "files", label: "文件与改动", icon: "file" },
  { id: "links", label: "外部连接", icon: "link" },
  { id: "models", label: "模型与档位", icon: "bot" },
  { id: "extensions", label: "技能和 MCP", icon: "plug" },
  { id: "usage", label: "用量与预算", icon: "chart" },
  { id: "audit", label: "审计日志", icon: "shield" },
];

interface Props {
  tab: PanelTab;
  onTab(tab: PanelTab): void;
  onClose(): void;
  /** 右栏现在多宽（像素）。 */
  width: number;
  onResize(width: number): void;
  room: Room;
  members: readonly Bot[];
  bots: readonly Bot[];
  team: Team;
  board: TaskBoard;
  taskActions: TaskActions;
  /** 任务页里展开的是哪个任务。 */
  focusTask: TaskId | null;
  onFocusTask(taskId: TaskId | null): void;
  /** 这个房间的定时任务。 */
  routines: readonly Routine[];
  routineActions: RoutineActions;
  /** 这个房间和外面的连接。 */
  links: readonly Link[];
  linkActions: LinkActions;
  /** 变了就让「用量」页重新拉一次。 */
  usageKey: number;
  /** 「执行」页：每个 Bot 一张卡片。 */
  execution: readonly ExecCard[];
  onJump(card: ExecCard): void;
  /** 只停一个 Bot。 */
  onStopBot(botId: BotId): void;
  /** 房间内内置 Bot 的轮次级文件检查点。 */
  checkpoints?: readonly CheckpointInfo[];
  onRewindCheckpoint?(checkpointId: string, force: boolean): Promise<RewindReport>;
  onJumpMessage?(messageId: string): Promise<boolean> | boolean | void;
  /** 命令面板挂起的扩展热重载请求（消费一次即清零）。 */
  extensionsReloadPending?: boolean;
  onConsumeExtensionsReload?(): void;
}

export function ContextPanel({
  tab,
  onTab,
  onClose,
  width,
  onResize,
  room,
  members,
  bots,
  team,
  board,
  taskActions,
  focusTask,
  onFocusTask,
  routines,
  routineActions,
  links,
  linkActions,
  usageKey,
  execution,
  onJump,
  onStopBot,
  checkpoints,
  onRewindCheckpoint,
  onJumpMessage,
  extensionsReloadPending,
  onConsumeExtensionsReload,
}: Props) {
  const [menuOpen, setMenuOpen] = useState(false);
  const pickerRef = useRef<HTMLDivElement>(null);
  const current = TABS.find((item) => item.id === tab) ?? TABS[0]!;

  useEffect(() => {
    if (!menuOpen) return;
    const onDown = (event: MouseEvent) => {
      if (!pickerRef.current?.contains(event.target as Node)) setMenuOpen(false);
    };
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") setMenuOpen(false);
    };
    window.addEventListener("mousedown", onDown);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("mousedown", onDown);
      window.removeEventListener("keydown", onKey);
    };
  }, [menuOpen]);

  return (
    <aside className="sa-panel" aria-label="上下文面板">
      <div
        className="sa-panel-resizer"
        role="separator"
        aria-orientation="vertical"
        aria-label="调整面板宽度，可以拖，也可以用左右方向键"
        aria-valuemin={PANEL_MIN}
        aria-valuemax={PANEL_MAX}
        aria-valuenow={width}
        tabIndex={0}
        onPointerDown={(event) => {
          event.preventDefault();
          event.currentTarget.setPointerCapture(event.pointerId);
        }}
        onPointerMove={(event) => {
          if (!event.currentTarget.hasPointerCapture(event.pointerId)) return;
          onResize(panelWidthAt(event.clientX, window.innerWidth));
        }}
        onPointerUp={(event) => event.currentTarget.releasePointerCapture(event.pointerId)}
        onKeyDown={(event) => {
          const next =
            event.key === "ArrowLeft"
              ? width + PANEL_STEP
              : event.key === "ArrowRight"
                ? width - PANEL_STEP
                : event.key === "Home"
                  ? PANEL_MAX
                  : event.key === "End"
                    ? PANEL_MIN
                    : null;
          if (next === null) return;
          event.preventDefault();
          onResize(clampPanelWidth(next));
        }}
      />

      {/* 单行清爽面板头：取代原先挤成两行的 11 个页签按钮 */}
      <header className="sa-panel-head">
        <div ref={pickerRef} className="sa-panel-picker">
          <button
            type="button"
            className="sa-panel-trigger"
            aria-haspopup="listbox"
            aria-expanded={menuOpen}
            onClick={() => setMenuOpen((open) => !open)}
            title="点击快速切换工作台面板"
          >
            <Icon name={current.icon} size={15} />
            <span className="sa-panel-title">{current.label}</span>
            <span className="sa-stage-caret" aria-hidden="true" />
          </button>

          {menuOpen && (
            <ul className="sa-panel-menu" role="listbox" aria-label="切换面板">
              {TABS.map((item) => (
                <li key={item.id}>
                  <button
                    type="button"
                    role="option"
                    aria-selected={item.id === tab}
                    className="sa-panel-menu-item"
                    onClick={() => {
                      onTab(item.id);
                      setMenuOpen(false);
                    }}
                  >
                    <Icon name={item.icon} size={14} />
                    <span>{item.label}</span>
                  </button>
                </li>
              ))}
            </ul>
          )}
        </div>

        {(tab === "tasks" || tab === "activity") && (
          <div className="sa-panel-subswitch" role="tablist" aria-label="任务视图">
            <button
              type="button"
              role="tab"
              aria-selected={tab === "tasks"}
              onClick={() => onTab("tasks")}
            >
              列表
            </button>
            <button
              type="button"
              role="tab"
              aria-selected={tab === "activity"}
              onClick={() => onTab("activity")}
            >
              流线图
            </button>
          </div>
        )}

        <button type="button" className="sa-icon-button sa-panel-close" aria-label="收起侧栏" onClick={onClose}>
          ×
        </button>
      </header>

      <div className="sa-panel-body" role="region" aria-label={current.label}>
        {tab === "execution" && (
          <ExecutionPanel
            cards={execution}
            onJump={onJump}
            onStop={(card) => onStopBot(card.botId)}
            onShowTask={(taskId) => {
              onFocusTask(taskId);
              onTab("tasks");
            }}
          />
        )}
        {tab === "tasks" && (
          <TasksPanel
            key={room.id}
            room={room}
            board={board}
            members={members.filter((bot) => bot.enabled)}
            bots={bots}
            actions={taskActions}
            focus={focusTask}
            onFocus={onFocusTask}
          />
        )}
        {tab === "routines" && (
          <RoutinesPanel
            key={room.id}
            room={room}
            members={members.filter((bot) => bot.enabled)}
            bots={bots}
            routines={routines}
            actions={routineActions}
          />
        )}
        {tab === "links" && <LinksPanel key={room.id} room={room} links={links} actions={linkActions} />}
        {tab === "models" && <ModelsPanel />}
        {tab === "extensions" && (
          <ExtensionsPanel
            pendingReload={extensionsReloadPending}
            onConsumeReload={onConsumeExtensionsReload}
          />
        )}
        {tab === "usage" && <UsagePanel refreshKey={usageKey} />}
        {tab === "audit" && (
          <AuditPanel
            key={room.id}
            room={room}
            task={(board.rooms[room.id] ?? []).find((task) => task.id === focusTask) ?? null}
          />
        )}
        {tab === "files" && (
          <FilesPanel
            key={room.id}
            room={room}
            members={members}
            checkpoints={checkpoints}
            onRewindCheckpoint={onRewindCheckpoint}
            onJumpMessage={onJumpMessage}
          />
        )}
        {tab === "members" && <MembersPanel key={room.id} room={room} members={members} bots={bots} team={team} />}
        {tab === "activity" && (
          <TaskGraph
            key={room.id}
            tasks={board.rooms[room.id] ?? []}
            runs={board.runs}
            focus={focusTask}
            onShowTask={(taskId) => {
              onFocusTask(taskId);
              onTab("tasks");
            }}
            loadRuns={taskActions.loadRuns}
          />
        )}
      </div>
    </aside>
  );
}
