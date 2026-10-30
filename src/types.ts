export type BuiltinAction =
  | "whiteboard"
  | "projection"
  | "files"
  | "browser"
  | "notepad"
  | "screenshot"
  | "screenrec"
  | "annotation"
  | "office"
  | "filetransfer"
  | "welcome";

export type Accent = "cyan" | "blue" | "amber" | "violet" | "mint" | "rose" | "ice";

export interface AppEntry {
  id: string;
  name: string;
  builtin?: BuiltinAction;
  path?: string;
  args?: string;
  icon?: string | null;
  accent: Accent;
  pinned: boolean;
  addedAt: number;
}

/** One look of the 欢迎 app: slides that rotate, and the playlist under them.
    Media lives as data URLs, the same way custom wallpapers do. */
export interface WelcomeTheme {
  id: string;
  name: string;
  images: string[];
  /** Seconds each slide holds before the rotation moves on. */
  interval: number;
  /** One track loops; several play in order. */
  musicUrls: string[];
}

export interface LauncherSettings {
  deviceName: string;
  tagline: string;
  /** Off leaves the wallpaper's own branding unobstructed. */
  showScreenInfo: boolean;
  fullscreenOnStart: boolean;
  showSeconds: boolean;
  wallpaper: string;
  customWallpapers: string[];
  welcomeThemes: WelcomeTheme[];
  welcomeActiveTheme: string;
  sidebarEnabled: boolean;
  sidebarPosition: "left" | "right" | "custom";
  sidebarX: number;
  sidebarY: number;
  preferredOfficeApp?: string;
  preferredOfficePath?: string;
  preferredAnnotationApp?: string;
  preferredAnnotationPath?: string;
}

export interface LauncherState {
  version: 1;
  entries: AppEntry[];
  settings: LauncherSettings;
}

export interface ImportedApplication {
  name: string;
  path: string;
  icon?: string | null;
}

export interface SystemStatus {
  batteryPercent: number | null;
  charging: boolean;
}

export type WindowAction =
  | "minimize"
  | "close"
  | "toggleMaximize"
  | "toggleFullscreen"
  | "fullscreenOn"
  | "fullscreenOff";

export interface AirplayStatus {
  name: string;
  service: string;
  host: string;
  address: string;
  port: number;
  connected: boolean;
  packets: number;
  frames: number;
  seconds: number;
  missing: number;
  resendRequests: number;
  volumeDb: number;
  lastPeer: string | null;
  lastCodec: string | null;
  paired: boolean;
  fairplay: boolean;
  mirroring: boolean;
  mirrorFrames: number;
  mirrorKeyframes: number;
  mirrorBytes: number;
  mirrorWidth: number;
  mirrorHeight: number;
  /** 画面落后现实的时间（毫秒），按发送端采集时间戳算：均值与峰值 */
  mirrorLatencyMs: number;
  mirrorLatencyMaxMs: number;
  /** 以本场会话第一帧为基准的同一延迟，抵消两台机器之间的时钟差 */
  mirrorDriftMs: number;
  mirrorDriftMaxMs: number;
  /** 相邻两帧到达之间最长的一次间隔（毫秒），即流本身可见的卡顿 */
  mirrorGapMaxMs: number;
  /** 相邻两帧到达的平均间隔（毫秒），也就是发送端真实的帧周期 */
  mirrorGapAvgMs: number;
  /** 本机每帧从到达到交出的平均耗时（毫秒） */
  mirrorPipelineMs: number;
  /** 转发队列溢出时丢掉的帧数，每一次都是参考链上的一个洞 */
  mirrorDrops: number;
  /** 预览自己上报的接收侧指标，尚未有预览时为空 */
  preview: MirrorPerf | null;
}

export interface MirrorPerf {
  painted: number;
  skipped: number;
  queueMax: number;
  inFlightAvgMs: number;
  inFlightMaxMs: number;
  fps: number;
}

export interface DlnaStatus {
  name: string;
  location: string;
  port: number;
  phase: "idle" | "loading" | "playing" | "paused" | "stopped" | "ended" | "error";
  position: number;
  duration: number;
  volume: number;
  uri: string | null;
  error: string | null;
}

export interface BrowserStatus {
  name: string;
  listening: boolean;
  port: number;
  address: string;
  /** The six digits shown on the big screen as the 投屏码. */
  code: string;
  url: string;
  connected: boolean;
  peer: string | null;
  frames: number;
  keyframes: number;
  videoBytes: number;
  audioBytes: number;
  width: number;
  height: number;
}

export interface ReceiverStatus {
  airplay: AirplayStatus | null;
  dlna: DlnaStatus | null;
  browser: BrowserStatus | null;
}

/** What `miracast_status` reports about the operating system's own receiver. */
export interface MiracastStatus {
  /** `C:\Windows\System32\cast.exe` when 无线显示器 is installed. */
  launcher: string | null;
}

export interface MiracastLaunch {
  /** `"launched"` when the system app took over, `"settings"` when it is missing. */
  mode: "launched" | "settings";
}

export interface LanPeer {
  id: string;
  name: string;
  model?: string | null;
  protocol: "HORIZON" | "Cast" | "AirPlay" | "DLNA";
  address: string | null;
  location: string | null;
}

export type CastSenderPhase =
  | "idle"
  | "connecting"
  | "launching"
  | "loading"
  | "playing"
  | "paused"
  | "buffering"
  | "stopped"
  | "ended"
  | "error";

export interface CastSenderStatus {
  device: string;
  host: string;
  url: string;
  contentType: string;
  phase: CastSenderPhase;
  position: number;
  duration: number;
  volume: number | null;
  observedAt: number;
  error: string | null;
}

export const MAX_PINNED = 6;
