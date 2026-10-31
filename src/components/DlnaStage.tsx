import { useCallback, useEffect, useRef, useState, type RefObject } from "react";
import { createPortal } from "react-dom";
import type { DlnaStatus } from "../types";
import { isTypingTarget } from "../lib/dom";
import { MediaRail, useMediaViews } from "./MediaRail";
import { media, type MediaKind, type PlayState, type QueueTrack } from "../lib/media";
import { CloseIcon } from "./Icons";

interface DlnaStageProps {
  status: DlnaStatus;
  onClose: () => void;
}

const IMAGE_EXTENSIONS = [
  ".jpg",
  ".jpeg",
  ".png",
  ".webp",
  ".gif",
  ".bmp",
  ".heic",
  ".heif",
  ".avif",
];

const AUDIO_EXTENSIONS = [
  ".mp3",
  ".wav",
  ".aac",
  ".flac",
  ".ogg",
  ".oga",
  ".wma",
  ".m4a",
  ".mka",
  ".opus",
];

/** Mirrors `MediaKind::from_uri` in the engine.
 *
 *  The queue already knows a track's kind because the library resolved it, which
 *  is the only reliable answer; this exists for the moment before the first poll
 *  lands, when all that is in hand is an address. Guessing a photo as a video
 *  would put it on the native surface, which does not draw stills — hence the
 *  stage reads the library as soon as it can. */
