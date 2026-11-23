import { clampPanelWidth, panelWidthAt, PANEL_MAX, PANEL_MIN, PANEL_STEP } from "../api/layout";
import type { TaskBoard } from "../api/tasks";
import type { LinkActions, RoutineActions, TaskActions, Team } from "../api/useWorkspace";
import type { Bot, BotId, Link, Room, Routine, TaskId } from "../api/types";
import type { ExecCard } from "../api/execution";
import { AuditPanel } from "./AuditPanel";
import { ExecutionPanel } from "./ExecutionPanel";
import { ExtensionsPanel } from "./ExtensionsPanel";
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
  | "extensions"
  | "usage"
  | "audit"
  | "files"
  | "activity";

export interface ActivityNode {
  id: string;
  title: string;
  arrived: boolean;
}

const TABS: readonly { id: PanelTab; label: string }[] = [
  { id: "execution", label: "执行" },
  { id: "tasks", label: "任务" },
  { id: "routines", label: "定时" },
  { id: "members", label: "成员" },
  { id: "links", label: "连接" },
  { id: "extensions", label: "扩展" },
  { id: "usage", label: "用量" },
  { id: "audit", label: "审计" },
  { id: "files", label: "文件" },
  { id: "activity", label: "活动" },
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
  activity: readonly ActivityNode[];
  /** 变了就让「用量」页重新拉一次。 */
  usageKey: number;
  /** 「执行」页：每个 Bot 一张卡片。 */
  execution: readonly ExecCard[];
  onJump(card: ExecCard): void;
  /** 只停一个 Bot。 */
  onStopBot(botId: BotId): void;
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
  activity,
  usageKey,
  execution,
  onJump,
  onStopBot,
}: Props) {
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
      <div className="sa-tabs" role="tablist">
        {TABS.map(({ id, label }) => (
          <button
            key={id}
            type="button"
            role="tab"
            id={`sa-tab-${id}`}
            aria-selected={tab === id}
            aria-controls="sa-tabpanel"
            className="sa-tab"
            onClick={() => onTab(id)}
          >
            {label}
          </button>
        ))}
        <button type="button" className="sa-icon-button sa-panel-close" aria-label="收起侧栏" onClick={onClose}>
          ×
        </button>
      </div>
      <div className="sa-panel-body" role="tabpanel" id="sa-tabpanel" aria-labelledby={`sa-tab-${tab}`}>
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
            room={room}
            members={members.filter((bot) => bot.enabled)}
            bots={bots}
            routines={routines}
            actions={routineActions}
          />
        )}
        {tab === "links" && <LinksPanel room={room} links={links} actions={linkActions} />}
        {tab === "extensions" && <ExtensionsPanel />}
        {tab === "usage" && <UsagePanel refreshKey={usageKey} />}
        {tab === "audit" && (
          <AuditPanel
            key={room.id}
            room={room}
            task={(board.rooms[room.id] ?? []).find((task) => task.id === focusTask) ?? null}
          />
        )}
        {tab === "files" && <p className="sa-panel-empty">还没有文件。</p>}
        {tab === "members" && <MembersPanel room={room} members={members} bots={bots} team={team} />}
        {tab === "activity" && (
          <>
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
            {activity.length > 0 && (
              <ol className="sa-nodes">
                {activity.map((node) => (
                  <li key={node.id} className="sa-node" data-node-id={node.id} data-arrived={node.arrived}>
                    {!node.arrived && (
                      <svg className="light-running-border" aria-hidden="true">
                        <rect x="1" y="1" width="calc(100% - 2px)" height="calc(100% - 2px)" rx="8" pathLength="100" />
                      </svg>
                    )}
                    <span className="sa-row-label">{node.title}</span>
                    <span className="sa-row-meta">{node.arrived ? "已到达" : "飞行中"}</span>
                  </li>
                ))}
              </ol>
            )}
          </>
        )}
      </div>
    </aside>
  );
}
