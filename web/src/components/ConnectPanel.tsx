import { useEffect, useRef, useState, type FormEvent, type ReactNode } from "react";
import { toFailure, toForm, toTarget, type ConnectFailure, type DesktopState, type ServerForm } from "../api/desktop";
import { Icon } from "./Icon";

interface Props {
  state: DesktopState;
  invoke(command: string, args?: Record<string, unknown>): Promise<unknown>;
  onClose(): void;
}

type Mode = "local" | "server";
type Busy = "test" | "connect" | "key" | null;

/** 「连接」面板（只在桌面版里）：用本机，还是通过 SSH 连服务器。连的时候主界面照常用，没连上就在这里说原因。 */
export function ConnectPanel({ state, invoke, onClose }: Props) {
  const onServer = state.current.mode === "server";
  const [mode, setMode] = useState<Mode>(
    onServer || state.connecting || state.failed || state.pinned === "server" ? "server" : "local",
  );
  const [form, setForm] = useState<ServerForm>(() => toForm(state.saved));
  const [busy, setBusy] = useState<Busy>(null);
  const [tested, setTested] = useState(false);
  const [failure, setFailure] = useState<ConnectFailure | null>(null);
  const [password, setPassword] = useState("");
  const [note, setNote] = useState<string | null>(null);
  // 点了「不连了」之后，还没回来的那一次不再算数。
  const round = useRef(0);

  // 这里刚试出来的结果优先；没有就是打开应用时后台自动连的那一次。
  const failed = failure ?? (busy || tested ? null : state.failed);
  const connecting = busy === "connect" || state.connecting !== null;
  const locked = state.pinned !== null || connecting || busy !== null;
  const set = (patch: Partial<ServerForm>) => {
    setForm({ ...form, ...patch });
    setTested(false);
    setFailure(null);
  };

  const target = () => {
    const made = toTarget(form);
    if (typeof made === "string") {
      setFailure({ server: form.host, reason: made });
      return null;
    }
    return made;
  };

  // 面板关了就看不到测试结果：还在测的那一次停掉，不留着 ssh 在后台跑。
  const testing = useRef(false);
  testing.current = busy === "test";
  const stopTest = useRef(() => {});
  stopTest.current = () => void invoke("cancel_connect").catch(() => undefined);
  useEffect(
    () => () => {
      if (testing.current) stopTest.current();
    },
    [],
  );

  // 从菜单打开时焦点还在面板外面：Esc 挂在窗口上才关得掉。
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !event.isComposing) onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const run = async (kind: Exclude<Busy, null>, command: string, extra: Record<string, unknown> = {}) => {
    const args = target();
    if (!args) return false;
    const mine = ++round.current;
    setBusy(kind);
    setFailure(null);
    setTested(false);
    try {
      await invoke(command, { ...args, ...extra });
      return mine === round.current;
    } catch (error) {
      const failure = toFailure(error, args.host);
      if (mine === round.current && !failure.cancelled) setFailure(failure);
      return false;
    } finally {
      if (mine === round.current) setBusy(null);
    }
  };
  // 外壳停 ssh 要等一会儿：面板不等它，马上恢复。
  const cancel = () => {
    round.current += 1;
    setBusy(null);
    void act("cancel_connect");
  };

  const connect = (event: FormEvent) => {
    event.preventDefault();
    // 连上了外壳会把窗口换到服务器的页面，这个面板也就没了。
    void run("connect", "connect_server");
  };
  const test = async () => setTested(await run("test", "test_server"));
  const installKey = async (event: FormEvent) => {
    event.preventDefault();
    // 密码交给谁由外壳定：就是上面写着「没连上」的那一台。装好了外壳接着连。
    setBusy("key");
    try {
      await invoke("install_key", { server: failed?.server, password });
    } catch (error) {
      const failure = toFailure(error, failed?.server ?? form.host);
      if (!failure.cancelled) setFailure(failure);
    } finally {
      setPassword("");
      setBusy(null);
    }
  };
  const act = (command: string) =>
    invoke(command).then(
      () => setNote(null),
      (error: unknown) => setNote(String(error)),
    );

  const frame = (body: ReactNode) => (
    <div className="sa-palette-backdrop" onMouseDown={(event) => event.target === event.currentTarget && onClose()}>
      <div
        className="sa-connect-panel"
        role="dialog"
        aria-modal="true"
        aria-label="连接"
      >
        <header className="sa-connect-head">
          <h2>连接</h2>
          <button type="button" className="sa-link" onClick={onClose} aria-label="关闭">
            关闭
          </button>
        </header>
        {body}
        {note && (
          <p className="sa-form-error" role="alert">
            {note}
          </p>
        )}
      </div>
    </div>
  );

  // 这一页是服务器发来的：它只能换回本机。连别的服务器、填密码都回到本机的页面上做。
  if (onServer) {
    return frame(
      <section className="sa-form">
        <div className="sa-field">
          正在用的服务器
          <code className="sa-connect-path">{state.current.mode === "server" ? state.current.server : ""}</code>
        </div>
        {state.pinned !== "server" && (
          <div className="sa-connect-actions">
            <button type="button" className="sa-button" onClick={() => void act("use_local")}>
              换回本机
            </button>
          </div>
        )}
        <p className="sa-connect-hint">
          {state.pinned === "server"
            ? "启动时用 --server 定死了，这里换不了。"
            : "换回本机会断开这台服务器，服务器上的任务照常跑。要连别的服务器，先换回本机再连。"}
        </p>
      </section>,
    );
  }

  return frame(
    <>
      <div className="sa-connect-modes" role="radiogroup" aria-label="连接方式">
        <button
          type="button"
          role="radio"
          aria-checked={mode === "local"}
          className="sa-connect-mode"
          onClick={() => setMode("local")}
        >
          <strong>
            <Icon name="plug" /> 本机 <span className="sa-tag">正在用</span>
          </strong>
          <span>服务和任务都在这台电脑上，不联网也能用。</span>
        </button>
        <button
          type="button"
          role="radio"
          aria-checked={mode === "server"}
          className="sa-connect-mode"
          onClick={() => setMode("server")}
        >
          <strong>
            <Icon name="server" /> 通过 SSH 连接
          </strong>
          <span>连你自己的云服务器或 VPS 上常驻的 Myriad，关掉电脑任务也不断。</span>
        </button>
      </div>

      {mode === "local" ? (
        <section className="sa-form">
          <div className="sa-field">
            工作目录（Bot 读写这个文件夹）
            <code className="sa-connect-path">{state.workdir ?? "还没有"}</code>
          </div>
          <div className="sa-connect-actions">
            {state.pinned !== "workdir" && (
              <button type="button" className="sa-button sa-button-quiet" onClick={() => void act("pick_workdir")}>
                换工作目录…
              </button>
            )}
            <button type="button" className="sa-button sa-button-quiet" onClick={() => void act("open_data_dir")}>
              打开数据目录
            </button>
          </div>
          <p className="sa-connect-hint">换工作目录会重新打开 Myriad。</p>
        </section>
      ) : (
        <>
          <form className="sa-form" onSubmit={connect}>
            <label className="sa-field">
              主机
              <input
                autoFocus
                value={form.host}
                placeholder="1.2.3.4，或 ~/.ssh/config 里的 Host 名"
                autoComplete="off"
                spellCheck={false}
                disabled={locked}
                onChange={(event) => set({ host: event.target.value })}
              />
            </label>
            <div className="sa-connect-row">
              <label className="sa-field">
                用户
                <input
                  value={form.user}
                  placeholder="留空：用 ~/.ssh/config 或当前用户"
                  autoComplete="off"
                  spellCheck={false}
                  disabled={locked}
                  onChange={(event) => set({ user: event.target.value })}
                />
              </label>
              <label className="sa-field">
                端口
                <input
                  value={form.port}
                  placeholder="22"
                  inputMode="numeric"
                  autoComplete="off"
                  disabled={locked}
                  onChange={(event) => set({ port: event.target.value })}
                />
              </label>
            </div>
            <label className="sa-field">
              密钥文件
              <span className="sa-connect-key">
                <input
                  value={form.key}
                  placeholder="留空：用 ssh-agent、~/.ssh/config 或默认的密钥"
                  autoComplete="off"
                  spellCheck={false}
                  disabled={locked}
                  onChange={(event) => set({ key: event.target.value })}
                />
                <button
                  type="button"
                  className="sa-button sa-button-quiet"
                  disabled={locked}
                  onClick={() =>
                    void invoke("pick_key").then(
                      (path) => typeof path === "string" && set({ key: path }),
                      (error: unknown) => setNote(String(error)),
                    )
                  }
                >
                  选…
                </button>
              </span>
            </label>
            <div className="sa-connect-actions">
              <button
                type="button"
                className="sa-button sa-button-quiet"
                disabled={busy !== null || connecting}
                onClick={() => void test()}
              >
                {busy === "test" ? "正在测试…" : "测试连接"}
              </button>
              <button type="submit" className="sa-button" disabled={busy !== null || connecting}>
                {connecting ? "正在连…" : state.pinned === "server" ? "重新连接" : "保存并连接"}
              </button>
              {(connecting || busy === "test") && (
                <button type="button" className="sa-button sa-button-quiet" onClick={cancel}>
                  {connecting ? "不连了" : "不测了"}
                </button>
              )}
            </div>
          </form>
          {connecting && (
            <p className="sa-connect-hint" role="status">
              正在后台连 {state.connecting ?? form.host}
              ，第一次可能要十几秒。可以先关掉这个面板接着用本机，连上了会自动换过去。
            </p>
          )}
          {tested && (
            <p className="sa-connect-ok" role="status">
              连得上，服务器上的 Myriad 开着。
            </p>
          )}
          {failed && (
            <div role="alert">
              <p className="sa-form-error">没连上 {failed.server}，现在用的还是本机：</p>
              <pre className="sa-connect-reason">{failed.reason}</pre>
            </div>
          )}
          {failed?.password && (
            <form className="sa-form sa-connect-password" onSubmit={installKey}>
              <strong>用密码登录一次 {failed.server}</strong>
              <p className="sa-connect-hint">
                把这台电脑的 SSH 公钥装到服务器上，以后都用密钥连。密码只用这一次，不会保存。
              </p>
              <label className="sa-field">
                {failed.server} 的密码
                <input
                  type="password"
                  value={password}
                  autoComplete="off"
                  required
                  disabled={busy !== null}
                  onChange={(event) => setPassword(event.target.value)}
                />
              </label>
              <div className="sa-connect-actions">
                <button type="submit" className="sa-button" disabled={busy !== null || password === ""}>
                  {busy === "key" ? "正在装…" : "装好密钥再连"}
                </button>
              </div>
            </form>
          )}
        </>
      )}
      {state.pinned && (
        <p className="sa-connect-hint">
          启动时用 {state.pinned === "server" ? "--server" : "--workdir"} 定死了，这里换不了。
        </p>
      )}
    </>,
  );
}