function kindFromUri(uri: string | null): MediaKind {
  if (!uri) return "video";
  const path = uri.toLowerCase().split(/[?#]/)[0];
  if (IMAGE_EXTENSIONS.some((ext) => path.endsWith(ext))) return "image";
  if (AUDIO_EXTENSIONS.some((ext) => path.endsWith(ext))) return "audio";
  return "video";
}

function formatTime(seconds: number): string {
  const total = Math.max(0, Math.round(seconds));
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const secs = total % 60;
  if (hours > 0) {
    return `${hours}:${String(minutes).padStart(2, "0")}:${String(secs).padStart(2, "0")}`;
  }
  return `${minutes}:${String(secs).padStart(2, "0")}`;
}

function extractTitle(uri: string | null, name: string): string {
  if (!uri) return name || "媒体播放";
  const parts = uri.split("/");
  const file = parts[parts.length - 1] || "";
  const decoded = decodeURIComponent(file);
  const dotIndex = decoded.lastIndexOf(".");
  if (dotIndex > 0) return decoded.slice(0, dotIndex);
  return decoded || name || "媒体播放";
}

/** The polled DLNA status is the same player state read through the receiver's
 *  door, so it can hold the stage up for the first few hundred milliseconds
 *  before the queue view arrives. After that one door answers everything. */
function fromStatus(status: DlnaStatus): PlayState {
  return {
    phase: status.phase,
    uri: status.uri,
    position: status.position,
    duration: status.duration,
    volume: status.volume,
    muted: false,
    error: status.error,
  };
}

export function DlnaStage({ status, onClose }: DlnaStageProps) {
  // The stage opens on the picture: a 340px rail beside the frame shrinks the
  // video the room came to watch, so it stays collapsed until asked for.
  const [railOpen, setRailOpen] = useState(false);
  const { library, queue, busy, error, run } = useMediaViews(true, railOpen);
  const screenRef = useRef<HTMLDivElement | null>(null);

  const state: PlayState = queue?.state ?? fromStatus(status);
  const current: QueueTrack | null =
    queue && queue.current !== null ? (queue.items[queue.current] ?? null) : null;
  const kind = current?.kind ?? kindFromUri(state.uri);
  const isAudio = kind === "audio";
  const isImage = kind === "image";
  const title = current?.title || extractTitle(state.uri, status.name);
  const showSurface =
    !isAudio &&
    !isImage &&
    !state.error &&
    (state.phase === "playing" || state.phase === "paused");

  const act = useCallback(
    (action: ControlAction, value?: number) => {
      switch (action) {
        case "play":
        case "pause":
        case "stop":
          run(() => media.transport(action));
          break;
        case "seek":
          run(() => media.seek(value ?? 0));
          break;
        case "volume":
          run(() => media.setVolume(value ?? 0));
          break;
        case "mute":
          run(() => media.setMute(Boolean(value)));
          break;
      }
    },
    [run],
  );

  const step = useCallback(
    (direction: 1 | -1) => run(() => (direction > 0 ? media.next() : media.previous())),
    [run],
  );

  // The picture is a native MediaEngine child window, not a DOM element, so it
  // has to be told where this stage's screen area is on every layout change —
  // including the change made by opening or closing the rail beside it.
  useEffect(() => {
    if (!showSurface) {
      void media.present(null).catch(() => undefined);
      return;
    }
    let frame = 0;
    const place = () => {
      window.cancelAnimationFrame(frame);
      frame = window.requestAnimationFrame(() => {
        const element = screenRef.current;
        if (!element) return;
        const rect = element.getBoundingClientRect();
        if (rect.width < 2 || rect.height < 2) return;
        void media
          .present([rect.left, rect.top, rect.width, rect.height])
          .catch(() => undefined);
      });
    };
    place();
    window.addEventListener("resize", place);
    const observer = new ResizeObserver(place);
    if (screenRef.current) observer.observe(screenRef.current);
    return () => {
      window.cancelAnimationFrame(frame);
      window.removeEventListener("resize", place);
      observer.disconnect();
    };
  }, [showSurface, state.uri, state.phase]);

  // Leaving the stage must not leave the surface floating over the launcher.
  useEffect(() => {
    return () => {
      void media.present(null).catch(() => undefined);
    };
  }, []);

  const handleKeyDown = useCallback(
    (e: KeyboardEvent) => {
      // The rail is a form: Escape inside its URL field or a select belongs to
      // that control, not to the stage.
      if (e.key === "Escape" && !isTypingTarget(e.target)) {
        e.preventDefault();
        onClose();
      }
    },
    [onClose],
  );

  useEffect(() => {
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [handleKeyDown]);

  const progress = state.duration > 0 ? (state.position / state.duration) * 100 : 0;
  // The rail already shows the reason in its own note; the bar carries it only
  // when the rail is closed, so the same sentence never appears twice.
  const railNote = railOpen ? null : (error ?? queue?.halted ?? null);

  return createPortal(
    <div className="dlna-stage" role="dialog" aria-modal="true" aria-label={`${title} 播放`}>
      <div className="dlna-stage__body">
        <div className="dlna-stage__main">
          {isAudio ? (
            <DlnaMusicPlayer
              title={title}
              deviceName={status.name}
              phase={state.phase}
              position={state.position}
              duration={state.duration}
              progress={progress}
              volume={state.volume}
              muted={state.muted}
              slot={queue?.current ?? null}
              total={queue?.items.length ?? 0}
              onControl={act}
              onStep={step}
            />
          ) : (
            <DlnaVideoPlayer
              title={title}
              phase={state.phase}
              position={state.position}
              duration={state.duration}
              progress={progress}
              error={state.error}
              imageUri={isImage ? state.uri : null}
              screenRef={screenRef}
              volume={state.volume}
              muted={state.muted}
              slot={queue?.current ?? null}
              total={queue?.items.length ?? 0}
              onControl={act}
              onStep={step}
            />
          )}
        </div>
        {railOpen &&
          (queue ? (
            <MediaRail library={library} queue={queue} run={run} busy={busy} error={error} />
          ) : (
            <aside className="media-rail" aria-label="片库与队列">
              <p className="media-rail__empty">正在读取片库…</p>
            </aside>
          ))}
      </div>

      <div className="dlna-stage__bar">
        <div className="dlna-stage__meta">
          <span className="dlna-stage__dot-wrap">
            <span
              className={`dlna-stage__dot ${state.phase === "playing" ? "is-live" : ""}`}
            />
          </span>
          <span className="dlna-stage__phase">{phaseLabel(state.phase)}</span>
          <strong>{title}</strong>
          {queue && queue.items.length > 0 && (
            <span className="dlna-stage__count">
              队列 {(queue.current === null ? 0 : queue.current + 1)} / {queue.items.length}
            </span>
          )}
          {railNote && <span className="dlna-stage__warn">{railNote}</span>}
        </div>
        <div className="dlna-stage__actions">
          <button
            type="button"
            className={`dlna-stage__rail-toggle ${railOpen ? "is-on" : ""}`}
            onClick={() => setRailOpen((open) => !open)}
            aria-pressed={railOpen}
          >
            片库与队列
            <b>{queue?.items.length ?? 0}</b>
          </button>
          <button
            type="button"
            className="dlna-stage__exit"
            onClick={onClose}
            aria-label="退出播放"
          >
            <CloseIcon size={15} />
            退出
            <kbd>Esc</kbd>
          </button>
        </div>
      </div>
    </div>,
    document.body,
  );
}

function phaseLabel(phase: PlayState["phase"]): string {
  const labels: Record<PlayState["phase"], string> = {
    idle: "等待投放",
    loading: "正在加载",
    playing: "正在播放",
    paused: "已暂停",
    stopped: "已停止",
    ended: "播放结束",
    error: "播放出错",
  };
  return labels[phase] ?? phase;
}

type ControlAction = "play" | "pause" | "stop" | "seek" | "volume" | "mute";

type Control = (action: ControlAction, value?: number) => void;
type Step = (direction: 1 | -1) => void;

function PlayGlyph() {
  return (
    <svg width="18" height="18" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
      <path d="M8 5v14l11-7z" />
    </svg>
  );
}

function PauseGlyph() {
  return (
    <svg width="18" height="18" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
      <path d="M7 5h4v14H7zM13 5h4v14h-4z" />
    </svg>
  );
}

function StopGlyph() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
      <rect x="6" y="6" width="12" height="12" rx="2" />
    </svg>
  );
}

function StepGlyph({ back }: { back: boolean }) {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
      <path d={back ? "M7 5h2v14H7zM18 5v14l-9-7zM15 5h2v14h-2z" : "M15 5h2v14h-2zM6 5v14l9-7zM7 5h2v14H7z"} />
    </svg>
  );
}

