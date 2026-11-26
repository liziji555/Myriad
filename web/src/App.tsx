import { useCallback, useEffect, useMemo, useState, type CSSProperties } from "react";
import { loadLayout, saveLayout, type Layout } from "./api/layout";
import { CHECKPOINTS_CHANGED, checkpointsByMessage } from "./api/checkpoints";
import { EXTENSIONS_CHANGED } from "./api/extensions";
import { skillPrompt, type SlashCommand } from "./api/slash";
import { presenceOf, speakersOf, type Presence } from "./api/presence";
import { executionOf } from "./api/execution";
import { STATE_LABEL } from "./api/tasks";
import { api } from "./api/client";
import type { CheckpointInfo, RoomId, SkillInfo, TaskId, UpdateStatus } from "./api/types";
import { useWorkspace } from "./api/useWorkspace";
import {
  DESKTOP_CHANGED,
  DESKTOP_OPEN,
  desktopInvoke,
  type DesktopState,
  type DesktopUpdateInfo,
} from "./api/desktop";
import { ConnectPanel } from "./components/ConnectPanel";
import { UpdatePanel } from "./components/UpdatePanel";
import { BotCreateModal } from "./components/BotCreateModal";
import { RoomCreateModal } from "./components/RoomCreateModal";
import { noticesFor } from "./api/workspace";
import { CommandPalette, type PaletteEntry } from "./components/CommandPalette";
import { ComposerControls } from "./components/ComposerControls";
import { Composer } from "./components/Composer";
import {
  ContextPanel,
  type PanelTab,
} from "./components/ContextPanel";
import { Notices } from "./components/Notices";
import { applyTheme, loadTheme, nextTheme } from "./api/theme";
import { Sidebar, type RoomState } from "./components/Sidebar";
import { StageBar } from "./components/StageBar";
import { WindowControls } from "./components/WindowControls";
import { Timeline } from "./components/Timeline";
import { FlowView } from "./components/FlowView";
import { TrajectoryPane } from "./components/TrajectoryPane";
import { flowOf } from "./api/flow";
import { HUE_COLORS, hueOf } from "./api/avatar";
import { LightProvider, useLight } from "./light/LightProvider";
import "./vendor/light-ui/running-border.css";

/** 窗口够宽时左栏常驻（可以收成窄条），窄了变成从左上角呼出的抽屉。 */
const DOCKED = "(min-width: 1101px)";

function useMedia(query: string): boolean {
  const [matches, setMatches] = useState(
    () => window.matchMedia(query).matches,
  );
  useEffect(() => {
    const media = window.matchMedia(query);
    const onChange = () => setMatches(media.matches);
    onChange();
    media.addEventListener("change", onChange);
    return () => media.removeEventListener("change", onChange);
  }, [query]);
  return matches;
}

export function App() {
  return (
    <LightProvider>
      <Shell />
    </LightProvider>
  );
}

