import { useCallback, useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import type { DlnaStatus } from "../types";
import { CloseIcon } from "./Icons";

interface DlnaStageProps {
  status: DlnaStatus;
  onClose: () => void;
}

function isAudioContent(uri: string | null): boolean {
  if (!uri) return false;
  const lower = uri.toLowerCase();
  return (
    lower.includes("/audio") ||
    lower.endsWith(".mp3") ||
    lower.endsWith(".wav") ||
    lower.endsWith(".flac") ||
    lower.endsWith(".aac") ||
    lower.endsWith(".ogg") ||
    lower.endsWith(".m4a") ||
    lower.endsWith(".wma")
  );
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

export function DlnaStage({ status, onClose }: DlnaStageProps) {
  const isAudio = isAudioContent(status.uri);
  const title = extractTitle(status.uri, status.name);
  const [visualizerBars, setVisualizerBars] = useState<number[]>(() =>
    Array.from({ length: 32 }, () => Math.random() * 0.3 + 0.1),
  );
  const animRef = useRef<number>(0);

  useEffect(() => {
    if (!isAudio || status.phase !== "playing") {
      cancelAnimationFrame(animRef.current);
      return;
    }
    const animate = () => {
      setVisualizerBars((prev) =>
        prev.map((v) => {
          const target = Math.random() * 0.8 + 0.1;
          return v + (target - v) * 0.15;
        }),
      );
      animRef.current = requestAnimationFrame(animate);
    };
    animRef.current = requestAnimationFrame(animate);
    return () => cancelAnimationFrame(animRef.current);
  }, [isAudio, status.phase]);

  const handleKeyDown = useCallback(
    (e: KeyboardEvent) => {
      if (e.key === "Escape") {
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

  const progress = status.duration > 0 ? (status.position / status.duration) * 100 : 0;

  return createPortal(
    <div className="dlna-stage" role="dialog" aria-modal="true" aria-label={`${title} 播放`}>
      {isAudio ? (
        <DlnaMusicPlayer
          title={title}
          deviceName={status.name}
          phase={status.phase}
          position={status.position}
          duration={status.duration}
          progress={progress}
          bars={visualizerBars}
        />
      ) : (
        <DlnaVideoPlayer
          title={title}
          phase={status.phase}
          position={status.position}
          duration={status.duration}
          progress={progress}
          error={status.error}
        />
      )}

      <div className="dlna-stage__bar">
        <div className="dlna-stage__meta">
          <span className="dlna-stage__dot-wrap">
            <span
              className={`dlna-stage__dot ${status.phase === "playing" ? "is-live" : ""}`}
            />
          </span>
          <span className="dlna-stage__phase">{phaseLabel(status.phase)}</span>
          <strong>{title}</strong>
          {status.duration > 0 && (
            <span className="dlna-stage__time">
              {formatTime(status.position)} / {formatTime(status.duration)}
            </span>
          )}
        </div>
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
    </div>,
    document.body,
  );
}

function phaseLabel(phase: DlnaStatus["phase"]): string {
  const labels: Record<DlnaStatus["phase"], string> = {
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

function DlnaMusicPlayer({
  title,
  deviceName,
  phase,
  position,
  duration,
  progress,
  bars,
}: {
  title: string;
  deviceName: string;
  phase: DlnaStatus["phase"];
  position: number;
  duration: number;
  progress: number;
  bars: number[];
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
        <div className="dlna-music__visualizer">
          {bars.map((height, i) => (
            <div
              key={i}
              className="dlna-music__bar"
              style={{ transform: `scaleY(${height})` }}
            />
          ))}
        </div>
      </div>
      <div className="dlna-music__info">
        <h2 className="dlna-music__title">{title}</h2>
        <p className="dlna-music__device">{deviceName}</p>
      </div>
      <div className="dlna-music__progress">
        <div className="dlna-music__progress-bar">
          <div
            className="dlna-music__progress-fill"
            style={{ width: `${progress}%` }}
          />
        </div>
        <div className="dlna-music__progress-times">
          <span>{formatTime(position)}</span>
          <span>{duration > 0 ? formatTime(duration) : "--:--"}</span>
        </div>
      </div>
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
}: {
  title: string;
  phase: DlnaStatus["phase"];
  position: number;
  duration: number;
  progress: number;
  error: string | null;
}) {
  return (
    <div className="dlna-video">
      <div className="dlna-video__screen">
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
        ) : phase === "ended" ? (
          <div className="dlna-video__placeholder">
            <span>播放完成</span>
          </div>
        ) : (
          <div className="dlna-video__playing">
            <div className="dlna-video__play-icon">
              <svg width="48" height="48" viewBox="0 0 24 24" fill="currentColor">
                <path d="M8 5v14l11-7z" />
              </svg>
            </div>
            <span>{title}</span>
          </div>
        )}
      </div>
      <div className="dlna-video__controls">
        <div className="dlna-video__progress-bar">
          <div
            className="dlna-video__progress-fill"
            style={{ width: `${progress}%` }}
          />
        </div>
        <div className="dlna-video__controls-row">
          <span className="dlna-video__time">
            {formatTime(position)}
            {duration > 0 ? ` / ${formatTime(duration)}` : ""}
          </span>
          <span className="dlna-video__phase-badge">{phaseLabel(phase)}</span>
        </div>
      </div>
    </div>
  );
}
