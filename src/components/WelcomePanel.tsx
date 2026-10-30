import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ChangeEvent,
  type CSSProperties,
  type ReactNode,
} from "react";
import { createPortal } from "react-dom";
import type { WelcomeTheme } from "../types";
import {
  DEFAULT_WELCOME_THEME,
  WELCOME_MAX_INTERVAL,
  WELCOME_MIN_INTERVAL,
  createId,
} from "../lib/state";
import { CloseIcon, PlusIcon, TrashIcon } from "./Icons";

/* ------------------------------------------------------------------
   Types
------------------------------------------------------------------ */

export interface WelcomePanelProps {
  themes: WelcomeTheme[];
  activeThemeId: string;
  onThemesChange: (themes: WelcomeTheme[]) => void;
  onActiveThemeChange: (id: string) => void;
  onClose: () => void;
  onNotify: (text: string, tone?: "success" | "error" | "info") => void;
}

/* ------------------------------------------------------------------
   Constants
------------------------------------------------------------------ */

const FADE_DURATION_MS = 800;
const MIN_INTERVAL = WELCOME_MIN_INTERVAL;
const MAX_INTERVAL = WELCOME_MAX_INTERVAL;
const DEFAULT_INTERVAL = DEFAULT_WELCOME_THEME.interval;

/* ------------------------------------------------------------------
   Helpers
------------------------------------------------------------------ */

function isGradientDataUrl(value: string): boolean {
  return value.startsWith("linear-gradient(") || value.startsWith("radial-gradient(");
}

function readFileAsDataUrl(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(reader.result as string);
    reader.onerror = () => reject(reader.error);
    reader.readAsDataURL(file);
  });
}

function clampInterval(value: number): number {
  return Math.max(MIN_INTERVAL, Math.min(MAX_INTERVAL, Math.round(value)));
}

/* ------------------------------------------------------------------
   Inline SVG icons for transport controls
------------------------------------------------------------------ */

function PlaySvg({ size = 18 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
      <path d="M6.5 4.3a1.4 1.4 0 0 1 2.1-1.2l12 7.7a1.4 1.4 0 0 1 0 2.4l-12 7.7A1.4 1.4 0 0 1 6.5 19.7V4.3Z" />
    </svg>
  );
}

function PauseSvg({ size = 18 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
      <rect x="5" y="3.5" width="4.6" height="17" rx="1.3" />
      <rect x="14.4" y="3.5" width="4.6" height="17" rx="1.3" />
    </svg>
  );
}

function PrevSvg({ size = 18 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
      <rect x="3.5" y="4" width="2.4" height="16" rx="1" />
      <path d="M20.2 4.6a1.2 1.2 0 0 1 .4 1.7L14 12l6.6 5.7a1.2 1.2 0 0 1-1.6 1.8l-8-6.9a1.2 1.2 0 0 1 0-1.8l8-6.9a1.2 1.2 0 0 1 1.2-.3Z" />
    </svg>
  );
}

function NextSvg({ size = 18 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
      <rect x="18.1" y="4" width="2.4" height="16" rx="1" />
      <path d="M3.8 4.6a1.2 1.2 0 0 0-.4 1.7L10 12l-6.6 5.7a1.2 1.2 0 0 0 1.6 1.8l8-6.9a1.2 1.2 0 0 0 0-1.8l-8-6.9a1.2 1.2 0 0 0-1.2-.3Z" />
    </svg>
  );
}

function MusicNoteSvg({ size = 16 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M9 18V5l12-2v13" />
      <circle cx="6" cy="18" r="3" />
      <circle cx="18" cy="16" r="3" />
    </svg>
  );
}

function GearSvg({ size = 18 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M12 15.4a3.4 3.4 0 1 0 0-6.8 3.4 3.4 0 0 0 0 6.8Z" />
      <path d="M19.6 15a1.7 1.7 0 0 0 .34 1.87l.06.06a2.06 2.06 0 1 1-2.92 2.92l-.06-.06a1.7 1.7 0 0 0-1.87-.34 1.7 1.7 0 0 0-1.03 1.56V21a2.06 2.06 0 1 1-4.12 0v-.1a1.7 1.7 0 0 0-1.11-1.56 1.7 1.7 0 0 0-1.87.34l-.06.06a2.06 2.06 0 1 1-2.92-2.92l.06-.06A1.7 1.7 0 0 0 4.4 15a1.7 1.7 0 0 0-1.56-1.03H2.7a2.06 2.06 0 1 1 0-4.12h.1A1.7 1.7 0 0 0 4.4 8.73a1.7 1.7 0 0 0-.34-1.87l-.06-.06A2.06 2.06 0 1 1 6.92 3.9l.06.06a1.7 1.7 0 0 0 1.87.34H9a1.7 1.7 0 0 0 1.03-1.56V2.6a2.06 2.06 0 1 1 4.12 0v.1A1.7 1.7 0 0 0 15.2 4.3a1.7 1.7 0 0 0 1.87-.34l.06-.06a2.06 2.06 0 1 1 2.92 2.92l-.06.06a1.7 1.7 0 0 0-.34 1.87v.08a1.7 1.7 0 0 0 1.56 1.03h.19a2.06 2.06 0 1 1 0 4.12h-.1a1.7 1.7 0 0 0-1.56 1.03Z" />
    </svg>
  );
}

