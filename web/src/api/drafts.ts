/** 每个房间里写了一半还没发的话。记在浏览器里：换房间、刷新、断线重连都还在。 */

/** 一份草稿最多留这么多字，再长的不记（照常能发）。 */
export const MAX_DRAFT_CHARS = 20_000;

type Store = Pick<Storage, "getItem" | "setItem" | "removeItem">;

const key = (roomId: string) => `sa.draft.${roomId}`;

export function readDraft(store: Store, roomId: string): string {
  try {
    return store.getItem(key(roomId)) ?? "";
  } catch {
    return "";
  }
}

export function writeDraft(store: Store, roomId: string, text: string): void {
  try {
    if (text === "" || text.length > MAX_DRAFT_CHARS) store.removeItem(key(roomId));
    else store.setItem(key(roomId), text);
  } catch {
    // 浏览器不让存：这次照常用，只是刷新后不记得。
  }
}

function browserStore(): Store | null {
  try {
    return globalThis.localStorage ?? null;
  } catch {
    return null;
  }
}

export function loadDraft(roomId: string): string {
  const store = browserStore();
  return store ? readDraft(store, roomId) : "";
}

export function saveDraft(roomId: string, text: string): void {
  const store = browserStore();
  if (store) writeDraft(store, roomId, text);
}