function SpeakerGlyph({ muted }: { muted: boolean }) {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" aria-hidden="true">
      <path d="M4 9v6h3l5 4V5L7 9H4z" fill="currentColor" stroke="none" />
      {muted ? (
        <path d="M16 9l5 6M21 9l-5 6" />
      ) : (
        <path d="M16 8.5a5 5 0 010 7M18.5 6a8.5 8.5 0 010 12" />
      )}
    </svg>
  );
}

/** Transport controls for the receiver's own screen. The controller on the phone
 *  may be out of hand, so play, pause, seek, skip, stop, volume and mute all work
 *  here too — and each one is the same dispatcher action the controller sends. */
function TransportRow({
  phase,
  position,
  duration,
  progress,
  volume,
  muted,
  seekable,
  slot,
  total,
  onControl,
  onStep,
}: {
  phase: PlayState["phase"];
  position: number;
  duration: number;
  progress: number;
  volume: number;
  muted: boolean;
  seekable: boolean;
  slot: number | null;
  total: number;
  onControl: Control;
  onStep: Step;
}) {
  const playing = phase === "playing";
  const jump = (delta: number) => {
    if (duration <= 0) return;
    onControl("seek", Math.min(Math.max(position + delta, 0), duration));
  };
  const steppable = total > 1;
  return (
    <div className="dlna-transport">
      {seekable && (
        <button
          type="button"
          className="dlna-transport__bar"
          aria-label="跳转进度"
          disabled={duration <= 0}
          onClick={(event) => {
            const box = event.currentTarget.getBoundingClientRect();
            const ratio = (event.clientX - box.left) / box.width;
            if (duration > 0) {
              onControl("seek", Math.min(Math.max(ratio, 0), 1) * duration);
            }
          }}
        >
          <span
            className="dlna-transport__fill"
            style={{ width: `${Math.min(Math.max(progress, 0), 100)}%` }}
          />
        </button>
      )}
      <div className="dlna-transport__row">
        <span className="dlna-transport__time">
          {seekable && duration > 0
            ? `${formatTime(position)} / ${formatTime(duration)}`
            : seekable
              ? "直播"
              : "图片"}
          {steppable && (
            <em className="dlna-transport__slot">
              {slot === null ? "未选中" : `第 ${slot + 1} 条`} · 共 {total} 条
            </em>
          )}
        </span>
        <div className="dlna-transport__buttons">
          {steppable && (
            <button
              type="button"
              className="dlna-transport__button"
              onClick={() => onStep(-1)}
              aria-label="上一条"
            >
              <StepGlyph back />
            </button>
          )}
          {seekable && (
            <button
              type="button"
              className="dlna-transport__button dlna-transport__button--text"
              onClick={() => jump(-10)}
              aria-label="后退 10 秒"
            >
              −10
            </button>
          )}
          <button
            type="button"
            className="dlna-transport__button dlna-transport__button--main"
            onClick={() => onControl(playing ? "pause" : "play")}
            aria-label={playing ? "暂停" : "播放"}
          >
            {playing ? <PauseGlyph /> : <PlayGlyph />}
          </button>
          {seekable && (
            <button
              type="button"
              className="dlna-transport__button dlna-transport__button--text"
              onClick={() => jump(10)}
              aria-label="前进 10 秒"
            >
              +10
            </button>
          )}
          {steppable && (
            <button
              type="button"
              className="dlna-transport__button"
              onClick={() => onStep(1)}
              aria-label="下一条"
            >
              <StepGlyph back={false} />
            </button>
          )}
          <button
            type="button"
            className="dlna-transport__button"
            onClick={() => onControl("stop")}
            aria-label="停止投放"
          >
            <StopGlyph />
          </button>
        </div>
        <label className="dlna-transport__volume">
          <span>音量</span>
          <input
            type="range"
            min={0}
            max={100}
            value={Math.round(Math.min(Math.max(volume, 0), 1) * 100)}
            onChange={(event) => onControl("volume", Number(event.target.value) / 100)}
            aria-label="音量"
          />
          <button
            type="button"
            className={`dlna-transport__button dlna-transport__button--mute ${muted ? "is-muted" : ""}`}
            onClick={() => onControl("mute", muted ? 0 : 1)}
            aria-pressed={muted}
            aria-label={muted ? "取消静音" : "静音"}
          >
            <SpeakerGlyph muted={muted} />
          </button>
        </label>
      </div>
    </div>
  );
}

