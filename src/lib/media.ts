import { invoke } from "@tauri-apps/api/core";
import { isTauri } from "./bridge";

/**
 * The launcher's own door to the stage.
 *
 * Every call here is the same action name, argument shape and JSON the
 * extension and the phone use over `POST /api/media/<action>`, because both go
 * through one dispatcher in Rust. So the big screen cannot offer a control the
 * network does not have, and a queue edit made here is immediately what a
 * controller sees.
 */

export type MediaKind = "video" | "audio" | "image";

/** One entry of the hub library — a file that landed in the media folder, or a
 *  URL a user chose to project without uploading. */
export interface LibraryEntry {
  id: string;
  title: string;
  kind: MediaKind;
  file: string | null;
  url: string | null;
  bytes: number;
  addedAt: number;
  source: string;
}

/** One item of the queue. `local` is false for a stored URL, which the room has
 *  to show: that item plays only while the machine serving it stays reachable. */
export interface QueueTrack {
  id: string;
  uri: string;
  title: string;
  kind: MediaKind;
  local: boolean;
}

export interface PlayState {
  phase: "idle" | "loading" | "playing" | "paused" | "stopped" | "ended" | "error";
  uri: string | null;
  position: number;
  duration: number;
  volume: number;
  muted: boolean;
  error: string | null;
}

export interface QueueView {
  items: QueueTrack[];
  current: number | null;
  order: "sequential" | "shuffle";
  repeat: "off" | "all" | "one";
  imageDwell: number;
  playing: boolean;
  /** Why the queue stopped itself, or null. A clean end of a list is not an error. */
  halted: string | null;
  state: PlayState;
}

export interface LibraryView {
  items: LibraryEntry[];
  root: string;
  base: string;
}

export interface StatusView {
  state: PlayState;
  playing: boolean;
  current: QueueTrack | null;
  queue: QueueView;
}

async function call<T>(action: string, args?: Record<string, unknown>): Promise<T> {
  if (!isTauri) throw new Error("仅桌面端可用");
  return invoke<T>("media_invoke", { action, args: args ?? null });
}

export const media = {
  library: () => call<LibraryView>("library"),
  queue: () => call<QueueView>("queue"),
  status: () => call<StatusView>("status"),
  /** Replace the list with these entries and start playing. */
  setQueue: (ids: string[], start?: number) =>
    call<QueueView>("setQueue", { ids, start, autoplay: true }),
  /** Add behind the current item without stealing it. */
  enqueue: (ids: string[]) => call<QueueView>("enqueue", { ids, autoplay: false }),
  clearQueue: () => call<QueueView>("clearQueue"),
  queueRemove: (id: string) => call<QueueView>("queueRemove", { id }),
  queueMove: (id: string, to: number) => call<QueueView>("queueMove", { id, to }),
  jump: (id: string) => call<QueueView>("jump", { id }),
  next: () => call<QueueView>("next"),
  previous: () => call<QueueView>("previous"),
  setNext: (id: string) => call<QueueView>("setNext", { id }),
  setOrder: (order: QueueView["order"]) => call<QueueView>("order", { order }),
  setRepeat: (repeat: QueueView["repeat"]) => call<QueueView>("repeat", { repeat }),
  setDwell: (seconds: number) => call<QueueView>("dwell", { seconds }),
  transport: (action: "play" | "pause" | "stop", value?: number) =>
    call<StatusView>("transport", value === undefined ? { action } : { action, value }),
  seek: (at: number) => call<StatusView>("transport", { action: "seek", at }),
  setVolume: (value: number) => call<StatusView>("transport", { action: "volume", value }),
  /** `mute` reads the value as a boolean, unlike `volume`, which reads a number. */
  setMute: (muted: boolean) => call<StatusView>("transport", { action: "mute", value: muted }),
  /** Places the native video surface, or hides it with null. */
  present: (rect: [number, number, number, number] | null) =>
    call<StatusView>("present", { rect }),
  remove: (id: string) => call<{ removed: boolean; fileError: string | null; queue: QueueView }>("remove", { id }),
  rename: (id: string, title: string) => call<{ item: LibraryEntry }>("rename", { id, title }),
  addUrl: (url: string, title: string) => call<{ item: LibraryEntry }>("addUrl", { url, title }),
};

/** The hub's address for an entry — the one shape the queue prints, with the file
 *  name at the end so any client holding only an address can read the kind. A
 *  stored URL has no file of ours: its own address is the only thing to draw from.
 *  `img-src` allows plain `http:`, and only images cross into the DOM — timed
 *  media plays on the native surface and never becomes a web request. */
export function playUrl(base: string, entry: LibraryEntry): string {
  if (!entry.file) return entry.url ?? "";
  return `${base.replace(/\/$/, "")}/media/${entry.id}/${encodeURIComponent(entry.file)}`;
}

export function formatBytes(bytes: number): string {
  if (bytes <= 0) return "—";
  const units = ["B", "KB", "MB", "GB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value >= 10 || unit === 0 ? Math.round(value) : value.toFixed(1)} ${units[unit]}`;
}
