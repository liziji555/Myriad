import { useEffect, useState, type FormEvent } from "react";
import { looksLikeRepo, minutesLeft, pairingCommand, pairingLink, repoFromInput } from "../api/links";
import type { LinkActions } from "../api/useWorkspace";
import type { Link, Pairing, Room } from "../api/types";

interface Props {
  room: Room;
  /** 这个房间的连接。 */
  links: readonly Link[];
  actions: LinkActions;
}

/** 右栏的「连接」页：哪些 GitHub 仓库的事进这个房间，哪些 Telegram 聊天能和这个房间说话。 */
export function LinksPanel({ room, links, actions }: Props) {
  const [repo, setRepo] = useState("");
  const [pairing, setPairing] = useState<Pairing | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [, setTick] = useState(0);

  // 配对码只属于生成它的那个房间。
  useEffect(() => {
    setPairing(null);
    setError(null);
  }, [room.id]);

  // 配对码剩下的时间每半分钟更新一次。
  useEffect(() => {
    if (!pairing) return;
    const timer = setInterval(() => setTick((n) => n + 1), 30_000);
    return () => clearInterval(timer);
  }, [pairing]);

  const run = async (action: () => Promise<unknown>) => {
    setError(null);
    setBusy(true);
    try {
      await action();
      return true;
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
      return false;
    } finally {
      setBusy(false);
    }
  };

  const repos = links.filter((link) => link.source === "github");
  const chats = links.filter((link) => link.source === "telegram");
  const wanted = repoFromInput(repo);
  const left = pairing ? minutesLeft(pairing) : 0;
  const link = pairing ? pairingLink(pairing) : undefined;

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (await run(() => actions.linkRepo(room.id, wanted))) setRepo("");
  };

  const list = (items: readonly Link[], label: string) => (
    <ul className="sa-list" aria-label={label}>
      {items.map((item) => (
        <li key={item.id} className="sa-member">
          <div className="sa-row sa-row-static">
            <span className="sa-dot" data-state="idle" aria-hidden="true" />
            <span className="sa-row-label" title={item.external}>
              {item.title}
            </span>
            <button
              type="button"
              className="sa-link sa-link-danger sa-row-action"
              disabled={busy}
              onClick={() => {
                if (window.confirm(`断开「${item.title}」？`)) void run(() => actions.remove(item.id));
              }}
            >
              断开
            </button>
          </div>
        </li>
      ))}
    </ul>
  );

  return (
    <div className="sa-members">
      {error && (
        <p className="sa-form-error" role="alert">
          {error}
        </p>
      )}

      <section className="sa-links-section" aria-labelledby="sa-links-github">
        <h3 id="sa-links-github" className="sa-section-title">
          GitHub 仓库
        </h3>
        {repos.length === 0 ? (
          <p className="sa-panel-empty">
            还没有连仓库。连上之后，仓库里新的 issue、评论和没通过的 CI 会进这个房间，由入口 Bot 来看。
          </p>
        ) : (
          list(repos, "连到这个房间的 GitHub 仓库")
        )}
        <form className="sa-inline-form" onSubmit={(event) => void submit(event)}>
          <input
            value={repo}
            spellCheck={false}
            placeholder="owner/name"
            aria-label="仓库（owner/name）"
            onChange={(event) => setRepo(event.target.value)}
          />
          <button type="submit" className="sa-button sa-button-quiet" disabled={busy || !looksLikeRepo(wanted)}>
            连接
          </button>
        </form>
        <p className="sa-links-hint">
          还要在仓库的 Webhook 设置里把地址填成「你的公网地址/hooks/github」，密钥和环境变量
          <code>GITHUB_WEBHOOK_SECRET</code> 里的一样。没设这个环境变量时不接收任何事件。
        </p>
      </section>

      <section className="sa-links-section" aria-labelledby="sa-links-telegram">
        <h3 id="sa-links-telegram" className="sa-section-title">
          Telegram 聊天
        </h3>
        {chats.length === 0 ? (
          <p className="sa-panel-empty">
            还没有连聊天。连上之后，在那个聊天里对机器人说的话会进这个房间，Bot 的回复送回那个聊天。
          </p>
        ) : (
          list(chats, "连到这个房间的 Telegram 聊天")
        )}
        {pairing && left > 0 ? (
          <div className="sa-pairing" role="status">
            <p>
              在 Telegram 里把这句话发给机器人（群里也可以，先把机器人拉进群）：
            </p>
            <code className="sa-pairing-code">{pairingCommand(pairing)}</code>
            {link ? (
              <p>
                或者直接
                <a href={link} target="_blank" rel="noreferrer">
                  打开和 @{pairing.bot_username} 的私聊
                </a>
                。
              </p>
            ) : (
              <p>
                现在还没连上 Telegram：要先把机器人的令牌放进环境变量 <code>TELEGRAM_BOT_TOKEN</code> 再启动
                sa serve，否则这个码没有人收。
              </p>
            )}
            <p className="sa-links-hint">这个码只能用一次，还有 {left} 分钟过期。</p>
          </div>
        ) : (
          pairing && <p className="sa-links-hint">配对码过期了，重新生成一个。</p>
        )}
        <button
          type="button"
          className="sa-button sa-button-quiet"
          disabled={busy}
          onClick={() => void run(async () => setPairing(await actions.pair(room.id)))}
        >
          {pairing ? "重新生成配对码" : "生成配对码"}
        </button>
      </section>
    </div>
  );
}
