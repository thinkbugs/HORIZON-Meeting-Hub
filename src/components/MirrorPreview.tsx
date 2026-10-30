import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { Channel } from "@tauri-apps/api/core";
import {
  attachAirplayPreview,
  attachBrowserPreview,
  detachAirplayPreview,
  detachBrowserPreview,
  reportMirrorPerf,
  requestBrowserKeyframe,
  supportsAirplayPreview,
} from "../lib/bridge";
import { FullscreenIcon } from "./Icons";

const HEADER = 24;

/** Decoder queue depth that counts as falling behind. At 30 fps this is about
 * 170 ms of frames the live view can never show on time. */
const MAX_QUEUE = 5;

/** A preview canvas is rarely shown at source resolution, and a 2880×1800
 * bitmap copied every frame at 60 fps is main-thread work the viewer cannot
 * see. The backing store is capped to the box it is drawn in instead. */
const MAX_PIXEL_DENSITY = 2;

type PreviewPhase =
  | "idle"
  | "waiting"
  | "resyncing"
  | "live"
  | "unsupported"
  | "error";

/** Previews currently painting, so the streaming style survives a hand-off
 *  between the panel card and the full-window stage. */
let livePreviews = 0;

interface Size {
  width: number;
  height: number;
}

export interface MirrorView {
  phase: PreviewPhase;
  size: Size | null;
  hint: string;
}

/**
 * Owns the decoder of one mirror source and the `<canvas>` it paints into.
 *
 * AirPlay senders and the browser extension share one wire format: a 24-byte
 * header per frame followed by Annex B H.264, with SPS/PPS in front of every
 * IDR. That lets a decoder be configured from any keyframe, and it lets both
 * receivers feed this one component.
 *
 * The canvas is a component-owned DOM node the panel moves between surfaces
 * instead of a node React re-creates: an AirPlay session carries exactly one
 * IDR at its start, so a decoder that is thrown away when the panel hands the
 * picture to the full-screen stage can never be rebuilt at the other end.
 */
