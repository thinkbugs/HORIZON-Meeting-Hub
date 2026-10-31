import { useCallback, useEffect, useMemo, useState } from "react";
import { formatBytes, media, playUrl, type LibraryView, type QueueView, type MediaKind } from "../lib/media";

interface MediaRailProps {
  library: LibraryView | null;
  queue: QueueView;
  /** Runs an action and re-reads both views, so a phone that edited the queue in
   *  the meantime is not overwritten by a stale local copy. */
  run: (action: () => Promise<unknown>) => void;
  busy: boolean;
  error: string | null;
}

const KIND_LABEL: Record<MediaKind, string> = {
  video: "视频",
  audio: "音频",
  image: "图片",
};

function KindGlyph({ kind }: { kind: MediaKind }) {
  if (kind === "image") {
    return (
      <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" aria-hidden="true">
        <rect x="3" y="4" width="18" height="16" rx="2" />
        <circle cx="8.5" cy="9" r="1.6" />
        <path d="M4 18l5-5 4 4 3-3 4 4" />
      </svg>
    );
  }
  if (kind === "audio") {
    return (
      <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" aria-hidden="true">
        <path d="M9 18V6l10-2v12" />
        <circle cx="6.5" cy="18" r="2.5" />
        <circle cx="16.5" cy="16" r="2.5" />
      </svg>
    );
  }
  return (
    <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" aria-hidden="true">
      <rect x="2.5" y="5" width="19" height="14" rx="2" />
      <path d="M10 9.5l5 2.5-5 2.5z" fill="currentColor" stroke="none" />
    </svg>
  );
}

/** The room's own file-casting management: one library, one queue, drag to
 *  reorder, and every control is the same action the extension and the phone
 *  already have. Nothing here is decorative — a row exists because the stage
 *  can act on it. */
