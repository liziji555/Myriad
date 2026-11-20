import { useEffect, useState } from "react";
import { api } from "./api/client";
import type { RoomId } from "./api/types";
import { useWorkspace } from "./api/useWorkspace";
import { Composer } from "./components/Composer";
import { ContextPanel, type ActivityNode, type PanelTab } from "./components/ContextPanel";
import { RoomHeader } from "./components/RoomHeader";
import { Sidebar } from "./components/Sidebar";
import { Timeline } from "./components/Timeline";
import { LightProvider, useLight } from "./light/LightProvider";

export function App() {
  return (
    <LightProvider>
      <Shell />
    </LightProvider>
  );
}

function Shell() {
  const workspace = useWorkspace();
  const emit = useLight();
  const [selected, setSelected] = useState<RoomId | null>(null);
  const [tab, setTab] = useState<PanelTab>("tasks");
  const [panelOpen, setPanelOpen] = useState(true);
  const [demoNodes, setDemoNodes] = useState<ActivityNode[]>([]);
  const [version, setVersion] = useState("");

  useEffect(() => {
    api.health().then((health) => setVersion(health.version), () => {});
  }, []);

  if (workspace.phase === "unauthorized") {
    return (
      <main className="sa-gate">
        <h1>需要访问令牌</h1>
        <p>
          请回到运行 <code>sa serve</code> 的窗口，用它打印的链接重新打开这个页面。
        </p>
      </main>
    );
  }
  if (workspace.phase === "loading") {
    return (
      <main className="sa-gate">
        <p>{workspace.connected ? "正在加载…" : "正在连接本机服务…"}</p>
      </main>
    );
  }

  const room = workspace.rooms.find((r) => r.id === selected) ?? workspace.rooms[0];
  const members = room ? workspace.bots.filter((bot) => room.member_bot_ids.includes(bot.id)) : [];
  const showPanel = (next: PanelTab) => {
    setTab(next);
    setPanelOpen(true);
  };

  // 演示：先把节点放进右栏，再让光线从按钮飞到这个节点。
  const runLightDemo = (source: HTMLElement) => {
    const id = `demo-${Date.now()}`;
    setDemoNodes((nodes) => [...nodes, { id, title: "演示节点", arrived: false }]);
    showPanel("activity");
    requestAnimationFrame(async () => {
      const result = await emit({
        id,
        source,
        target: () => document.querySelector(`[data-node-id="${id}"]`),
        color: getComputedStyle(document.documentElement).getPropertyValue("--sa-accent").trim(),
      });
      setDemoNodes((nodes) =>
        result === "arrived"
          ? nodes.map((node) => (node.id === id ? { ...node, arrived: true } : node))
          : nodes.filter((node) => node.id !== id),
      );
    });
  };

  return (
    <div className="sa-shell" data-panel-open={panelOpen}>
      <Sidebar
        rooms={workspace.rooms}
        bots={workspace.bots}
        selectedRoomId={room?.id ?? null}
        connected={workspace.connected}
        version={version}
        onSelect={setSelected}
        onCreateRoom={async (name) => setSelected((await workspace.createRoom(name)).id)}
      />
      <main className="sa-main">
        {room ? (
          <>
            <RoomHeader
              room={room}
              memberCount={members.length}
              panelOpen={panelOpen}
              onShowMembers={() => showPanel("members")}
              onTogglePanel={() => setPanelOpen((open) => !open)}
            />
            <Timeline onLightDemo={runLightDemo} />
            <Composer />
          </>
        ) : (
          <div className="sa-empty">
            <h2>还没有房间</h2>
            <p>点左上角的 + 新建一个。</p>
          </div>
        )}
      </main>
      {room && (
        <ContextPanel
          tab={tab}
          onTab={setTab}
          onClose={() => setPanelOpen(false)}
          room={room}
          members={members}
          activity={demoNodes}
        />
      )}
    </div>
  );
}