function DlnaMusicPlayer({
  title,
  deviceName,
  phase,
  position,
  duration,
  progress,
  volume,
  muted,
  slot,
  total,
  onControl,
  onStep,
}: {
  title: string;
  deviceName: string;
  phase: PlayState["phase"];
  position: number;
  duration: number;
  progress: number;
  volume: number;
  muted: boolean;
  slot: number | null;
  total: number;
  onControl: Control;
  onStep: Step;
}) {
  return (
    <div className="dlna-music">
      <div className="dlna-music__art">
        <div className="dlna-music__disc">
          <div className={`dlna-music__vinyl ${phase === "playing" ? "is-spinning" : ""}`}>
            <div className="dlna-music__groove" />
            <div className="dlna-music__groove dlna-music__groove--2" />
            <div className="dlna-music__groove dlna-music__groove--3" />
            <div className="dlna-music__center" />
          </div>
        </div>
        <p className="dlna-music__position">
          {slot === null ? "等待投放" : `第 ${slot + 1} 条 · 共 ${total} 条`}
        </p>
      </div>
      <div className="dlna-music__info">
        <h2 className="dlna-music__title">{title}</h2>
        <p className="dlna-music__device">{deviceName}</p>
      </div>
      <TransportRow
        phase={phase}
        position={position}
        duration={duration}
        progress={progress}
        volume={volume}
        muted={muted}
        seekable
        slot={slot}
        total={total}
        onControl={onControl}
        onStep={onStep}
      />
    </div>
  );
}

function DlnaVideoPlayer({
  title,
  phase,
  position,
  duration,
  progress,
  error,
  imageUri,
  screenRef,
  volume,
  muted,
  slot,
  total,
  onControl,
  onStep,
}: {
  title: string;
  phase: PlayState["phase"];
  position: number;
  duration: number;
  progress: number;
  error: string | null;
  imageUri: string | null;
  screenRef: RefObject<HTMLDivElement | null>;
  volume: number;
  muted: boolean;
  slot: number | null;
  total: number;
  onControl: Control;
  onStep: Step;
}) {
  return (
    <div className="dlna-video">
      <div className="dlna-video__screen" ref={screenRef}>
        {phase === "idle" || phase === "loading" ? (
          <div className="dlna-video__placeholder">
            <div className="dlna-video__spinner" />
            <span>正在加载媒体…</span>
          </div>
        ) : error ? (
          <div className="dlna-video__error">
            <span>播放出错</span>
            <small>{error}</small>
          </div>
        ) : imageUri ? (
          <img className="dlna-video__photo" src={imageUri} alt={title} />
        ) : phase === "ended" ? (
          <div className="dlna-video__placeholder">
            <span>播放完成</span>
          </div>
        ) : (
          <div className="dlna-video__playing">
            <span>{title}</span>
          </div>
        )}
      </div>
      <div className="dlna-video__controls">
        <TransportRow
          phase={phase}
          position={position}
          duration={duration}
          progress={progress}
          volume={volume}
          muted={muted}
          seekable={!imageUri}
          slot={slot}
          total={total}
          onControl={onControl}
          onStep={onStep}
        />
      </div>
    </div>
  );
}