export function MediaRail({ library, queue, run, busy, error }: MediaRailProps) {
  const [tab, setTab] = useState<"queue" | "library">("queue");
  const [picked, setPicked] = useState<Set<string>>(() => new Set());
  const [dragId, setDragId] = useState<string | null>(null);
  const [dropIndex, setDropIndex] = useState<number | null>(null);
  const [sortBy, setSortBy] = useState<"recent" | "title" | "kind">("recent");
  const [urlDraft, setUrlDraft] = useState("");

  const clearPicked = useCallback(() => setPicked(new Set()), []);

  const entries = useMemo(() => {
    const items = [...(library?.items ?? [])];
    if (sortBy === "title") items.sort((a, b) => a.title.localeCompare(b.title, "zh-Hans-CN"));
    if (sortBy === "kind") items.sort((a, b) => a.kind.localeCompare(b.kind) || a.title.localeCompare(b.title, "zh-Hans-CN"));
    if (sortBy === "recent") items.sort((a, b) => b.addedAt - a.addedAt);
    return items;
  }, [library, sortBy]);

  // A reorder has to land where the row was dropped, not where it was picked up:
  // the backend moves `from` to `to` in the list it currently holds.
  const drop = (index: number) => {
    if (!dragId) return;
    const from = queue.items.findIndex((item) => item.id === dragId);
    setDragId(null);
    setDropIndex(null);
    if (from < 0 || from === index) return;
    run(() => media.queueMove(dragId, index));
  };

  const pickedIds = [...picked];
  const toggle = (id: string) =>
    setPicked((previous) => {
      const next = new Set(previous);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  return (
    <aside className="media-rail" aria-label="片库与队列">
      <div className="media-rail__tabs" role="tablist">
        <button
          type="button"
          role="tab"
          aria-selected={tab === "queue"}
          className={tab === "queue" ? "is-on" : ""}
          onClick={() => setTab("queue")}
        >
          投屏队列
          <b>{queue.items.length}</b>
        </button>
        <button
          type="button"
          role="tab"
          aria-selected={tab === "library"}
          className={tab === "library" ? "is-on" : ""}
          onClick={() => setTab("library")}
        >
          主机片库
          <b>{library?.items.length ?? 0}</b>
        </button>
      </div>

      {error && <p className="media-rail__note is-error">{error}</p>}

      {tab === "queue" ? (
        <>
          {queue.halted && <p className="media-rail__note">{queue.halted}</p>}
          <ul className="media-rail__list">
            {queue.items.length === 0 && (
              <li className="media-rail__empty">队列是空的。切到「主机片库」勾选文件，或让手机直接投一条。</li>
            )}
            {queue.items.map((item, index) => (
              <li
                key={`${item.id}-${index}`}
                draggable
                className={
                  (queue.current === index ? "is-current " : "") +
                  (dropIndex === index ? "is-drop " : "") +
                  (dragId === item.id ? "is-dragging" : "")
                }
                onDragStart={() => setDragId(item.id)}
                onDragOver={(event) => {
                  event.preventDefault();
                  setDropIndex(index);
                }}
                onDragEnd={() => {
                  setDragId(null);
                  setDropIndex(null);
                }}
                onDrop={() => drop(index)}
              >
                <span className="media-rail__grip" aria-hidden="true">⋮⋮</span>
                <span className="media-rail__slot">{index + 1}</span>
                <span className="media-rail__glyph">
                  <KindGlyph kind={item.kind} />
                </span>
                <button
                  type="button"
                  className="media-rail__title"
                  title={item.title}
                  onClick={() => run(() => media.jump(item.id))}
                >
                  {item.title}
                  {!item.local && <em className="media-rail__remote">外链</em>}
                </button>
                <span className="media-rail__ops">
                  <button
                    type="button"
                    title="上移一位"
                    aria-label={`把 ${item.title} 上移一位`}
                    disabled={index === 0 || busy}
                    onClick={() => run(() => media.queueMove(item.id, index - 1))}
                  >
                    ↑
                  </button>
                  <button
                    type="button"
                    title="下移一位"
                    aria-label={`把 ${item.title} 下移一位`}
                    disabled={index === queue.items.length - 1 || busy}
                    onClick={() => run(() => media.queueMove(item.id, index + 1))}
                  >
                    ↓
                  </button>
                  <button
                    type="button"
                    title="预排为下一条"
                    aria-label={`把 ${item.title} 预排为下一条`}
                    onClick={() => run(() => media.setNext(item.id))}
                  >
                    ⇥
                  </button>
                  <button
                    type="button"
                    title="从队列移除"
                    aria-label={`从队列移除 ${item.title}`}
                    onClick={() => run(() => media.queueRemove(item.id))}
                  >
                    <CloseSmall />
                  </button>
                </span>
              </li>
            ))}
          </ul>

          <div className="media-rail__modes">
            <label>
              顺序
              <select
                value={queue.order}
                disabled={busy}
                onChange={(event) => run(() => media.setOrder(event.target.value as QueueView["order"]))}
              >
                <option value="sequential">顺序播放</option>
                <option value="shuffle">随机播放</option>
              </select>
            </label>
            <label>
              循环
              <select
                value={queue.repeat}
                disabled={busy}
                onChange={(event) => run(() => media.setRepeat(event.target.value as QueueView["repeat"]))}
              >
                <option value="off">不循环</option>
                <option value="all">循环列表</option>
                <option value="one">单曲循环</option>
              </select>
            </label>
            <label>
              图片停留
              <input
                type="number"
                min={1}
                max={600}
                step={1}
                value={queue.imageDwell}
                disabled={busy}
                onChange={(event) => run(() => media.setDwell(Number(event.target.value) || 1))}
              />
            </label>
          </div>

          <div className="media-rail__foot">
            <button type="button" disabled={busy || queue.items.length === 0} onClick={() => run(() => media.previous())}>
              上一条
            </button>
            <button type="button" disabled={busy || queue.items.length === 0} onClick={() => run(() => media.next())}>
              下一条
            </button>
            <button
              type="button"
              className="is-quiet"
              disabled={busy || queue.items.length === 0}
              onClick={() =>
                run(async () => {
                  const view = await media.clearQueue();
                  clearPicked();
                  return view;
                })
              }
            >
              清空队列
            </button>
          </div>
        </>
      ) : (
        <>
          <div className="media-rail__modes">
            <label>
              排列
              <select value={sortBy} onChange={(event) => setSortBy(event.target.value as typeof sortBy)}>
                <option value="recent">最近加入</option>
                <option value="title">按名称</option>
                <option value="kind">按类型</option>
              </select>
            </label>
            <span className="media-rail__count">
              {picked.size > 0 ? `已选 ${picked.size} 项` : "勾选后可整批排入"}
            </span>
          </div>
          <ul className="media-rail__list">
            {!library && <li className="media-rail__empty">正在读取片库…</li>}
            {library && entries.length === 0 && (
              <li className="media-rail__empty">
                片库还没有文件。手机或浏览器扩展把文件传到 7878 端口后，条目会出现在这里。
              </li>
            )}
            {entries.map((entry) => {
              const thumb = entry.kind === "image" && library ? playUrl(library.base, entry) : "";
              return (
              <li key={entry.id} className={picked.has(entry.id) ? "is-picked" : ""}>
                <input
                  type="checkbox"
                  aria-label={`选择 ${entry.title}`}
                  checked={picked.has(entry.id)}
                  onChange={() => toggle(entry.id)}
                />
                {thumb ? (
                  <img
                    className="media-rail__thumb"
                    src={thumb}
                    alt=""
                    loading="lazy"
                  />
                ) : (
                  <span className="media-rail__glyph">
                    <KindGlyph kind={entry.kind} />
                  </span>
                )}
                <span className="media-rail__title" title={entry.title}>
                  {entry.title}
                  {!entry.file && <em className="media-rail__remote">直投</em>}
                </span>
                <span className="media-rail__meta">
                  {KIND_LABEL[entry.kind]} · {formatBytes(entry.bytes)}
                </span>
                <span className="media-rail__ops">
                  <button
                    type="button"
                    title="只投这一条"
                    aria-label={`播放 ${entry.title}`}
                    onClick={() => run(() => media.setQueue([entry.id]))}
                  >
                    ▶
                  </button>
                  <button
                    type="button"
                    title="追加到队列"
                    aria-label={`把 ${entry.title} 追加到队列`}
                    onClick={() => run(() => media.enqueue([entry.id]))}
                  >
                    ＋
                  </button>
                  <button
                    type="button"
                    title="从片库删除"
                    aria-label={`从片库删除 ${entry.title}`}
                    onClick={() => run(() => media.remove(entry.id))}
                  >
                    <CloseSmall />
                  </button>
                </span>
              </li>
              );
            })}
          </ul>

          <form
            className="media-rail__url"
            onSubmit={(event) => {
              event.preventDefault();
              const url = urlDraft.trim();
              if (!url) return;
              run(async () => {
                await media.addUrl(url, "");
                setUrlDraft("");
              });
            }}
          >
            <input
              type="url"
              placeholder="粘贴一个网络上的媒体地址，不上传、直接投"
              value={urlDraft}
              onChange={(event) => setUrlDraft(event.target.value)}
            />
            <button type="submit" disabled={busy || urlDraft.trim() === ""}>
              存入片库
            </button>
          </form>

          <div className="media-rail__foot">
            <button
              type="button"
              className="is-primary"
              disabled={busy || picked.size === 0}
              onClick={() =>
                run(async () => {
                  const view = await media.setQueue(pickedIds);
                  clearPicked();
                  return view;
                })
              }
            >
              排入队列并播放
            </button>
            <button type="button" disabled={busy || picked.size === 0} onClick={() =>
                run(async () => {
                  const view = await media.enqueue(pickedIds);
                  clearPicked();
                  return view;
                })
              }>
              追加到队列
            </button>
          </div>
        </>
      )}
    </aside>
  );
}

function CloseSmall() {
  return (
    <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" aria-hidden="true">
      <path d="M6 6l12 12M18 6L6 18" />
    </svg>
  );
}

/**
 * The stage polls the queue because a controller's cast and a phone's reorder
 * are not announced — one read a second is the only way the big screen learns
 * the list changed under it.
 *
 * The library needs no polling at all: every path that changes it (`addUrl`,
 * `rename`, `remove`, an upload landing) emits `media-library`, so the listing
 * is fetched when the rail opens and again whenever that event arrives.
 *
 * `railOpen` is passed in rather than guessed because the rail is the only
 * reader of the listing; polling it behind a closed rail costs an IPC a second
 * for a panel nobody is looking at.
 */
export function useMediaViews(open: boolean, railOpen: boolean) {
  const [library, setLibrary] = useState<LibraryView | null>(null);
  const [queue, setQueue] = useState<QueueView | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const refreshQueue = useCallback(() => {
    void media
      .queue()
      .then(setQueue)
      .catch((reason) => setError(String(reason?.message ?? reason)));
  }, []);

  const refreshLibrary = useCallback(() => {
    void media
      .library()
      .then(setLibrary)
      .catch(() => undefined);
  }, []);

  useEffect(() => {
    if (!open) return;
    refreshQueue();
    const timer = window.setInterval(refreshQueue, 1000);
    return () => window.clearInterval(timer);
  }, [open, refreshQueue]);

  useEffect(() => {
    if (!open || !railOpen || library) return;
    refreshLibrary();
  }, [open, railOpen, library, refreshLibrary]);

  useEffect(() => {
    if (!open) return;
    let unlisten: (() => void) | undefined;
    void import("@tauri-apps/api/event")
      .then(({ listen }) => listen("media-library", (event) => setLibrary(event.payload as LibraryView)))
      .then((stop) => {
        unlisten = stop;
      });
    return () => unlisten?.();
  }, [open]);

  /** A queue edit answers with the whole new list, so the row order and the
   *  current highlight move on the click itself instead of waiting for the next
   *  poll. A library edit is covered by `media-library`. */
  const run = useCallback((action: () => Promise<unknown>) => {
    setBusy(true);
    setError(null);
    void action()
      .then((result) => {
        const view = (result as { queue?: QueueView } | null)?.queue ?? (result as QueueView | null);
        if (view && Array.isArray(view.items)) setQueue(view);
      })
      .catch((reason) => setError(String(reason?.message ?? reason)))
      .finally(() => setBusy(false));
  }, []);

  return { library, queue, busy, error, run };
}