export function useMirrorPreview({
  active,
  source,
  host,
  idleHint,
}: {
  active: boolean;
  source: "airplay" | "browser";
  host: HTMLElement | null;
  idleHint?: string;
}): MirrorView {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  if (!canvasRef.current && typeof document !== "undefined") {
    const element = document.createElement("canvas");
    element.width = 960;
    element.height = 540;
    element.setAttribute("aria-label", "镜像预览");
    canvasRef.current = element;
  }
  const sizeRef = useRef<Size | null>(null);
  const [phase, setPhase] = useState<PreviewPhase>("idle");
  const [size, setSize] = useState<Size | null>(null);
  const [message, setMessage] = useState<string | null>(null);

  // The canvas travels to whichever surface is showing the stream; moving the
  // node keeps its backing store, so the hand-off shows the last frame rather
  // than an empty box.
  useLayoutEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas || !host || canvas.parentElement === host) return;
    host.insertBefore(canvas, host.firstChild);
  }, [host]);

  // The video layer repaints 30–60 times a second while a stream is on screen.
  // A `backdrop-filter` layered above it then re-blurs that surface for every
  // frame, which held the display at 27 fps on an entry-level GPU, so the
  // frosted layers are traded for their flat tint for the length of a session.
  useEffect(() => {
    if (!active || phase !== "live") return;
    livePreviews += 1;
    document.body.classList.add("is-streaming");
    return () => {
      livePreviews = Math.max(0, livePreviews - 1);
      if (!livePreviews) document.body.classList.remove("is-streaming");
    };
  }, [active, phase]);

  useEffect(() => {
    if (!active) {
      setPhase("idle");
      setMessage(null);
      sizeRef.current = null;
      setSize(null);
      return;
    }
    if (!supportsAirplayPreview) {
      setPhase("unsupported");
      return;
    }

    const attach = source === "browser" ? attachBrowserPreview : attachAirplayPreview;
    const detach = source === "browser" ? detachBrowserPreview : detachAirplayPreview;
    // A remount can attach the new channel before the old cleanup runs.
    const owner = `${source}:${crypto.randomUUID?.() ?? Math.random().toString(36).slice(2)}`;

    let disposed = false;
    let decoder: VideoDecoder | null = null;
    let configuredCodec: string | null = null;
    let configuredAt = 0;
    let lastArrival = 0;

    // The preview is a live view, not a recorder: if it cannot keep up it
    // throws the backlog away and re-enters at the next IDR instead of drifting
    // further and further behind.
    let needsKeyframe = true;
    let live = false;
    // Software first: a platform decoder pipelines several frames before it
    // emits the first one — measured 130 ms of arrival-to-paint with a queue of
    // a single frame — and a live mirror cannot spend that. 1080p costs a
    // fraction of a core.
    let acceleration: "no-preference" | "prefer-software" = "prefer-software";
    let platformDecoder = false;
    let lastRequest = 0;
    let lastResync = 0;
    let backlogHits = 0;
    let failures = 0;
    let resyncHint: ReturnType<typeof setTimeout> | null = null;

    // The webview half of the path, which the engine cannot see: a frame that
    // waits in the decoder queue is latency the viewer feels, so each one is
    // timed from arrival to paint and reported once a second.
    const arrivals = new Map<number, number>();
    let painted = 0;
    let skipped = 0;
    let lastPainted = 0;
    let lastSkipped = 0;
    let queueMax = 0;
    let flightSum = 0;
    let flightCount = 0;
    let flightMax = 0;
    let reported = 0;

    // The size the canvas is actually drawn at, so the backing store can match
    // it instead of the source resolution.
    let boxWidth = 0;
    let boxHeight = 0;
    const dpr = Math.min(window.devicePixelRatio || 1, MAX_PIXEL_DENSITY);
    let observer: ResizeObserver | null = null;
    if (typeof ResizeObserver !== "undefined" && canvasRef.current) {
      observer = new ResizeObserver((entries) => {
        const box = entries[entries.length - 1]?.contentRect;
        if (!box) return;
        boxWidth = Math.round(box.width * dpr);
        boxHeight = Math.round(box.height * dpr);
      });
      observer.observe(canvasRef.current);
    }

    // Dev-only probe: how long each frame waits between IPC arrival, decode and
    // paint, plus wall-clock capture latency when the sender marks its PTS as
    // `Date.now()` microseconds (flag bit 1).
    const probe = import.meta.env.DEV
      ? {
          source,
          arrivals: new Map<number, { at: number; wall: boolean }>(),
          ipc: [] as number[],
          glass: [] as number[],
          wire: [] as number[],
          gaps: [] as number[],
          queue: [] as number[],
          draw: [] as number[],
          decodeCall: [] as number[],
          down: [] as number[],
          decodeAt: new Map<number, number>(),
          drops: 0,
          decoded: 0,
          error: null as string | null,
          codec: null as string | null,
          visibility: document.visibilityState,
          raw: [] as { key: boolean; pts: number; data: Uint8Array }[],
          ring: [] as { key: boolean; pts: number; data: Uint8Array }[],
          log: [] as { k: boolean; len: number; q: number }[],
          crash: null as null | { key: boolean; pts: number; data: Uint8Array }[],
          rawBytes: 0,
          valid: 0,
          pushed: 0,
          frozen: false,
          resyncs: 0,
          accel: null as string | null,
          failure: null as null | { at: number; decoded: number; key: boolean },
        }
      : null;
    if (probe) {
      (window as unknown as Record<string, unknown>).__mirrorProbe = probe;
    }
    const record = (list: number[], value: number) => {
      list.push(value);
      if (list.length > 600) list.splice(0, list.length - 600);
    };

    /**
     * At most one `drawImage` per display frame. A decoder that fell behind
     * emits its backlog in bursts, and every paint but the last one is
     * main-thread work the compositor throws away before it reaches the screen.
     */
    let pending: VideoFrame | null = null;
    let scheduled = 0;

    const closeDecoder = () => {
      const current = decoder;
      decoder = null;
      configuredCodec = null;
      configuredAt = 0;
      // A frame decoded by the decoder being thrown away is not something the
      // rebuilt chain should paint after the new keyframe arrives.
      pending?.close();
      pending = null;
      if (current && current.state !== "closed") {
        try {
          current.close();
        } catch {
          // A decoder that failed on its own is already closed.
        }
      }
    };

    const fatal = (error: unknown) => {
      if (disposed) return;
      if (import.meta.env.DEV) {
        console.error("MirrorPreview decoder failure", error);
      }
      if (probe) probe.error = `${(error as Error)?.name}: ${(error as Error)?.message}`;
      if (probe) probe.frozen = true;
      if (probe) probe.crash = probe.ring.slice();
      if (probe) probe.failure = { at: probe.decoded, decoded: probe.decoded, key: probe.log[probe.log.length - 1]?.k ?? false };
      closeDecoder();
      needsKeyframe = true;
      setPhase("error");
      setMessage(error instanceof Error ? error.message : String(error));
    };

    /**
     * A decode error usually means the reference chain broke, not that the
     * stream is unusable. Rebuild the decoder and enter again at an IDR; only
     * repeated failures without a single painted frame are fatal.
     */
    const recover = (error: unknown) => {
      if (disposed) return;
      if (probe) {
        probe.error ??= `${(error as Error)?.name}: ${(error as Error)?.message}`;
        probe.crash = probe.ring.slice();
        probe.failure ??= { at: probe.decoded, decoded: probe.decoded, key: probe.log[probe.log.length - 1]?.k ?? false };
      }
      failures += 1;
      if (failures > 3) {
        fatal(error);
        return;
      }
      if (import.meta.env.DEV) {
        console.warn("MirrorPreview resyncing after decode error", error);
      }
      if (!platformDecoder) {
        // A CPU decoder that cannot drive an unusual stream (a 4K60 sender on a
        // weak laptop) gets one chance at the platform path.
        platformDecoder = true;
        acceleration = "no-preference";
      }
      closeDecoder();
      needsKeyframe = true;
      live = false;
      backlogHits = 0;
      if (probe) probe.resyncs += 1;
      setPhase("resyncing");
      setMessage(null);
      requestKeyframeSoon();
    };

    /** Asks the sender for an IDR, sparingly: every request costs a full frame. */
    const requestKeyframeSoon = () => {
      if (source !== "browser") return;
      const now = performance.now();
      if (now - lastRequest < 700) return;
      lastRequest = now;
      void requestBrowserKeyframe();
    };

    /**
     * Drops the decoder and everything queued behind it. The CPU path is the
     * default and decodes 1080p with plenty of headroom; a backlog seen while
     * running on the platform decoder means that decoder is stalling, and the
     * session goes back to the CPU path for good.
     */
    const resync = (backlog: boolean) => {
      if (disposed) return;
      const now = performance.now();
      if (now - lastResync < 250) return;
      lastResync = now;
      if (backlog && platformDecoder) {
        platformDecoder = false;
        acceleration = "prefer-software";
      }
      closeDecoder();
      needsKeyframe = true;
      live = false;
      backlogHits = 0;
      if (probe) probe.resyncs += 1;
      setPhase("resyncing");
      setMessage(null);
      if (resyncHint) clearTimeout(resyncHint);
      resyncHint = setTimeout(() => {
        resyncHint = null;
        if (!disposed && !live) setMessage("正在重新同步画面…");
      }, 900);
      requestKeyframeSoon();
    };

    const paint = () => {
      scheduled = 0;
      const frame = pending;
      if (!frame) return;
      pending = null;
      if (disposed) {
        frame.close();
        return;
      }
      const canvas = canvasRef.current;
      if (!canvas || !canvas.isConnected) {
        // No surface is on screen — the panel is showing another tab. Keep
        // decoding so the reference chain stays alive (the session will never
        // send another IDR to rebuild it from), but skip the blit.
        arrivals.delete(frame.timestamp);
        frame.close();
        return;
      }
      const arrived = arrivals.get(frame.timestamp);
      if (arrived !== undefined) {
        arrivals.delete(frame.timestamp);
        const flight = performance.now() - arrived;
        flightSum += flight;
        flightCount += 1;
        if (flight > flightMax) flightMax = flight;
      }
      const ratio =
        frame.displayHeight > 0 ? frame.displayWidth / frame.displayHeight : 16 / 9;
      let width = frame.displayWidth;
      let height = frame.displayHeight;
      if (boxWidth > 0 && boxHeight > 0) {
        width = Math.min(width, boxWidth);
        height = Math.round(width / ratio);
        if (height > boxHeight) {
          height = boxHeight;
          width = Math.round(height * ratio);
        }
      }
      if (width < 2 || height < 2) {
        width = frame.displayWidth;
        height = frame.displayHeight;
      }
      if (canvas.width !== width || canvas.height !== height) {
        canvas.width = width;
        canvas.height = height;
      }
      if (
        sizeRef.current?.width !== frame.displayWidth ||
        sizeRef.current?.height !== frame.displayHeight
      ) {
        sizeRef.current = { width: frame.displayWidth, height: frame.displayHeight };
        setSize(sizeRef.current);
      }
      const startedDraw = performance.now();
      canvas
        .getContext("2d", { alpha: false })
        ?.drawImage(frame, 0, 0, canvas.width, canvas.height);
      painted += 1;
      if (probe) record(probe.draw, performance.now() - startedDraw);
      if (probe) {
        const arrival = probe.arrivals.get(frame.timestamp);
        if (arrival) {
          probe.arrivals.delete(frame.timestamp);
          record(probe.ipc, performance.now() - arrival.at);
          if (arrival.wall) record(probe.glass, Date.now() - frame.timestamp / 1000);
        }
        const queued = probe.decodeAt.get(frame.timestamp);
        if (queued !== undefined) {
          probe.decodeAt.delete(frame.timestamp);
          record(probe.down, performance.now() - queued);
        }
      }
      if (!live) {
        live = true;
        failures = 0;
        if (resyncHint) {
          clearTimeout(resyncHint);
          resyncHint = null;
        }
        setPhase("live");
        setMessage(null);
      }
      frame.close();
    };

    const draw = (frame: VideoFrame) => {
      if (disposed) {
        frame.close();
        return;
      }
      // A decoded frame a newer one already superseded never reaches the screen.
      // Comparing against the newest *arrival* would be wrong: a hardware
      // decoder holds several frames in its pipeline, so every output is already
      // ~130 ms old when it appears, and that policy painted 12 of 27 frames.
      if (pending) {
        skipped += 1;
        pending.close();
      }
      pending = frame;
      if (!scheduled) scheduled = requestAnimationFrame(paint);
    };

    /**
     * Configures once per codec string. No `description` is passed: every
     * keyframe carries SPS/PPS in Annex B, and a decoder given an avcC record
     * would expect length-prefixed samples instead.
     */
    const configure = (payload: Uint8Array): boolean => {
      const codec = avcCodec(payload);
      if (!codec) return false;
      if (decoder && decoder.state === "configured" && configuredCodec === codec) {
        return true;
      }
      try {
        closeDecoder();
        const next = new VideoDecoder({ output: draw, error: recover });
        next.configure({
          codec,
          optimizeForLatency: true,
          hardwareAcceleration: acceleration,
        });
        decoder = next;
        configuredCodec = codec;
        configuredAt = performance.now();
        if (probe) {
          probe.codec = codec;
          probe.accel = acceleration;
        }
        return true;
      } catch (error) {
        fatal(error);
        return false;
      }
    };

    const channel = new Channel<ArrayBuffer>();
    channel.onmessage = (body) => {
      if (disposed || !(body instanceof ArrayBuffer) || body.byteLength < HEADER) {
        return;
      }
      const view = new DataView(body);
      if (view.getUint8(0) !== 1) return;
      const flags = view.getUint8(1);
      const keyframe = (flags & 1) === 1;
      const wall = (flags & 2) === 2;
      const gap = (flags & 4) === 4;
      const ptsUs = Number(view.getBigUint64(4, true));
      const declared = view.getUint32(16, true);
      const end = Math.min(body.byteLength, HEADER + declared);
      if (end <= HEADER) return;
      const payload = new Uint8Array(body, HEADER, end - HEADER);

      const at = performance.now();
      const queued = decoder?.decodeQueueSize ?? 0;
      if (queued > queueMax) queueMax = queued;
      arrivals.set(ptsUs, at);
      if (arrivals.size > 240) {
        const oldest = arrivals.keys().next().value;
        if (oldest !== undefined) arrivals.delete(oldest);
      }
      if (probe) {
        if (lastArrival > 0) record(probe.gaps, at - lastArrival);
        lastArrival = at;
        if (wall) record(probe.wire, Date.now() - ptsUs / 1000);
        if (decoder) record(probe.queue, decoder.decodeQueueSize);
        probe.valid += 1;
        probe.log.push({ k: keyframe, len: payload.length, q: decoder?.decodeQueueSize ?? -1 });
        if (probe.log.length > 300) probe.log.splice(0, probe.log.length - 300);
        if (keyframe) probe.ring = [];
        const recording = (window as unknown as Record<string, unknown>).__mirrorRecord === true;
        if (recording && payload.length < 1_500_000) {
          probe.ring.push({ key: keyframe, pts: ptsUs, data: payload.slice() });
          if (probe.ring.length > 120) probe.ring.splice(0, probe.ring.length - 120);
        }
        if (!probe.frozen && probe.rawBytes + payload.length < 4_000_000) {
          probe.raw.push({ key: keyframe, pts: ptsUs, data: payload.slice() });
          probe.rawBytes += payload.length;
          probe.pushed += 1;
        }
      }

      if (keyframe) {
        // A keyframe is a clean entry point even when the previous decoder was
        // hopelessly behind, so flush it and start over on this frame.
        if (decoder && decoder.decodeQueueSize > MAX_QUEUE) {
          closeDecoder();
        }
        if (!configure(payload)) {
          needsKeyframe = true;
          return;
        }
        needsKeyframe = false;
      } else {
        if (needsKeyframe || gap) {
          // A delta after a hole is undecodable. Skipping it is the only safe
          // move; the sender is asked for a fresh IDR right away.
          needsKeyframe = true;
          requestKeyframeSoon();
          return;
        }
        if (!decoder || decoder.state !== "configured") {
          needsKeyframe = true;
          requestKeyframeSoon();
          return;
        }
        // Sustained backlog means the decoder cannot keep up with the sender.
        // Drop it rather than let the live view drift by seconds. One spike
        // can be a warm-up or a GC pause; two arrivals in a row with a full
        // queue at 30 fps cannot drain. The grace period skips the first
        // keyframe's warm-up burst.
        if (
          source === "browser" &&
          at - configuredAt > 1200 &&
          decoder.decodeQueueSize > MAX_QUEUE
        ) {
          backlogHits += 1;
          if (backlogHits >= 2) {
            resync(true);
            return;
          }
        } else {
          backlogHits = 0;
        }
      }

      if (!decoder || decoder.state !== "configured") return;
      if (probe) {
        probe.arrivals.set(ptsUs, { at, wall });
        if (probe.arrivals.size > 240) {
          const oldest = probe.arrivals.keys().next().value;
          if (oldest !== undefined) probe.arrivals.delete(oldest);
        }
      }
      try {
        const startedDecode = performance.now();
        if (probe) probe.decodeAt.set(ptsUs, startedDecode);
        decoder.decode(
          new EncodedVideoChunk({
            type: keyframe ? "key" : "delta",
            timestamp: ptsUs,
            data: payload,
          }),
        );
        if (probe) record(probe.decodeCall, performance.now() - startedDecode);
        if (probe) probe.decoded += 1;
      } catch (error) {
        recover(error);
      }
    };

    const reporter = window.setInterval(() => {
      // Only report while frames are moving: an idle preview must not overwrite
      // the numbers of the one that is actually on screen.
      if (painted + skipped === reported) return;
      reported = painted + skipped;
      void reportMirrorPerf({
        painted,
        // Per window, like the fps: a cumulative count is unreadable next to a
        // one-second paint rate.
        skipped: skipped - lastSkipped,
        queueMax,
        inFlightAvgMs: flightCount ? flightSum / flightCount : 0,
        inFlightMaxMs: flightMax,
        fps: painted - lastPainted,
      });
      lastPainted = painted;
      lastSkipped = skipped;
      queueMax = 0;
      flightSum = 0;
      flightCount = 0;
      flightMax = 0;
    }, 1000);

    setPhase("waiting");
    setMessage(null);
    attach(owner, channel)
      .then(() => {
        // Joining mid-stream means waiting for the sender's next periodic IDR,
        // which can be seconds away. Ask for one as soon as the channel is up.
        if (source === "browser" && !disposed) requestKeyframeSoon();
      })
      .catch(fatal);

    return () => {
      disposed = true;
      window.clearInterval(reporter);
      if (scheduled) cancelAnimationFrame(scheduled);
      pending?.close();
      pending = null;
      observer?.disconnect();
      if (resyncHint) {
        clearTimeout(resyncHint);
        resyncHint = null;
      }
      void detach(owner);
      closeDecoder();
    };
  }, [active, source]);

  const hint = useMemo(() => {
    const waiting = idleHint ?? (source === "browser" ? "等待浏览器扩展开始投放" : "等待 iPhone、iPad 或 Mac 开始镜像");
    if (phase === "unsupported") {
      return "当前 WebView 不支持 WebCodecs，无法在面板内预览；接收与播放不受影响";
    }
    if (phase === "live") return "";
    // Short resyncs stay silent; only a stall long enough to notice explains
    // itself.
    if (phase === "resyncing") return message ?? "";
    if (phase === "error") return message ?? "画面解码失败";
    return waiting;
  }, [idleHint, message, phase, source]);

  return { phase, size, hint };
}

