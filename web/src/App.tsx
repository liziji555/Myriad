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
  const [loadError, setLoadError] = useState<string | null>(null);
  const [retry, setRetry] = useState(0);

  useEffect(() => {
    api.health().then((health) => setVersion(health.version), () => {});
  }, []);

  // 选中的房间（没选时是第一个）。换房间、重新连上、片段丢了，都重新拉一次最新的消息。
  const roomId = (workspace.rooms.find((r) => r.id === selected) ?? workspace.rooms[0])?.id ?? null;
  const { openRoom, epoch } = workspace;
  useEffect(() => {
    if (!roomId) return;
    let stale = false;
    setLoadError(null);
    openRoom(roomId).catch((cause: unknown) => {
      if (!stale) setLoadError(cause instanceof Error ? cause.message : String(cause));
    });
    return () => {
      stale = true;
    };
  }, [roomId, epoch, retry, openRoom]);

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
  const roomChat = room ? workspace.chat.rooms[room.id] : undefined;
  // 有 Bot 在写、在挑人接话之前的空档里有人排着队，都算「正在回复」，可以点停止。
  const replying =
    (roomChat?.messages.some((message) => message.state === "streaming") ?? false) ||
    Boolean(roomChat?.activity?.speaking) ||
    Boolean(roomChat?.activity?.routing) ||
    (roomChat?.activity?.queued.length ?? 0) > 0;
  // 按进房间的先后排，和服务端一致。
  const members = room
    ? room.member_bot_ids.flatMap((id) => workspace.bots.filter((bot) => bot.id === id))
    : [];
  const queued = (roomChat?.activity?.queued ?? []).flatMap(
    (id) => workspace.bots.find((bot) => bot.id === id)?.name ?? [],
  );
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
            <Timeline
              key={room.id}
              chat={roomChat}
              bots={workspace.bots}
              loadError={loadError}
              onRetry={() => setRetry((n) => n + 1)}
              onLoadOlder={() => workspace.loadOlder(room.id)}
              onDecide={workspace.decideApproval}
              onLightDemo={runLightDemo}
            />
            {roomChat?.activity?.routing ? (
              <p className="sa-room-status" role="status">
                正在挑选由谁来回答…
              </p>
            ) : (
              queued.length > 0 && (
                <p className="sa-room-status" role="status">
                  接下来：{queued.join("、")}
                </p>
              )
            )}
            <Composer
              key={`composer-${room.id}`}
              disabledReason={room.host_bot_id ? null : "这个房间里还没有 Bot，发了也没人回应"}
              replying={replying}
              members={members.filter((bot) => bot.enabled)}
              onSend={(content) => workspace.sendMessage(room.id, content)}
              onStop={() => workspace.stopReply(room.id)}
            />
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
          bots={workspace.bots}
          team={workspace.team}
          activity={demoNodes}
        />
      )}
    </div>
  );
}
