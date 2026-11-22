import { useEffect, useState, type CSSProperties } from "react";
import { loadLayout, saveLayout, type Layout } from "./api/layout";
import type { SlashCommand } from "./api/slash";
import { presenceOf, type Presence } from "./api/presence";
import { STATE_LABEL } from "./api/tasks";
import { api } from "./api/client";
import type { RoomId, TaskId } from "./api/types";
import { useWorkspace } from "./api/useWorkspace";
import { noticesFor } from "./api/workspace";
import { CommandPalette, type PaletteEntry } from "./components/CommandPalette";
import { Composer } from "./components/Composer";
import { ContextPanel, type ActivityNode, type PanelTab } from "./components/ContextPanel";
import { Notices } from "./components/Notices";
import { applyTheme, loadTheme, nextTheme } from "./api/theme";
import { Sidebar } from "./components/Sidebar";
import { StageBar } from "./components/StageBar";
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
  const [roomsOpen, setRoomsOpen] = useState(false);
  const [prefill, setPrefill] = useState<string | null>(null);
  const [layout, setLayout] = useState(loadLayout);
  const panelOpen = layout.panelOpen;
  const changeLayout = (patch: Partial<Layout>) =>
    setLayout((current) => {
      const next = { ...current, ...patch };
      saveLayout(next);
      return next;
    });
  const [focusTask, setFocusTask] = useState<TaskId | null>(null);
  const [demoNodes, setDemoNodes] = useState<ActivityNode[]>([]);
  const [version, setVersion] = useState("");
  const [loadError, setLoadError] = useState<string | null>(null);
  const [retry, setRetry] = useState(0);
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [theme, setTheme] = useState(loadTheme);
  const [createSignal, setCreateSignal] = useState(0);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if ((event.ctrlKey || event.metaKey) && !event.altKey && event.key.toLowerCase() === "k") {
        event.preventDefault();
        setPaletteOpen((open) => !open);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

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
  // 任务的运行过程不算：它有自己的停止按钮，不占着输入框。
  const replying =
    (roomChat?.messages.some((message) => message.state === "streaming" && message.kind === "chat") ??
      false) ||
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
  const setPanelOpen = (open: boolean) => changeLayout({ panelOpen: open });
  const showPanel = (next: PanelTab) => {
    setTab(next);
    setPanelOpen(true);
  };
  const roomTasks = room ? (workspace.board.rooms[room.id] ?? []) : [];
  const presence = presenceOf(members, roomChat, roomTasks);
  const pendingTasks = roomTasks.filter((task) => ["review", "waiting", "blocked"].includes(task.state)).length;
  // 点了现场栏上的牌子：主线上有那条消息就滚过去闪一下，否则打开那个任务。
  const jumpTo = (row: Presence) => {
    const target = row.messageId && document.querySelector(`[data-message-id="${row.messageId}"]`);
    if (target) {
      target.scrollIntoView({ block: "center", behavior: "smooth" });
      target.setAttribute("data-flash", "true");
      window.setTimeout(() => target.removeAttribute("data-flash"), 1600);
    } else if (row.taskId) {
      setFocusTask(row.taskId);
      showPanel("tasks");
    }
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

  const tabAction = (id: PanelTab, title: string): PaletteEntry => ({
    id: `action:tab-${id}`,
    group: "action",
    title,
    run: () => showPanel(id),
  });
  const paletteEntries: PaletteEntry[] = [
    ...workspace.rooms.map(
      (r): PaletteEntry => ({
        id: `room:${r.id}`,
        group: "room",
        title: r.name,
        hint: r.id === room?.id ? "正开着" : undefined,
        run: () => setSelected(r.id),
      }),
    ),
    ...workspace.bots.map(
      (bot): PaletteEntry => ({
        id: `bot:${bot.id}`,
        group: "bot",
        title: bot.name,
        hint: bot.persona.split("\n")[0] || (bot.enabled ? undefined : "已停用"),
        run: () => showPanel("members"),
      }),
    ),
    // 只有打开过的房间的任务在手上。
    ...Object.values(workspace.board.rooms)
      .flat()
      .map(
        (task): PaletteEntry => ({
          id: `task:${task.id}`,
          group: "task",
          title: task.title,
          hint: [workspace.rooms.find((r) => r.id === task.room_id)?.name, STATE_LABEL[task.state]]
            .filter(Boolean)
            .join(" · "),
          run: () => {
            setSelected(task.room_id);
            setFocusTask(task.id);
            showPanel("tasks");
          },
        }),
      ),
    {
      id: "action:new-room",
      group: "action",
      title: "新建房间",
      run: () => {
        // 填名字的地方在房间列表里：先把它打开。
        setRoomsOpen(true);
        setCreateSignal((n) => n + 1);
      },
    },
    ...(room
      ? [
          tabAction("tasks", "打开任务页"),
          tabAction("routines", "打开定时任务"),
          tabAction("members", "管理 Bot 和房间成员"),
          tabAction("links", "连接 GitHub 仓库和 Telegram 聊天"),
          tabAction("extensions", "看技能和 MCP 服务"),
          tabAction("usage", "看用量和花费"),
          tabAction("audit", "看审计日志：谁批准了什么、做了什么"),
          tabAction("activity", "看任务图"),
          {
            id: "action:toggle-sidebar",
            group: "action" as const,
            title: roomsOpen ? "收起房间列表" : "打开房间列表",
            run: () => setRoomsOpen((open) => !open),
          },
          {
            id: "action:toggle-panel",
            group: "action" as const,
            title: panelOpen ? "收起面板" : "打开面板",
            run: () => setPanelOpen(!panelOpen),
          },
        ]
      : []),
    ...(room && replying
      ? [
          {
            id: "action:stop",
            group: "action" as const,
            title: "停止正在进行的回复",
            run: () => void workspace.stopReply(room.id),
          },
        ]
      : []),
  ];

  // 输入框里 `/` 列出来的命令：就是命令面板里的那些动作，分成「面板」和「动作」两组。
  const slashCommands: SlashCommand[] = paletteEntries
    .filter((entry) => entry.group === "action")
    .map((entry) => ({
      id: entry.id,
      group: entry.id.startsWith("action:tab-") ? "面板" : "动作",
      title: entry.title,
      run: entry.run,
    }))
    .sort((a, b) => Number(b.group === "面板") - Number(a.group === "面板"));

  return (
    <div
      className="sa-shell"
      data-panel-open={panelOpen}
      // 没有房间时房间列表总是开着：新建房间的入口在那里。
      data-rooms={roomsOpen || !room ? "open" : "closed"}
      style={{ "--sa-panel-width": `${layout.panelWidth}px` } as CSSProperties}
    >
      {paletteOpen && <CommandPalette entries={paletteEntries} onClose={() => setPaletteOpen(false)} />}
      <StageBar
        room={room}
        presence={presence}
        connected={workspace.connected}
        pendingTasks={pendingTasks}
        roomsOpen={roomsOpen || !room}
        drawerOpen={panelOpen}
        onToggleRooms={() => setRoomsOpen((open) => !open)}
        onToggleDrawer={() => setPanelOpen(!panelOpen)}
        onShowTasks={() => showPanel("tasks")}
        onOpenPalette={() => setPaletteOpen(true)}
        theme={theme}
        onCycleTheme={() => {
          const next = nextTheme(theme);
          applyTheme(next);
          setTheme(next);
        }}
        onPick={jumpTo}
      />
      {(roomsOpen || !room) && (
        <>
          {room && <div className="sa-scrim" onClick={() => setRoomsOpen(false)} />}
          <Sidebar
            rooms={workspace.rooms}
            bots={workspace.bots}
            selectedRoomId={room?.id ?? null}
            connected={workspace.connected}
            version={version}
            onSelect={(id) => {
              setSelected(id);
              setRoomsOpen(false);
            }}
            onCreateRoom={async (name) => {
              setSelected((await workspace.createRoom(name)).id);
              setRoomsOpen(false);
            }}
            createSignal={createSignal}
          />
        </>
      )}
      <main className="sa-main">
        {room ? (
          <>
            <Notices
              notices={noticesFor(workspace.notices, room.id)}
              onDismiss={workspace.dismissNotice}
              onShowTask={(taskId) => {
                setFocusTask(taskId);
                showPanel("tasks");
              }}
              onShowUsage={() => showPanel("usage")}
            />
            <Timeline
              key={room.id}
              chat={roomChat}
              bots={workspace.bots}
              tasks={roomTasks}
              runs={workspace.board.runs}
              taskActions={workspace.tasks}
              members={members.filter((bot) => bot.enabled)}
              onShowTask={(taskId) => {
                setFocusTask(taskId);
                showPanel("tasks");
              }}
              loadError={loadError}
              onRetry={() => setRetry((n) => n + 1)}
              onLoadOlder={() => workspace.loadOlder(room.id)}
              onDecide={workspace.decideApproval}
              onLightDemo={runLightDemo}
              onExample={setPrefill}
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
              roomId={room.id}
              prefill={prefill}
              onPrefilled={() => setPrefill(null)}
              commands={slashCommands}
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
            <p>在左边的房间列表里点 + 新建一个。</p>
          </div>
        )}
      </main>
      {room && (
        <ContextPanel
          tab={tab}
          onTab={setTab}
          onClose={() => setPanelOpen(false)}
          width={layout.panelWidth}
          onResize={(panelWidth) => changeLayout({ panelWidth })}
          room={room}
          members={members}
          bots={workspace.bots}
          team={workspace.team}
          board={workspace.board}
          taskActions={workspace.tasks}
          focusTask={focusTask}
          onFocusTask={setFocusTask}
          routines={workspace.routines.filter((routine) => routine.room_id === room.id)}
          routineActions={workspace.routineActions}
          links={workspace.links.filter((link) => link.room_id === room.id)}
          linkActions={workspace.linkActions}
          activity={demoNodes}
          usageKey={workspace.notices.length}
        />
      )}
    </div>
  );
}