/**
 * The box a mirror stream is shown in. Registering itself as the session's
 * host is what moves the canvas here, so a card and the full-screen stage can
 * alternate without ever rebuilding the decoder.
 */
export function MirrorSurface({
  view,
  onHost,
  variant = "card",
  onClick,
}: {
  view: MirrorView;
  onHost?: (element: HTMLDivElement | null) => void;
  variant?: "card" | "stage";
  onClick?: () => void;
}) {
  const hostRef = useRef<HTMLDivElement | null>(null);
  useLayoutEffect(() => {
    if (!onHost) return;
    onHost(hostRef.current);
    return () => onHost(null);
  }, [onHost]);

  const presentable = Boolean(onClick);
  return (
    <div
      ref={hostRef}
      className={`cast-preview${variant === "stage" ? " cast-preview--stage" : ""}${presentable ? " is-clickable" : ""}`}
      onClick={onClick}
      onKeyDown={
        onClick
          ? (event) => {
              if (event.key !== "Enter" && event.key !== " ") return;
              event.preventDefault();
              onClick();
            }
          : undefined
      }
      role={presentable ? "button" : undefined}
      tabIndex={presentable ? 0 : undefined}
      aria-label={presentable ? "全屏观看投屏画面" : undefined}
    >
      {view.hint && (
        <div className={`cast-preview__hint${view.phase === "error" ? " is-error" : ""}`}>
          {view.hint}
        </div>
      )}
      {view.phase === "live" && (
        <span className="cast-preview__badge">
          H.264
          {view.size ? ` · ${view.size.width}×${view.size.height}` : ""}
        </span>
      )}
      {presentable && (
        <span className="cast-preview__open" aria-hidden="true">
          <FullscreenIcon size={13} />
          全屏观看
        </span>
      )}
    </div>
  );
}