/* ------------------------------------------------------------------
   Slide renderer
------------------------------------------------------------------ */

function SlideImage({
  src,
  visible,
}: {
  src: string;
  visible: boolean;
}) {
  const style: CSSProperties = {
    position: "absolute",
    inset: 0,
    opacity: visible ? 1 : 0,
    transition: `opacity ${FADE_DURATION_MS}ms ease`,
    willChange: "opacity",
  };

  if (isGradientDataUrl(src)) {
    return <div style={{ ...style, backgroundImage: src }} />;
  }

  return (
    <img
      src={src}
      alt=""
      draggable={false}
      style={{ ...style, width: "100%", height: "100%", objectFit: "cover" }}
    />
  );
}

/* ------------------------------------------------------------------
   Theme editor (slide-out panel)
------------------------------------------------------------------ */

interface ThemeEditorProps {
  themes: WelcomeTheme[];
  activeThemeId: string;
  onThemesChange: (themes: WelcomeTheme[]) => void;
  onActiveThemeChange: (id: string) => void;
  onClose: () => void;
  onNotify: (text: string, tone?: "success" | "error" | "info") => void;
}

function ThemeEditor({
  themes,
  activeThemeId,
  onThemesChange,
  onActiveThemeChange,
  onClose,
  onNotify,
}: ThemeEditorProps) {
  const activeTheme = themes.find((t) => t.id === activeThemeId) ?? themes[0] ?? null;
  const imageInputRef = useRef<HTMLInputElement>(null);
  const musicInputRef = useRef<HTMLInputElement>(null);

  const updateActiveTheme = useCallback(
    (patch: Partial<WelcomeTheme>) => {
      if (!activeTheme) return;
      const next = themes.map((t) => (t.id === activeTheme.id ? { ...t, ...patch } : t));
      onThemesChange(next);
    },
    [activeTheme, themes, onThemesChange],
  );

  const handleAddImages = useCallback(async () => {
    imageInputRef.current?.click();
  }, []);

  const handleImagePicked = useCallback(
    async (event: ChangeEvent<HTMLInputElement>) => {
      const files = event.target.files;
      if (!files || files.length === 0 || !activeTheme) return;

      try {
        const urls = await Promise.all(Array.from(files).map(readFileAsDataUrl));
        updateActiveTheme({ images: [...activeTheme.images, ...urls] });
        onNotify(`已添加 ${urls.length} 张图片`, "success");
      } catch {
        onNotify("图片读取失败", "error");
      }

      // Reset so the same file can be picked again
      event.target.value = "";
    },
    [activeTheme, updateActiveTheme, onNotify],
  );

  const handleRemoveImage = useCallback(
    (index: number) => {
      if (!activeTheme) return;
      const next = activeTheme.images.filter((_, i) => i !== index);
      updateActiveTheme({ images: next });
    },
    [activeTheme, updateActiveTheme],
  );

  const handleMusicPick = useCallback(
    async (event: ChangeEvent<HTMLInputElement>) => {
      const files = event.target.files;
      if (!files || files.length === 0 || !activeTheme) return;

      try {
        const urls = await Promise.all(Array.from(files).map(readFileAsDataUrl));
        updateActiveTheme({ musicUrls: [...activeTheme.musicUrls, ...urls] });
        onNotify(`已加入 ${urls.length} 首背景音乐`, "success");
      } catch {
        onNotify("音频读取失败", "error");
      }

      event.target.value = "";
    },
    [activeTheme, updateActiveTheme, onNotify],
  );

  const handleRemoveMusic = useCallback(
    (index: number) => {
      if (!activeTheme) return;
      updateActiveTheme({ musicUrls: activeTheme.musicUrls.filter((_, i) => i !== index) });
    },
    [activeTheme, updateActiveTheme],
  );

  const handleAddTheme = useCallback(() => {
    const id = createId();
    const newTheme: WelcomeTheme = {
      id,
      name: `主题 ${themes.length + 1}`,
      images: [],
      interval: DEFAULT_INTERVAL,
      musicUrls: [],
    };
    onThemesChange([...themes, newTheme]);
    onActiveThemeChange(id);
    onNotify("已新建主题", "success");
  }, [themes, onThemesChange, onActiveThemeChange, onNotify]);

  const handleRemoveTheme = useCallback(
    (id: string) => {
      if (themes.length <= 1) {
        onNotify("至少保留一个主题", "error");
        return;
      }
      const next = themes.filter((t) => t.id !== id);
      onThemesChange(next);
      if (activeThemeId === id) {
        onActiveThemeChange(next[0].id);
      }
      onNotify("已删除主题", "info");
    },
    [themes, activeThemeId, onThemesChange, onActiveThemeChange, onNotify],
  );

  return (
    <div className="welcome-editor-backdrop" onMouseDown={(e) => { if (e.target === e.currentTarget) onClose(); }}>
      <div className="welcome-editor" role="dialog" aria-modal="true" aria-label="主题设置">
        {/* Header */}
        <div className="welcome-editor__header">
          <h2 className="welcome-editor__title">主题设置</h2>
          <button className="welcome-editor__close" onClick={onClose} aria-label="关闭设置">
            <CloseIcon size={17} />
          </button>
        </div>

        {/* Theme selector */}
        <div className="welcome-editor__section">
          <label className="welcome-editor__label">主题</label>
          <div className="welcome-editor__theme-row">
            <select
              className="welcome-editor__select"
              value={activeThemeId}
              onChange={(e) => onActiveThemeChange(e.target.value)}
            >
              {themes.map((t) => (
                <option key={t.id} value={t.id}>
                  {t.name}
                </option>
              ))}
            </select>
            <button className="welcome-editor__icon-btn" onClick={handleAddTheme} title="新建主题" aria-label="新建主题">
              <PlusIcon size={16} />
            </button>
            {themes.length > 1 && activeTheme && (
              <button
                className="welcome-editor__icon-btn welcome-editor__icon-btn--danger"
                onClick={() => handleRemoveTheme(activeTheme.id)}
                title="删除当前主题"
                aria-label="删除当前主题"
              >
                <TrashIcon size={16} />
              </button>
            )}
          </div>
        </div>

        {activeTheme && (
          <>
            {/* Theme name */}
            <div className="welcome-editor__section">
              <label className="welcome-editor__label">主题名称</label>
              <input
                className="welcome-editor__input"
                type="text"
                value={activeTheme.name}
                onChange={(e) => updateActiveTheme({ name: e.target.value })}
                placeholder="输入主题名称"
              />
            </div>

            {/* Interval */}
            <div className="welcome-editor__section">
              <label className="welcome-editor__label">
                切换间隔：<strong>{activeTheme.interval} 秒</strong>
              </label>
              <input
                className="welcome-editor__range"
                type="range"
                min={MIN_INTERVAL}
                max={MAX_INTERVAL}
                step={1}
                value={activeTheme.interval}
                onChange={(e) => updateActiveTheme({ interval: clampInterval(Number(e.target.value)) })}
              />
              <div className="welcome-editor__range-labels">
                <span>{MIN_INTERVAL} 秒</span>
                <span>{MAX_INTERVAL} 秒</span>
              </div>
            </div>

            {/* Images */}
            <div className="welcome-editor__section">
              <div className="welcome-editor__label-row">
                <label className="welcome-editor__label">
                  图片 <span className="welcome-editor__count">{activeTheme.images.length}</span>
                </label>
                <button className="welcome-editor__add-btn" onClick={handleAddImages}>
                  <PlusIcon size={14} />
                  添加图片
                </button>
                <input
                  ref={imageInputRef}
                  type="file"
                  accept="image/*"
                  multiple
                  style={{ display: "none" }}
                  onChange={handleImagePicked}
                />
              </div>

              {activeTheme.images.length === 0 ? (
                <div className="welcome-editor__empty">
                  还没有图片，点「添加图片」选择本机照片。
                </div>
              ) : (
                <div className="welcome-editor__image-grid">
                  {activeTheme.images.map((src, idx) => (
                    <div key={`${idx}-${src.slice(0, 40)}`} className="welcome-editor__thumb">
                      {isGradientDataUrl(src) ? (
                        <div className="welcome-editor__thumb-img" style={{ backgroundImage: src }} />
                      ) : (
                        <img className="welcome-editor__thumb-img" src={src} alt="" draggable={false} />
                      )}
                      <button
                        className="welcome-editor__thumb-remove"
                        onClick={() => handleRemoveImage(idx)}
                        aria-label={`移除第 ${idx + 1} 张图片`}
                      >
                        <CloseIcon size={12} />
                      </button>
                      <span className="welcome-editor__thumb-idx">{idx + 1}</span>
                    </div>
                  ))}
                </div>
              )}
            </div>

            {/* Music */}
            <div className="welcome-editor__section">
              <div className="welcome-editor__label-row">
                <label className="welcome-editor__label">
                  背景音乐 <span className="welcome-editor__count">{activeTheme.musicUrls.length}</span>
                </label>
                <button className="welcome-editor__add-btn" onClick={() => musicInputRef.current?.click()}>
                  <MusicNoteSvg size={14} />
                  添加音乐
                </button>
                <input
                  ref={musicInputRef}
                  type="file"
                  accept="audio/*"
                  multiple
                  style={{ display: "none" }}
                  onChange={handleMusicPick}
                />
              </div>

              {activeTheme.musicUrls.length === 0 ? (
                <div className="welcome-editor__empty">
                  没有背景音乐。可一次选多首，播放时按顺序循环。
                </div>
              ) : (
                <div className="welcome-editor__music-list">
                  {activeTheme.musicUrls.map((_, idx) => (
                    <div key={idx} className="welcome-editor__music-row">
                      <MusicNoteSvg size={15} />
                      <span className="welcome-editor__music-name">第 {idx + 1} 首</span>
                      <button
                        className="welcome-editor__icon-btn welcome-editor__icon-btn--danger"
                        onClick={() => handleRemoveMusic(idx)}
                        aria-label={`移除第 ${idx + 1} 首音乐`}
                      >
                        <TrashIcon size={14} />
                      </button>
                    </div>
                  ))}
                </div>
              )}
            </div>
          </>
        )}
      </div>
    </div>
  );
}