function Shell() {
  const emit = useLight();
  const workspace = useWorkspace();
  const [selected, setSelected] = useState<RoomId | null>(null);
  const [tab, setTab] = useState<PanelTab>("execution");
  const [roomsOpen, setRoomsOpen] = useState(false);
  const docked = useMedia(DOCKED);
  const [invokeDesktop] = useState(() => desktopInvoke());
  const [desktop, setDesktop] = useState<DesktopState | null>(null);
  const [connectOpen, setConnectOpen] = useState(false);
  const [updateOpen, setUpdateOpen] = useState(false);
  const [botModalOpen, setBotModalOpen] = useState(false);
  const [roomModalOpen, setRoomModalOpen] = useState(false);
  const [serverUpdate, setServerUpdate] = useState<UpdateStatus | null>(null);
  const [desktopUpdate, setDesktopUpdate] = useState<DesktopUpdateInfo | null>(null);
  // 输入框的菜单里列的技能。技能是磁盘上的文件，随时会多会少：连上之后读一次，回到窗口时再读。
  // 没连上（没登录、还在加载）时不读：读了也是 401。技能不分房间，换房间不用重读。
  const [skills, setSkills] = useState<SkillInfo[]>([]);
  const ready = workspace.phase === "ready";
  useEffect(() => {
    if (!ready) return;
    let stale = false;
    let asked = 0;
    const load = () => {
      const mine = ++asked;
      void api.extensions().then(
        (found) => !stale && mine === asked && setSkills(found.skills),
        () => {},
      );
    };
    load();
    window.addEventListener("focus", load);
    window.addEventListener(EXTENSIONS_CHANGED, load);
    return () => {
      stale = true;
      window.removeEventListener("focus", load);
      window.removeEventListener(EXTENSIONS_CHANGED, load);
    };
  }, [ready]);
  // 连上后静默查一次更新：有新版就在左栏版本号旁亮起「有更新」，失败不打扰人。
  useEffect(() => {
    if (!ready) return;
    let stale = false;
    void api.checkUpdate().then(
      (found) => !stale && setServerUpdate(found),
      () => {},
    );
    if (invokeDesktop) {
      void (invokeDesktop("check_desktop_update") as Promise<DesktopUpdateInfo>).then(
        (found) => !stale && setDesktopUpdate(found),
        () => {},
      );
    }
    return () => {
      stale = true;
    };
  }, [ready, invokeDesktop]);
  useEffect(() => {
    if (!invokeDesktop) return;
    const refresh = () =>
      void invokeDesktop("desktop_state").then(
        (state) => setDesktop(state as DesktopState),
        () => setDesktop(null),
      );
    const open = () => setConnectOpen(true);
    refresh();
    window.addEventListener(DESKTOP_CHANGED, refresh);
    window.addEventListener(DESKTOP_OPEN, open);
    return () => {
      window.removeEventListener(DESKTOP_CHANGED, refresh);
      window.removeEventListener(DESKTOP_OPEN, open);
    };
  }, [invokeDesktop]);
  const openConnect = () => setConnectOpen(true);
  const [prefill, setPrefill] = useState<string | null>(null);
  const [layout, setLayout] = useState(loadLayout);
  const panelOpen = layout.panelOpen;
  const collapsed = layout.sidebarCollapsed;
  const changeLayout = (patch: Partial<Layout>) =>
    setLayout((current) => {
      const next = { ...current, ...patch };
      saveLayout(next);
      return next;
    });
  const [focusTask, setFocusTask] = useState<TaskId | null>(null);
  const [version, setVersion] = useState("");
  const [loadError, setLoadError] = useState<string | null>(null);
  const [retry, setRetry] = useState(0);
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [theme, setTheme] = useState(loadTheme);
  const [trajectoryOpen, setTrajectoryOpen] = useState(true);
  const [flowExpanded, setFlowExpanded] = useState(false);
  const [checkpoints, setCheckpoints] = useState<CheckpointInfo[]>([]);
  const [checkpointRefresh, setCheckpointRefresh] = useState(0);
  const [extensionsReloadTrigger, setExtensionsReloadTrigger] = useState(0);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (
        (event.ctrlKey || event.metaKey) &&
        !event.altKey &&
        event.key.toLowerCase() === "k"
      ) {
        event.preventDefault();
        setPaletteOpen((open) => !open);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  useEffect(() => {
    api.health().then(
      (health) => setVersion(health.version),
      () => {},
    );
  }, []);

  // 选中的房间（没选时是第一个）。换房间、重新连上、片段丢了，都重新拉一次最新的消息。
  const roomId =
    (workspace.rooms.find((r) => r.id === selected) ?? workspace.rooms[0])
      ?.id ?? null;
  const { openRoom, epoch } = workspace;
  useEffect(() => {
    if (!roomId) return;
    let stale = false;
    setLoadError(null);
    openRoom(roomId).catch((cause: unknown) => {
      if (!stale)
        setLoadError(cause instanceof Error ? cause.message : String(cause));
    });
    return () => {
      stale = true;
    };
  }, [roomId, epoch, retry, openRoom]);

  // 房间内已完成消息数变化、切换房间或回滚完成后，自动刷新轮次级文件检查点。
  const settledBotTurns = useMemo(() => {
    const msgs = roomId ? workspace.chat.rooms[roomId]?.messages ?? [] : [];
    return msgs.filter((m) => m.sender.kind === "bot" && m.state !== "streaming").length;
  }, [roomId, workspace.chat.rooms]);
  useEffect(() => {
    const bump = () => setCheckpointRefresh((n) => n + 1);
    window.addEventListener(CHECKPOINTS_CHANGED, bump);
    return () => window.removeEventListener(CHECKPOINTS_CHANGED, bump);
  }, []);
  useEffect(() => {
    if (!ready || !roomId) {
      setCheckpoints([]);
      return;
    }
    let stale = false;
    void api.listCheckpoints(roomId).then(
      (list) => !stale && setCheckpoints(list),
      () => !stale && setCheckpoints([]),
    );
    return () => {
      stale = true;
    };
  }, [ready, roomId, epoch, settledBotTurns, checkpointRefresh]);
  const checkpointMap = useMemo(() => checkpointsByMessage(checkpoints), [checkpoints]);
  const handleRewindCheckpoint = useCallback(
    async (checkpointId: string, force: boolean) => {
      if (!roomId) throw new Error("当前没有选中的房间");
      const report = await api.rewindCheckpoint(roomId, checkpointId, { force });
      window.dispatchEvent(new Event(CHECKPOINTS_CHANGED));
      return report;
    },
    [roomId],
  );

  if (workspace.phase === "unauthorized") {
    return (
      <main className="sa-gate">
        <WindowControls />
        <h1>需要访问令牌</h1>
        <p>
          请回到运行 <code>sa serve</code>{" "}
          的窗口，用它打印的链接重新打开这个页面。
        </p>
      </main>
    );
  }
  if (workspace.phase === "loading") {
    return (
      <main className="sa-gate">
        <WindowControls />
        <p>{workspace.connected ? "正在加载…" : "正在连接本机服务…"}</p>
      </main>
    );
  }

  const room =
    workspace.rooms.find((r) => r.id === selected) ?? workspace.rooms[0];
  const roomChat = room ? workspace.chat.rooms[room.id] : undefined;
  // 有 Bot 在写、在挑人接话之前的空档里有人排着队，都算「正在回复」，可以点停止。
  // 任务的运行过程不算：它有自己的停止按钮，不占着输入框。
  const replying =
    (roomChat?.messages.some(
      (message) => message.state === "streaming" && message.kind === "chat",
    ) ??
      false) ||
    speakersOf(roomChat?.activity).length > 0 ||
    Boolean(roomChat?.activity?.routing) ||
    (roomChat?.activity?.queued.length ?? 0) > 0;
  // 按进房间的先后排，和服务端一致。
  const members = room
    ? room.member_bot_ids.flatMap((id) =>
        workspace.bots.filter((bot) => bot.id === id),
      )
    : [];
  // 正在说话的那几个 Bot：没点名的插话它们都看得到。
  const speaking = speakersOf(roomChat?.activity).flatMap(
    (id) => workspace.bots.find((bot) => bot.id === id)?.name ?? [],
  );
  const queued = (roomChat?.activity?.queued ?? []).flatMap(
    (id) => workspace.bots.find((bot) => bot.id === id)?.name ?? [],
  );
  // 窄窗口里房间抽屉和面板都盖在主线上，同一时间只开一个。
  const setPanelOpen = (open: boolean) => {
    changeLayout({ panelOpen: open });
    if (open && !docked) setRoomsOpen(false);
  };
  const openRoomsDrawer = () => {
    setPanelOpen(false);
    setRoomsOpen(true);
  };
  const showPanel = (next: PanelTab) => {
    setTab(next);
    setPanelOpen(true);
  };
  // 左栏里点的面板页：已经开着的再点一下是收起。抽屉里点了就把抽屉收起来，让出地方。
  const toggleTab = (next: PanelTab) => {
    if (panelOpen && tab === next) setPanelOpen(false);
    else showPanel(next);
    if (!docked) setRoomsOpen(false);
  };
  // 左上角那个按钮：宽窗口里收起、展开常驻的左栏，窄窗口里开关抽屉。
  const toggleRooms = () => {
    if (docked) changeLayout({ sidebarCollapsed: !collapsed });
    else if (roomsOpen) setRoomsOpen(false);
    else openRoomsDrawer();
  };
  const roomsShown = docked ? !collapsed : roomsOpen || !room;
  // 每个打开过的房间此刻的样子：有步骤等批准是「等你」，有 Bot 在说、在排队、在挑人是「在忙」。
  const roomStates: Record<RoomId, RoomState> = {};
  for (const [id, chat] of Object.entries(workspace.chat.rooms)) {
    if (!chat) continue;
    const live = chat.messages.filter(
      (message) => message.state === "streaming",
    );
    if (
      live.some((message) =>
        message.steps.some((step) => step.state === "waiting"),
      )
    )
      roomStates[id] = "warn";
    else if (
      live.length > 0 ||
      speakersOf(chat.activity).length > 0 ||
      chat.activity?.routing ||
      chat.activity?.queued.length
    )
      roomStates[id] = "busy";
  }
  const roomTasks = room ? (workspace.board.rooms[room.id] ?? []) : [];
  const presence = presenceOf(members, roomChat, roomTasks);
  const execution = executionOf(presence, members, roomChat, roomTasks);
  const flow = room
    ? flowOf(
        roomChat?.messages ?? [],
        workspace.bots,
        room.host_bot_id,
        room.member_bot_ids,
        room.discussion,
      )
    : null;
  const liveLanes = flow
    ? new Set(
        flow.nodes
          .filter((node) => node.tone === "live" || node.tone === "waiting")
          .map((node) => node.lane),
      ).size
    : 0;
  const handoffCount = flow
    ? flow.edges.filter(
        (edge) => edge.kind === "dispatch" || edge.kind === "peer",
      ).length
    : 0;
  // 从协作视图、工作树轨迹或检查点面板点一个节点：若在更早分页里先自动补拉，再滚过去闪一下。
  const openMessage = async (messageId: string): Promise<boolean> => {
    if (flowExpanded) setFlowExpanded(false);
    if (roomId) {
      const found = await workspace.findMessage(roomId, messageId);
      if (!found) return false;
    }
    window.setTimeout(() => {
      const target = document.querySelector(`[data-message-id="${CSS.escape(messageId)}"]`);
      if (!target) return;
      target.scrollIntoView({ block: "center", behavior: "smooth" });
      target.setAttribute("data-flash", "true");
      window.setTimeout(() => target.removeAttribute("data-flash"), 1600);
    }, 60);
    return true;
  };
  const pendingTasks = roomTasks.filter((task) =>
    ["review", "waiting", "blocked"].includes(task.state),
  ).length;
  // 点了现场栏上的牌子：主线上有那条消息就滚过去闪一下，同屏模式下再发射一道流光落到该队员最新的工作树节点。
  const jumpTo = (row: Presence) => {
    const fireToTree = () => {
      const chip = document.querySelector(`[data-presence-bot="${CSS.escape(row.botId)}"]`);
      const nodes = document.querySelectorAll(`[data-traj-bot="${CSS.escape(row.botId)}"] .sa-traj-glyph`);
      const lastGlyph = nodes.length > 0 ? nodes[nodes.length - 1] : null;
      if (chip && lastGlyph) {
        void emit({
          id: `chip-${row.botId}-${Date.now()}`,
          source: chip,
          target: lastGlyph,
          color: HUE_COLORS[hueOf(row.botId)],
          duration: 1100,
          sourceKind: "model",
        });
      }
    };
    if (flowExpanded) {
      setFlowExpanded(false);
      window.setTimeout(() => {
        reveal(row);
        fireToTree();
      }, 60);
    } else {
      reveal(row);
      fireToTree();
    }
  };

  // ProkBot / GrokBot 风格：识别主控、每个 Bot 的 1v1 直聊房间、以及多 Bot 协作房间。
  const masterBot = workspace.bots.find((b) => b.enabled) ?? workspace.bots[0] ?? null;
  const directRoomOf = (botId: string) => {
    const bot = workspace.bots.find((b) => b.id === botId);
    const singles = workspace.rooms
      .filter((candidate) => candidate.member_bot_ids.length === 1 && candidate.member_bot_ids[0] === botId)
      .sort((a, b) => a.created_at - b.created_at);
    if (singles.length === 0) return undefined;
    if (bot) {
      const exact = singles.find((candidate) => candidate.name === bot.name);
      if (exact) return exact;
    }
    return singles[0];
  };
  const directRoomIds = new Set(
    workspace.bots.flatMap((bot) => {
      const dm = directRoomOf(bot.id);
      return dm ? [dm.id] : [];
    }),
  );
  const groupRooms = workspace.rooms.filter((candidate) => !directRoomIds.has(candidate.id));

  const openBotChat = async (bot: (typeof workspace.bots)[number]) => {
    setRoomsOpen(false);
    setFlowExpanded(false);
    const existing = directRoomOf(bot.id);
    if (existing) {
      setSelected(existing.id);
      return;
    }
    const created = await workspace.createRoom({
      name: bot.name,
      member_bot_ids: [bot.id],
      host_bot_id: bot.id,
      discussion: "normal",
    });
    setSelected(created.id);
  };

  const roomBadge = !room
    ? null
    : masterBot && room.member_bot_ids.length === 1 && room.member_bot_ids[0] === masterBot.id
      ? "主控"
      : room.member_bot_ids.length === 1
        ? "1 对 1"
        : room.member_bot_ids.length >= 2
          ? `${room.member_bot_ids.length} 位队员协作`
          : null;

  // 右侧单槽位互斥：打开任何工具面板时自动隐藏工作树，彻底告别四栏拥挤裁剪；收起工具面板后工作树自动回来。
  const showTrajectory = Boolean(!panelOpen && trajectoryOpen && !flowExpanded && flow);
  const toggleTrajectory = () => {
    if (typeof window !== "undefined" && window.innerWidth <= 960) {
      setPanelOpen(false);
      setFlowExpanded((expanded) => !expanded);
      return;
    }
    if (panelOpen) {
      setPanelOpen(false);
      setTrajectoryOpen(true);
      setFlowExpanded(false);
    } else {
      setFlowExpanded(false);
      setTrajectoryOpen((open) => !open);
    }
  };
  const reveal = (row: Presence) => {
    const target =
      row.messageId &&
      document.querySelector(`[data-message-id="${row.messageId}"]`);
    if (target) {
      // 窗口窄的时候面板盖在主线上面：先收起来，不然要看的东西被挡着。
      if (window.matchMedia("(max-width: 1100px)").matches) setPanelOpen(false);
      // 回复很长时审批卡可能在它的最下面：有卡就对着卡滚，不对着整条回复。
      const card =
        row.state === "approval" ? target.querySelector(".sa-approval") : null;
      (card ?? target).scrollIntoView({ block: "center", behavior: "smooth" });
      target.setAttribute("data-flash", "true");
      window.setTimeout(() => target.removeAttribute("data-flash"), 1600);
    } else if (row.taskId) {
      setFocusTask(row.taskId);
      showPanel("tasks");
    }
  };

  const tabAction = (id: PanelTab, title: string): PaletteEntry => ({
    id: `action:tab-${id}`,
    group: "action",
    title,
    run: () => showPanel(id),
  });
  const paletteEntries: PaletteEntry[] = [
    ...workspace.rooms.map((r): PaletteEntry => ({
      id: `room:${r.id}`,
      group: "room",
      title: r.name,
      hint: r.id === room?.id ? "正开着" : undefined,
      run: () => setSelected(r.id),
    })),
    ...workspace.bots.map((bot): PaletteEntry => ({
      id: `bot:${bot.id}`,
      group: "bot",
      title: bot.name,
      hint: bot.persona.split("\n")[0] || (bot.enabled ? undefined : "已停用"),
      run: () => showPanel("members"),
    })),
    // 只有打开过的房间的任务在手上。
    ...Object.values(workspace.board.rooms)
      .flat()
      .map((task): PaletteEntry => ({
        id: `task:${task.id}`,
        group: "task",
        title: task.title,
        hint: [
          workspace.rooms.find((r) => r.id === task.room_id)?.name,
          STATE_LABEL[task.state],
        ]
          .filter(Boolean)
          .join(" · "),
        run: () => {
          setSelected(task.room_id);
          setFocusTask(task.id);
          showPanel("tasks");
        },
      })),
    {
      id: "action:new-bot",
      group: "action",
      title: "新建 Bot",
      run: () => setBotModalOpen(true),
    },
    {
      id: "action:new-room",
      group: "action",
      title: "组建协作房间",
      run: () => setRoomModalOpen(true),
    },
    ...(room
      ? [
          tabAction("execution", "看每个 Bot 此刻在做什么"),
          tabAction("tasks", "打开任务页"),
          tabAction("routines", "打开定时任务"),
          tabAction("members", "管理 Bot 和房间成员"),
          tabAction("links", "连接 GitHub 仓库和 Telegram 聊天"),
          tabAction("models", "接自己的模型：模型服务和三个档位"),
          tabAction("extensions", "看技能和 MCP 服务"),
          tabAction("usage", "看用量和花费"),
          tabAction("audit", "看审计日志：谁批准了什么、做了什么"),
          tabAction("activity", "看任务图"),
          {
            id: "action:toggle-plan",
            group: "action" as const,
            title:
              room.permission === "plan"
                ? "退出规划模式（切回每次都问）"
                : "进入规划模式（Plan Mode：先出方案再执行）",
            run: () =>
              void workspace.team.updateRoom(room.id, {
                permission: room.permission === "plan" ? "ask" : "plan",
              }),
          },
          {
            id: "action:reload-extensions",
            group: "action" as const,
            title: "热重载 MCP 与钩子配置",
            run: () => {
              showPanel("extensions");
              setExtensionsReloadTrigger((n) => n + 1);
            },
          },
          {
            id: "action:toggle-sidebar",
            group: "action" as const,
            title: roomsShown ? "收起左栏" : "打开左栏",
            run: toggleRooms,
          },
          {
            id: "action:toggle-panel",
            group: "action" as const,
            title: panelOpen ? "收起面板" : "打开面板",
            run: () => setPanelOpen(!panelOpen),
          },
        ]
      : []),
    ...(desktop && desktop.pinned !== "workdir"
      ? [
          {
            id: "action:connect",
            group: "action" as const,
            title: "连接本机或服务器",
            run: openConnect,
          },
        ]
      : []),
    ...(invokeDesktop
      ? [
          {
            id: "action:data-dir",
            group: "action" as const,
            title: "打开数据目录",
            run: () => void invokeDesktop("open_data_dir").catch(() => {}),
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
  // 技能排在最前面：这个房间里有 Bot 能用的才列出来。停用的 Bot 不说话，harness 用的是它自己的技能。
  const skilled = members.filter((bot) => bot.enabled && bot.harness === "builtin");
  const skillCommands: SlashCommand[] = skills
    .filter((skill) =>
      skill.bots.length === 0 ? skilled.length > 0 : skilled.some((bot) => skill.bots.includes(bot.name)),
    )
    .map((skill) => ({
      id: `skill:${skill.name}`,
      group: "技能",
      title: skill.name,
      hint: skill.description,
      insert: skillPrompt(skill.name),
      run: () => {},
    }));

  return (
    <div
      className="sa-shell"
      data-panel-open={panelOpen}
      // 宽窗口：左栏常驻（docked）或收成窄条（rail）；窄窗口：抽屉开着（open）或收着（closed）。
      // 没有房间时抽屉总是开着：新建房间的入口在那里。
      data-rooms={
        docked
          ? collapsed
            ? "rail"
            : "docked"
          : roomsShown
            ? "open"
            : "closed"
      }
      style={{ "--sa-panel-width": `${layout.panelWidth}px` } as CSSProperties}
    >
      {connectOpen && desktop && invokeDesktop && (
        <ConnectPanel state={desktop} invoke={invokeDesktop} onClose={() => setConnectOpen(false)} />
      )}
      {updateOpen && (
        <UpdatePanel
          serverVersion={version}
          desktop={desktop}
          invokeDesktop={invokeDesktop}
          initialServerUpdate={serverUpdate}
          initialDesktopUpdate={desktopUpdate}
          onChecked={(nextServer, nextDesktop) => {
            setServerUpdate(nextServer);
            setDesktopUpdate(nextDesktop);
          }}
          onClose={() => setUpdateOpen(false)}
        />
      )}
      {paletteOpen && (
        <CommandPalette
          entries={paletteEntries}
          onClose={() => setPaletteOpen(false)}
        />
      )}
      {botModalOpen && (
        <BotCreateModal
          onCreate={async (body, openChat) => {
            const created = await workspace.team.createBot(body);
            if (openChat) {
              const dm = await workspace.createRoom({
                name: created.name,
                member_bot_ids: [created.id],
                host_bot_id: created.id,
                discussion: "normal",
              });
              setSelected(dm.id);
              setFlowExpanded(false);
            }
            return created;
          }}
          onClose={() => setBotModalOpen(false)}
        />
      )}
      {roomModalOpen && (
        <RoomCreateModal
          bots={workspace.bots}
          defaultMasterId={masterBot?.id ?? null}
          onCreate={async (body) => {
            const created = await workspace.createRoom(body);
            setSelected(created.id);
            setFlowExpanded(false);
            setRoomsOpen(false);
            return created;
          }}
          onOpenNewBot={() => setBotModalOpen(true)}
          onClose={() => setRoomModalOpen(false)}
        />
      )}
      <StageBar
        room={room}
        roomBadge={roomBadge}
        presence={presence}
        connected={workspace.connected}
        pendingTasks={pendingTasks}
        trajectoryShown={showTrajectory}
        docked={docked}
        roomsOpen={roomsShown}
        onToggleRooms={toggleRooms}
        onToggleTrajectory={toggleTrajectory}
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
      {(docked || roomsShown) && (
        <>
          {!docked && room && (
            <div className="sa-scrim" onClick={() => setRoomsOpen(false)} />
          )}
          <Sidebar
            masterBot={masterBot}
            directRoomOf={directRoomOf}
            groupRooms={groupRooms}
            bots={workspace.bots}
            selectedRoomId={room?.id ?? null}
            roomStates={roomStates}
            presence={presence}
            connected={workspace.connected}
            version={version}
            collapsed={collapsed}
            docked={docked}
            onToggleCollapsed={toggleRooms}
            activeTab={panelOpen ? tab : null}
            onShowPanel={toggleTab}
            onOpenBotChat={(bot) => void openBotChat(bot)}
            onSelectRoom={(id) => {
              setSelected(id);
              setFlowExpanded(false);
              setRoomsOpen(false);
            }}
            onNewBot={() => setBotModalOpen(true)}
            onNewRoom={() => setRoomModalOpen(true)}
            desktop={desktop}
            onConnect={openConnect}
            updateAvailable={Boolean(serverUpdate?.available || desktopUpdate?.available)}
            onUpdate={() => setUpdateOpen(true)}
          />
        </>
      )}
      <div className="sa-workspace">
        <main className="sa-main">
          {room ? (
            <div className="sa-split-workspace" data-split={showTrajectory}>
              <div className="sa-primary-column">
                <Notices
                  notices={noticesFor(workspace.notices, room.id)}
                  onDismiss={workspace.dismissNotice}
                  onShowTask={(taskId) => {
                    setFocusTask(taskId);
                    showPanel("tasks");
                  }}
                  onShowUsage={() => showPanel("usage")}
                />
                {flowExpanded && flow && (
                  <>
                    <div className="sa-flow-meta-bar">
                      <button
                        type="button"
                        className="sa-link"
                        onClick={() => setFlowExpanded(false)}
                      >
                        ← 返回同屏对话
                      </button>
                      <span>
                        {flow.lanes.length - 1} 条工作树泳道 · {handoffCount} 次派活与交接
                        {liveLanes > 0 ? ` · ${liveLanes} 个正在运行` : ""}
                      </span>
                    </div>
                    <FlowView
                      key={room.id}
                      flow={flow}
                      presence={presence}
                      onOpen={openMessage}
                    />
                  </>
                )}
                <div className="sa-viewpane" hidden={flowExpanded && !!flow}>
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
                    checkpoints={checkpointMap}
                    onRewindCheckpoint={handleRewindCheckpoint}
                    onExample={setPrefill}
                    hidden={flowExpanded && !!flow}
                  />
                </div>
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
                  commands={[...skillCommands, ...slashCommands]}
                  controls={
                    <ComposerControls
                      room={room}
                      host={members.find((bot) => bot.id === room.host_bot_id) ?? null}
                      busy={replying}
                      onPermission={(permission) => workspace.team.updateRoom(room.id, { permission })}
                      onTier={(tier) =>
                        room.host_bot_id ? workspace.team.updateBot(room.host_bot_id, { tier }) : Promise.resolve()
                      }
                      onManageModels={() => showPanel("models")}
                    />
                  }
                  disabledReason={
                    room.host_bot_id ? null : "这个房间里还没有 Bot，发了也没人回应"
                  }
                  replying={replying}
                  steerTo={speaking.length > 0 ? speaking.join("、") : null}
                  members={members.filter((bot) => bot.enabled)}
                  queue={roomChat?.queue ?? []}
                  onSend={(content) => workspace.sendMessage(room.id, content)}
                  onQueue={(content) => workspace.queueMessage(room.id, content)}
                  onEditQueued={workspace.editQueued}
                  onCancelQueued={workspace.cancelQueued}
                  onStop={() => workspace.stopReply(room.id)}
                />
              </div>
              {showTrajectory && flow && (
                <TrajectoryPane
                  key={`traj-${room.id}`}
                  roomId={room.id}
                  flow={flow}
                  bots={workspace.bots}
                  presence={presence}
                  onOpen={openMessage}
                  onShowDiff={() => showPanel("files")}
                  onExpandFlow={() => setFlowExpanded(true)}
                  onClose={() => setTrajectoryOpen(false)}
                />
              )}
            </div>
          ) : (
            <div className="sa-empty">
              <h2>先创建一个 Bot 开始对话</h2>
              <p>点左侧「新建 Bot」创建你的主控或专职助手，也可以直接组建多 Bot 协作房间。</p>
              <button type="button" className="sa-button" onClick={() => setBotModalOpen(true)}>
                新建 Bot
              </button>
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
            routines={workspace.routines.filter(
              (routine) => routine.room_id === room.id,
            )}
            routineActions={workspace.routineActions}
            links={workspace.links.filter((link) => link.room_id === room.id)}
            linkActions={workspace.linkActions}
            usageKey={workspace.notices.length}
            execution={execution}
            onJump={jumpTo}
            onStopBot={(botId) => void workspace.stopBot(room.id, botId)}
            checkpoints={checkpoints}
            onRewindCheckpoint={handleRewindCheckpoint}
            onJumpMessage={openMessage}
            extensionsReloadTrigger={extensionsReloadTrigger}
          />
        )}
      </div>
    </div>
  );
}
