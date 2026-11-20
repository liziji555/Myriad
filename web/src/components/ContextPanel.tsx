import type { Bot, Room } from "../api/types";

export type PanelTab = "tasks" | "members" | "files" | "activity";

export interface ActivityNode {
  id: string;
  title: string;
  arrived: boolean;
}

const TABS: readonly { id: PanelTab; label: string }[] = [
  { id: "tasks", label: "任务" },
  { id: "members", label: "成员" },
  { id: "files", label: "文件" },
  { id: "activity", label: "活动" },
];

interface Props {
  tab: PanelTab;
  onTab(tab: PanelTab): void;
  onClose(): void;
  room: Room;
  members: readonly Bot[];
  activity: readonly ActivityNode[];
}

export function ContextPanel({ tab, onTab, onClose, room, members, activity }: Props) {
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
        {tab === "tasks" && <p className="sa-panel-empty">还没有任务。</p>}
        {tab === "files" && <p className="sa-panel-empty">还没有文件。</p>}
        {tab === "members" &&
          (members.length === 0 ? (
            <p className="sa-panel-empty">这个房间还没有 Bot。</p>
          ) : (
            <ul className="sa-list">
              {members.map((bot) => (
                <li key={bot.id} className="sa-row sa-row-static">
                  <span className="sa-dot" data-state={bot.enabled ? "idle" : "off"} aria-hidden="true" />
                  <span className="sa-row-label">{bot.name}</span>
                  {bot.id === room.host_bot_id && <span className="sa-tag">入口</span>}
                </li>
              ))}
            </ul>
          ))}
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