/* ------------------------------------------------------------------
   Main WelcomePanel component
------------------------------------------------------------------ */

export function WelcomePanel({
  themes,
  activeThemeId,
  onThemesChange,
  onActiveThemeChange,
  onClose,
  onNotify,
}: WelcomePanelProps) {
  const activeTheme = useMemo(
    () => themes.find((t) => t.id === activeThemeId) ?? themes[0] ?? DEFAULT_WELCOME_THEME,
    [themes, activeThemeId],
  );

  const slideCount = activeTheme.images.length;
  const [currentIndex, setCurrentIndex] = useState(0);
  const [playing, setPlaying] = useState(true);
  const [settingsOpen, setSettingsOpen] = useState(false);

  // Keep index in bounds when theme changes
  useEffect(() => {
    if (currentIndex >= slideCount) {
      setCurrentIndex(slideCount > 0 ? slideCount - 1 : 0);
    }
  }, [slideCount, currentIndex]);

  // Auto-advance timer
  useEffect(() => {
    if (!playing || slideCount <= 1) return;

    const ms = activeTheme.interval * 1000;
    const timer = setInterval(() => {
      setCurrentIndex((prev) => (prev + 1) % slideCount);
    }, ms);

    return () => clearInterval(timer);
  }, [playing, activeTheme.interval, slideCount]);

  // Music: one element, one track at a time. A lone track loops; a playlist
  // advances when a track ends and wraps around.
  const audioRef = useRef<HTMLAudioElement | null>(null);
  const playFailureShown = useRef(false);
  const [musicIndex, setMusicIndex] = useState(0);
  const playlist = activeTheme.musicUrls;
  const playingRef = useRef(playing);

  useEffect(() => {
    playingRef.current = playing;
  }, [playing]);

  useEffect(() => {
    setMusicIndex(0);
  }, [activeTheme.id]);

  useEffect(() => {
    if (musicIndex >= playlist.length) {
      setMusicIndex(0);
      return;
    }
    const src = playlist[musicIndex];
    if (!src) return;

    const audio = new Audio(src);
    // Looping a whole playlist would trap it on the first track.
    audio.loop = playlist.length <= 1;
    audio.volume = 0.5;
    audioRef.current = audio;
    const advance = () => setMusicIndex((prev) => (prev + 1) % playlist.length);
    audio.addEventListener("ended", advance);
    if (playingRef.current) {
      audio.play().catch(() => {
        // A blocked track used to fail in silence, which is how "音乐无法播放"
        // stayed invisible. Say it once, not on every track change.
        if (playFailureShown.current) return;
        playFailureShown.current = true;
        onNotify("背景音乐无法播放，请重新选择音频文件", "error");
      });
    }

    return () => {
      audio.removeEventListener("ended", advance);
      audio.pause();
      audio.src = "";
      if (audioRef.current === audio) audioRef.current = null;
    };
  }, [playlist, musicIndex, activeTheme.id, onNotify]);

  // Pause/resume audio with playing state
  useEffect(() => {
    if (!audioRef.current) return;
    if (playing) {
      audioRef.current.play().catch(() => {});
    } else {
      audioRef.current.pause();
    }
  }, [playing]);

  // Keyboard shortcuts
  useEffect(() => {
    const handleKey = (e: KeyboardEvent) => {
      if (settingsOpen) {
        if (e.key === "Escape") {
          e.stopPropagation();
          setSettingsOpen(false);
        }
        return;
      }

      switch (e.key) {
        case "Escape":
          e.stopPropagation();
          onClose();
          break;
        case "ArrowLeft":
          e.preventDefault();
          setCurrentIndex((prev) => (prev - 1 + slideCount) % slideCount);
          break;
        case "ArrowRight":
          e.preventDefault();
          setCurrentIndex((prev) => (prev + 1) % slideCount);
          break;
        case " ":
          e.preventDefault();
          setPlaying((p) => !p);
          break;
      }
    };

    window.addEventListener("keydown", handleKey, true);
    return () => window.removeEventListener("keydown", handleKey, true);
  }, [onClose, slideCount, settingsOpen]);

  const goTo = useCallback(
    (index: number) => {
      setCurrentIndex(((index % slideCount) + slideCount) % slideCount);
    },
    [slideCount],
  );

  const goPrev = useCallback(() => goTo(currentIndex - 1), [currentIndex, goTo]);
  const goNext = useCallback(() => goTo(currentIndex + 1), [currentIndex, goTo]);
  const togglePlay = useCallback(() => setPlaying((p) => !p), []);

  return createPortal(
    <div className="welcome-root">
      {/* Slideshow layer */}
      <div className="welcome-stage">
        {activeTheme.images.map((src, idx) => (
          <SlideImage key={`${activeTheme.id}-${idx}`} src={src} visible={idx === currentIndex} />
        ))}

        {/* Empty state */}
        {slideCount === 0 && (
          <div className="welcome-empty">
            <div className="welcome-empty__icon">
              <PlusIcon size={28} />
            </div>
            <p className="welcome-empty__text">这个主题还没有图片</p>
            <p className="welcome-empty__hint">点右上角的齿轮，添加本机图片</p>
          </div>
        )}
      </div>

      {/* Top-right controls */}
      <div className="welcome-topbar">
        <button
          className="welcome-topbar__btn"
          onClick={() => setSettingsOpen(true)}
          aria-label="主题设置"
          title="主题设置"
        >
          <GearSvg size={18} />
        </button>
        <button
          className="welcome-topbar__btn"
          onClick={onClose}
          aria-label="退出欢迎界面"
          title="退出"
        >
          <CloseIcon size={17} />
        </button>
      </div>

      {/* Bottom bar */}
      <div className="welcome-bottombar">
        <div className="welcome-bottombar__inner">
          {/* Left: theme info */}
          <div className="welcome-bottombar__info">
            <span className="welcome-bottombar__theme-name">{activeTheme.name}</span>
            {slideCount > 0 && (
              <span className="welcome-bottombar__counter">
                {currentIndex + 1} / {slideCount}
              </span>
            )}
          </div>

          {/* Center: transport controls */}
          <div className="welcome-bottombar__controls">
            <button
              className="welcome-transport-btn"
              onClick={goPrev}
              disabled={slideCount <= 1}
              aria-label="上一张"
              title="上一张"
            >
              <PrevSvg size={16} />
            </button>

            <button
              className="welcome-transport-btn welcome-transport-btn--play"
              onClick={togglePlay}
              disabled={slideCount <= 1}
              aria-label={playing ? "暂停" : "播放"}
              title={playing ? "暂停" : "播放"}
            >
              {playing ? <PauseSvg size={18} /> : <PlaySvg size={18} />}
            </button>

            <button
              className="welcome-transport-btn"
              onClick={goNext}
              disabled={slideCount <= 1}
              aria-label="下一张"
              title="下一张"
            >
              <NextSvg size={16} />
            </button>
          </div>

          {/* Right: theme switcher */}
          <div className="welcome-bottombar__right">
            {themes.length > 1 && (
              <div className="welcome-theme-tabs">
                {themes.map((t) => (
                  <button
                    key={t.id}
                    className={`welcome-theme-tab${t.id === activeTheme.id ? " is-active" : ""}`}
                    onClick={() => {
                      onActiveThemeChange(t.id);
                      setCurrentIndex(0);
                    }}
                  >
                    {t.name}
                  </button>
                ))}
              </div>
            )}
          </div>
        </div>

        {/* Progress dots */}
        {slideCount > 1 && slideCount <= 20 && (
          <div className="welcome-dots">
            {activeTheme.images.map((_, idx) => (
              <button
                key={idx}
                className={`welcome-dot${idx === currentIndex ? " is-active" : ""}`}
                onClick={() => goTo(idx)}
                aria-label={`跳到第 ${idx + 1} 张`}
              />
            ))}
          </div>
        )}
      </div>

      {/* Settings editor (slide-out) */}
      {settingsOpen && (
        <ThemeEditor
          themes={themes}
          activeThemeId={activeThemeId}
          onThemesChange={onThemesChange}
          onActiveThemeChange={onActiveThemeChange}
          onClose={() => setSettingsOpen(false)}
          onNotify={onNotify}
        />
      )}

      {/* Inline styles */}
      <WelcomeStyles />
    </div>,
    document.body,
  );
}

