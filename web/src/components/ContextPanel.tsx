import type { TaskBoard } from "../api/tasks";
import type { TaskActions, Team } from "../api/useWorkspace";
import type { Bot, Room, TaskId } from "../api/types";
import { MembersPanel } from "./MembersPanel";
import { TasksPanel } from "./TasksPanel";
import { UsagePanel } from "./UsagePanel";

export type PanelTab = "tasks" | "members" | "usage" | "files" | "activity";

export interface ActivityNode {
  id: string;
  title: string;
  arrived: boolean;
}

const TABS: readonly { id: PanelTab; label: string }[] = [
  { id: "tasks", label: "任务" },
  { id: "members", label: "成员" },
  { id: "usage", label: "用量" },
  { id: "files", label: "文件" },
  { id: "activity", label: "活动" },
];

interface Props {
  tab: PanelTab;
  onTab(tab: PanelTab): void;
  onClose(): void;
  room: Room;
  members: readonly Bot[];
  bots: readonly Bot[];
  team: Team;
  board: TaskBoard;
  taskActions: TaskActions;
  /** 任务页里展开的是哪个任务。 */
  focusTask: TaskId | null;
  onFocusTask(taskId: TaskId | null): void;
  activity: readonly ActivityNode[];
  /** 变了就让「用量」页重新拉一次。 */
  usageKey: number;
}

export function ContextPanel({
  tab,
  onTab,
  onClose,
  room,
  members,
  bots,
  team,
  board,
  taskActions,
  focusTask,
  onFocusTask,
  activity,
  usageKey,
}: Props) {
  return (
    <aside className="sa-panel" aria-label="上下文面板">
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
        {tab === "usage" && <UsagePanel refreshKey={usageKey} />}
        {tab === "files" && <p className="sa-panel-empty">还没有文件。</p>}
        {tab === "members" && <MembersPanel room={room} members={members} bots={bots} team={team} />}
        {tab === "activity" &&
          (activity.length === 0 ? (
            <p className="sa-panel-empty">还没有活动。</p>
          ) : (
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
          ))}
      </div>
    </aside>
  );
}