/** Reads the H.264 profile and level from the SPS in an Annex B keyframe. */
function avcCodec(payload: Uint8Array): string | null {
  let sps: Uint8Array | null = null;
  let at = 0;
  while (at + 3 < payload.length) {
    const start = nextStart(payload, at);
    if (start >= payload.length - 3) break;
    const header = start + (payload[start + 2] === 1 ? 3 : 4);
    const end = nextStart(payload, header);
    if (end <= header) break;
    if ((payload[header] & 0x1f) === 7) {
      sps = payload.subarray(header, end);
      break;
    }
    at = end;
  }
  if (!sps || sps.length < 4) return null;
  return `avc1.${hex(sps[1])}${hex(sps[2])}${hex(sps[3])}`;
}

function hex(value: number): string {
  return value.toString(16).padStart(2, "0");
}

/** Index of the next Annex B start code, or the end of the buffer. */
function nextStart(payload: Uint8Array, from: number): number {
  for (let at = from; at + 3 <= payload.length; at += 1) {
    if (payload[at] !== 0 || payload[at + 1] !== 0) continue;
    if (payload[at + 2] === 1) return at;
    if (payload[at + 2] === 0 && at + 4 <= payload.length && payload[at + 3] === 1) {
      return at;
    }
  }
  return payload.length;
}