/* ------------------------------------------------------------------
   Scoped CSS injected via <style> tag
------------------------------------------------------------------ */

function WelcomeStyles(): ReactNode {
  return (
    <style>{`
/* ============================================================
   WelcomePanel - Cinematic slideshow
   All classes prefixed with "welcome-"
============================================================ */

.welcome-root {
  position: fixed;
  inset: 0;
  z-index: 500;
  font-family: var(--font-text, "Segoe UI Variable Text", "Segoe UI", "Microsoft YaHei UI", "PingFang SC", sans-serif);
  color: #edf4fc;
}

/* ---- Stage ---- */
.welcome-stage {
  position: absolute;
  inset: 0;
  background: #030509;
  overflow: hidden;
}

.welcome-empty {
  position: absolute;
  inset: 0;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 10px;
}

.welcome-empty__icon {
  display: grid;
  place-items: center;
  width: 64px;
  height: 64px;
  border-radius: 20px;
  border: 1px solid rgba(192, 220, 249, 0.12);
  background: rgba(255, 255, 255, 0.04);
  color: rgba(178, 205, 233, 0.4);
}

.welcome-empty__text {
  font-size: 16px;
  font-weight: 500;
  color: rgba(226, 237, 249, 0.6);
  margin: 0;
}

.welcome-empty__hint {
  font-size: 11px;
  color: rgba(178, 205, 233, 0.3);
  margin: 0;
}

/* ---- Top bar ---- */
.welcome-topbar {
  position: absolute;
  top: 0;
  right: 0;
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 18px 20px;
  z-index: 10;
  background: linear-gradient(180deg, rgba(3, 5, 9, 0.6) 0%, transparent 100%);
}

.welcome-topbar__btn {
  display: grid;
  place-items: center;
  width: 36px;
  height: 36px;
  border-radius: 10px;
  border: 1px solid rgba(192, 220, 249, 0.12);
  background: rgba(10, 18, 28, 0.65);
  backdrop-filter: blur(16px);
  -webkit-backdrop-filter: blur(16px);
  color: rgba(210, 228, 245, 0.7);
  cursor: pointer;
  transition: background 0.2s ease, border-color 0.2s ease, color 0.2s ease;
}

.welcome-topbar__btn:hover {
  background: rgba(255, 255, 255, 0.1);
  border-color: rgba(192, 220, 249, 0.22);
  color: #edf4fc;
}

/* ---- Bottom bar ---- */
.welcome-bottombar {
  position: absolute;
  bottom: 0;
  left: 0;
  right: 0;
  z-index: 10;
  padding: 0 24px 22px;
  background: linear-gradient(0deg, rgba(3, 5, 9, 0.75) 0%, rgba(3, 5, 9, 0.4) 60%, transparent 100%);
}

.welcome-bottombar__inner {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 16px;
  min-height: 44px;
}

.welcome-bottombar__info {
  display: flex;
  flex-direction: column;
  gap: 3px;
  min-width: 0;
  flex: 1;
}

.welcome-bottombar__theme-name {
  font-family: var(--font-display, "Segoe UI Variable Display", "Segoe UI", sans-serif);
  font-size: 15px;
  font-weight: 500;
  letter-spacing: 0.02em;
  color: rgba(240, 248, 255, 0.88);
  text-shadow: 0 1px 4px rgba(0, 0, 0, 0.6);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.welcome-bottombar__counter {
  font-family: "Cascadia Mono", "Consolas", monospace;
  font-size: 10px;
  letter-spacing: 0.06em;
  color: rgba(178, 205, 233, 0.45);
}

/* ---- Transport controls ---- */
.welcome-bottombar__controls {
  display: flex;
  align-items: center;
  gap: 6px;
  flex: none;
}

.welcome-transport-btn {
  display: grid;
  place-items: center;
  width: 38px;
  height: 38px;
  border-radius: 50%;
  border: 1px solid rgba(192, 220, 249, 0.12);
  background: rgba(10, 18, 28, 0.55);
  backdrop-filter: blur(14px);
  -webkit-backdrop-filter: blur(14px);
  color: rgba(210, 228, 245, 0.72);
  cursor: pointer;
  transition: background 0.2s ease, border-color 0.2s ease, color 0.2s ease, transform 0.15s ease;
}

.welcome-transport-btn:hover:not(:disabled) {
  background: rgba(255, 255, 255, 0.1);
  border-color: rgba(192, 220, 249, 0.24);
  color: #edf4fc;
}

.welcome-transport-btn:active:not(:disabled) {
  transform: scale(0.93);
}

.welcome-transport-btn:disabled {
  opacity: 0.3;
  cursor: default;
}

.welcome-transport-btn--play {
  width: 46px;
  height: 46px;
  border-color: rgba(117, 183, 255, 0.25);
  background: rgba(117, 183, 255, 0.1);
  color: rgba(160, 210, 255, 0.9);
}

.welcome-transport-btn--play:hover:not(:disabled) {
  background: rgba(117, 183, 255, 0.18);
  border-color: rgba(117, 183, 255, 0.4);
  color: #fff;
}

/* ---- Theme tabs ---- */
.welcome-bottombar__right {
  display: flex;
  align-items: center;
  justify-content: flex-end;
  flex: 1;
  min-width: 0;
}

.welcome-theme-tabs {
  display: flex;
  gap: 2px;
  padding: 3px;
  border: 1px solid rgba(184, 212, 240, 0.1);
  border-radius: 10px;
  background: rgba(255, 255, 255, 0.03);
  backdrop-filter: blur(12px);
  -webkit-backdrop-filter: blur(12px);
  max-width: 300px;
  overflow-x: auto;
}

.welcome-theme-tab {
  height: 28px;
  padding: 0 12px;
  border-radius: 7px;
  color: rgba(188, 209, 230, 0.45);
  font-size: 10.5px;
  letter-spacing: 0.03em;
  white-space: nowrap;
  cursor: pointer;
  transition: background 0.2s ease, color 0.2s ease;
  border: none;
  background: none;
}

.welcome-theme-tab:hover {
  color: rgba(225, 239, 252, 0.75);
}

.welcome-theme-tab.is-active {
  color: rgba(241, 248, 255, 0.9);
  background: rgba(255, 255, 255, 0.08);
  box-shadow: inset 0 1px 0 rgba(255, 255, 255, 0.06);
}

/* ---- Progress dots ---- */
.welcome-dots {
  display: flex;
  justify-content: center;
  gap: 6px;
  margin-top: 12px;
}

.welcome-dot {
  width: 6px;
  height: 6px;
  border-radius: 50%;
  border: none;
  background: rgba(192, 220, 249, 0.2);
  cursor: pointer;
  padding: 0;
  transition: background 0.25s ease, transform 0.2s ease;
}

.welcome-dot:hover {
  background: rgba(192, 220, 249, 0.45);
  transform: scale(1.3);
}

.welcome-dot.is-active {
  background: rgba(117, 183, 255, 0.8);
  transform: scale(1.2);
  box-shadow: 0 0 8px rgba(117, 183, 255, 0.4);
}

/* ============================================================
   Theme editor (slide-out panel)
============================================================ */

.welcome-editor-backdrop {
  position: fixed;
  inset: 0;
  z-index: 510;
  display: flex;
  justify-content: flex-end;
  background: rgba(2, 5, 10, 0.4);
  backdrop-filter: blur(6px);
  -webkit-backdrop-filter: blur(6px);
  animation: welcomeEditorFadeIn 0.25s ease both;
}

.welcome-editor {
  display: flex;
  flex-direction: column;
  width: min(420px, 92vw);
  height: 100%;
  border-left: 1px solid rgba(188, 218, 249, 0.1);
  background: linear-gradient(168deg, rgba(17, 27, 41, 0.97) 0%, rgba(8, 14, 23, 0.985) 52%, rgba(6, 11, 18, 0.99) 100%);
  box-shadow: -24px 0 90px rgba(0, 0, 0, 0.45);
  animation: welcomeEditorSlideIn 0.35s cubic-bezier(0.16, 1, 0.3, 1) both;
  overflow: hidden;
}

.welcome-editor__header {
  flex: none;
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 24px 22px 18px;
  border-bottom: 1px solid rgba(187, 216, 245, 0.08);
}

.welcome-editor__title {
  font-family: var(--font-display, "Segoe UI Variable Display", "Segoe UI", sans-serif);
  font-size: 20px;
  font-weight: 500;
  letter-spacing: 0.015em;
  color: rgba(243, 249, 255, 0.92);
  margin: 0;
}

.welcome-editor__close {
  display: grid;
  place-items: center;
  width: 32px;
  height: 32px;
  border-radius: 9px;
  border: 1px solid rgba(192, 220, 249, 0.1);
  background: rgba(255, 255, 255, 0.04);
  color: rgba(210, 228, 245, 0.6);
  cursor: pointer;
  transition: background 0.2s ease, color 0.2s ease;
}

.welcome-editor__close:hover {
  background: rgba(255, 255, 255, 0.08);
  color: #edf4fc;
}

/* Scrollable body */
.welcome-editor {
  overflow-y: auto;
}

.welcome-editor__section {
  padding: 18px 22px;
  border-bottom: 1px solid rgba(185, 212, 240, 0.06);
}

.welcome-editor__section:last-child {
  border-bottom: none;
}

.welcome-editor__label {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 10.5px;
  font-weight: 460;
  letter-spacing: 0.04em;
  color: rgba(154, 194, 235, 0.5);
  text-transform: uppercase;
  margin-bottom: 8px;
}

.welcome-editor__label strong {
  color: rgba(210, 228, 245, 0.75);
  font-weight: 500;
  text-transform: none;
}

.welcome-editor__count {
  display: inline-grid;
  place-items: center;
  min-width: 18px;
  height: 16px;
  padding: 0 5px;
  border-radius: 4px;
  background: rgba(255, 255, 255, 0.05);
  font-size: 8.5px;
  font-weight: 460;
  color: rgba(171, 202, 234, 0.5);
  text-transform: none;
}

.welcome-editor__label-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 10px;
  margin-bottom: 10px;
}

.welcome-editor__label-row .welcome-editor__label {
  margin-bottom: 0;
}

/* Theme selector row */
.welcome-editor__theme-row {
  display: flex;
  align-items: center;
  gap: 6px;
}

.welcome-editor__select {
  flex: 1;
  min-width: 0;
  height: 36px;
  padding: 0 10px;
  border: 1px solid rgba(181, 211, 242, 0.12);
  border-radius: 9px;
  background: rgba(255, 255, 255, 0.04);
  color: rgba(219, 233, 247, 0.78);
  font-size: 11.5px;
  font-family: inherit;
  cursor: pointer;
  appearance: none;
  -webkit-appearance: none;
  background-image: url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='12' height='12' viewBox='0 0 24 24' fill='none' stroke='rgba(180,210,240,0.4)' stroke-width='2' stroke-linecap='round'%3E%3Cpath d='m6 9 6 6 6-6'/%3E%3C/svg%3E");
  background-repeat: no-repeat;
  background-position: right 10px center;
  padding-right: 30px;
}

.welcome-editor__select:focus {
  border-color: rgba(108, 175, 251, 0.4);
  outline: none;
}

.welcome-editor__icon-btn {
  display: grid;
  place-items: center;
  width: 36px;
  height: 36px;
  border-radius: 9px;
  border: 1px solid rgba(192, 220, 249, 0.1);
  background: rgba(255, 255, 255, 0.04);
  color: rgba(210, 228, 245, 0.6);
  cursor: pointer;
  transition: background 0.2s ease, color 0.2s ease, border-color 0.2s ease;
}

.welcome-editor__icon-btn:hover {
  background: rgba(255, 255, 255, 0.08);
  color: #edf4fc;
  border-color: rgba(192, 220, 249, 0.2);
}

.welcome-editor__icon-btn--danger:hover {
  background: rgba(255, 100, 120, 0.1);
  color: rgba(255, 150, 163, 0.9);
  border-color: rgba(255, 128, 145, 0.25);
}

/* Text input */
.welcome-editor__input {
  width: 100%;
  height: 38px;
  padding: 0 12px;
  border: 1px solid rgba(181, 211, 242, 0.12);
  border-radius: 9px;
  background: rgba(255, 255, 255, 0.04);
  color: rgba(219, 233, 247, 0.82);
  font-size: 12px;
  font-family: inherit;
  transition: border-color 0.2s ease, background 0.2s ease;
}

.welcome-editor__input:focus {
  border-color: rgba(108, 175, 251, 0.4);
  background: rgba(255, 255, 255, 0.06);
  outline: none;
}

.welcome-editor__input::placeholder {
  color: rgba(170, 199, 227, 0.25);
}

/* Range slider */
.welcome-editor__range {
  width: 100%;
  height: 4px;
  border-radius: 2px;
  appearance: none;
  -webkit-appearance: none;
  background: rgba(181, 211, 242, 0.12);
  outline: none;
  cursor: pointer;
}

.welcome-editor__range::-webkit-slider-thumb {
  appearance: none;
  -webkit-appearance: none;
  width: 16px;
  height: 16px;
  border-radius: 50%;
  background: rgba(117, 183, 255, 0.85);
  border: 2px solid rgba(10, 18, 28, 0.8);
  box-shadow: 0 0 8px rgba(117, 183, 255, 0.3);
  cursor: pointer;
  transition: transform 0.15s ease;
}

.welcome-editor__range::-webkit-slider-thumb:hover {
  transform: scale(1.15);
}

.welcome-editor__range::-moz-range-thumb {
  width: 16px;
  height: 16px;
  border-radius: 50%;
  background: rgba(117, 183, 255, 0.85);
  border: 2px solid rgba(10, 18, 28, 0.8);
  box-shadow: 0 0 8px rgba(117, 183, 255, 0.3);
  cursor: pointer;
}

.welcome-editor__range-labels {
  display: flex;
  justify-content: space-between;
  margin-top: 4px;
  font-size: 9px;
  color: rgba(154, 194, 235, 0.3);
}

/* Add button */
.welcome-editor__add-btn {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  height: 32px;
  padding: 0 12px;
  border-radius: 8px;
  border: 1px solid rgba(117, 183, 255, 0.2);
  background: rgba(117, 183, 255, 0.08);
  color: rgba(160, 210, 255, 0.8);
  font-size: 10.5px;
  font-family: inherit;
  cursor: pointer;
  transition: background 0.2s ease, border-color 0.2s ease;
}

.welcome-editor__add-btn:hover {
  background: rgba(117, 183, 255, 0.14);
  border-color: rgba(117, 183, 255, 0.35);
}

/* Image grid */
.welcome-editor__image-grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(72px, 1fr));
  gap: 6px;
}

.welcome-editor__thumb {
  position: relative;
  aspect-ratio: 16 / 10;
  border-radius: 8px;
  border: 1px solid rgba(181, 211, 242, 0.08);
  overflow: hidden;
  background: rgba(4, 10, 17, 0.6);
}

.welcome-editor__thumb-img {
  width: 100%;
  height: 100%;
  object-fit: cover;
  display: block;
  background-size: cover;
  background-position: center;
}

.welcome-editor__thumb-remove {
  position: absolute;
  top: 3px;
  right: 3px;
  display: grid;
  place-items: center;
  width: 20px;
  height: 20px;
  border-radius: 5px;
  border: none;
  background: rgba(10, 18, 28, 0.75);
  color: rgba(255, 150, 163, 0.8);
  cursor: pointer;
  opacity: 0;
  transition: opacity 0.15s ease, background 0.15s ease;
}

.welcome-editor__thumb:hover .welcome-editor__thumb-remove {
  opacity: 1;
}

.welcome-editor__thumb-remove:hover {
  background: rgba(255, 80, 100, 0.3);
}

.welcome-editor__thumb-idx {
  position: absolute;
  bottom: 3px;
  left: 3px;
  display: grid;
  place-items: center;
  min-width: 16px;
  height: 16px;
  padding: 0 4px;
  border-radius: 4px;
  background: rgba(10, 18, 28, 0.7);
  font-size: 8px;
  font-family: "Cascadia Mono", "Consolas", monospace;
  color: rgba(178, 205, 233, 0.55);
}

/* Empty state in editor */
.welcome-editor__empty {
  padding: 20px;
  border: 1px dashed rgba(178, 208, 239, 0.1);
  border-radius: 10px;
  color: rgba(165, 194, 223, 0.3);
  font-size: 10.5px;
  text-align: center;
  line-height: 1.6;
}

/* Music row */
.welcome-editor__music-row {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 12px;
  border: 1px solid rgba(181, 211, 242, 0.1);
  border-radius: 9px;
  background: rgba(255, 255, 255, 0.03);
  color: rgba(178, 205, 233, 0.55);
  font-size: 11px;
}

.welcome-editor__music-name {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

/* ---- Animations ---- */
@keyframes welcomeEditorFadeIn {
  from { opacity: 0; }
  to { opacity: 1; }
}

@keyframes welcomeEditorSlideIn {
  from { transform: translateX(100%); }
  to { transform: translateX(0); }
}

/* ---- Reduced motion ---- */
@media (prefers-reduced-motion: reduce) {
  .welcome-editor-backdrop {
    animation: none;
  }
  .welcome-editor {
    animation: none;
  }
}
`}</style>
  );
}
