import { Channel, invoke } from "@tauri-apps/api/core";
import type {
  AirplayStatus,
  BrowserStatus,
  BuiltinAction,
  CastSenderStatus,
  DlnaStatus,
  ImportedApplication,
  LanPeer,
  LauncherState,
  MiracastLaunch,
  MiracastStatus,
  MirrorPerf,
  ReceiverStatus,
  SystemStatus,
  WindowAction,
} from "../types";
import { STORAGE_KEY } from "./state";

export const isTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

/**
 * Welcome themes and custom wallpapers carry megabytes of `data:` URLs. Keeping
 * them in the settings file means every unrelated change — a sidebar drag, a
 * device name — re-serialises the whole blob on the main thread, which measured
 * 253–265 ms of dropped frames at 16.7 MB. So the payloads are parked beside the
 * settings file and the state keeps a `media:<key>` reference instead.
 */
const MEDIA_PREFIX = "media:";

/** Fields whose items are opaque strings that may carry a `data:` payload. */
const MEDIA_FIELDS = ["images", "musicUrls", "customWallpapers"] as const;

function isInlineData(value: unknown): value is string {
  return typeof value === "string" && value.startsWith("data:");
}

function parkList(media: Record<string, string>, items: string[], prefix: string): string[] {
  return items.map((item, index) => {
    if (!isInlineData(item)) return item;
    const key = `${prefix}${index}`;
    media[key] = item;
    return `${MEDIA_PREFIX}${key}`;
  });
}

function extractMedia(state: LauncherState, media: Record<string, string>): LauncherState {
  const themes = state.settings.welcomeThemes.map((theme) => ({
    ...theme,
    images: parkList(media, theme.images, `${theme.id}.i`),
    musicUrls: parkList(media, theme.musicUrls, `${theme.id}.m`),
  }));
  return {
    ...state,
    settings: {
      ...state.settings,
      welcomeThemes: themes,
      customWallpapers: parkList(media, state.settings.customWallpapers, "wallpaper.w"),
    },
  };
}

/** A reference with no media behind it is dropped rather than rendered broken. */
function resolveMedia(value: unknown, media: Record<string, string>): string | null {
  if (typeof value !== "string") return null;
  if (!value.startsWith(MEDIA_PREFIX)) return value;
  return media[value.slice(MEDIA_PREFIX.length)] ?? null;
}

function restoreLists(value: unknown, media: Record<string, string>): unknown {
  if (Array.isArray(value)) {
    return value.map((item) => resolveMedia(item, media)).filter((item): item is string => item !== null);
  }
  if (!value || typeof value !== "object") return value;
  const holder = value as Record<string, unknown>;
  const restored: Record<string, unknown> = { ...holder };
  for (const field of MEDIA_FIELDS) {
    if (field in restored) restored[field] = restoreLists(restored[field], media);
  }
  if (Array.isArray(restored.welcomeThemes)) {
    restored.welcomeThemes = restored.welcomeThemes.map((theme) => restoreLists(theme, media));
  }
  return restored;
}

function inlineMedia(stored: unknown, media: Record<string, string>): unknown {
  if (!stored || typeof stored !== "object") return stored;
  const candidate = stored as { settings?: unknown };
  if (!candidate.settings || typeof candidate.settings !== "object") return stored;
  return { ...candidate, settings: restoreLists(candidate.settings, media) };
}

let mediaSignature = "";
let saveChain: Promise<void> = Promise.resolve();

export async function loadState(): Promise<LauncherState | null> {
  if (!isTauri) {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return null;
    try {
      return JSON.parse(raw) as LauncherState;
    } catch {
      return null;
    }
  }

  const [raw, mediaRaw] = await Promise.all([
    invoke<string | null>("load_state"),
    invoke<string | null>("load_welcome_media"),
  ]);
  if (!raw) return null;
  let media: Record<string, string> = {};
  try {
    const parsedMedia = mediaRaw ? JSON.parse(mediaRaw) : {};
    if (parsedMedia && typeof parsedMedia === "object") {
      media = parsedMedia as Record<string, string>;
    }
  } catch {
    media = {};
  }
  mediaSignature = JSON.stringify(media);
  try {
    return inlineMedia(JSON.parse(raw), media) as LauncherState;
  } catch {
    return null;
  }
}

export function saveState(state: LauncherState): Promise<void> {
  // Saves are chained: the media file and the settings file have to land in that
  // order, and two overlapping saves would otherwise interleave them.
  const task = async () => {
    if (!isTauri) {
      localStorage.setItem(STORAGE_KEY, JSON.stringify(state));
      return;
    }
    const media: Record<string, string> = {};
    const slim = extractMedia(state, media);
    const mediaRaw = JSON.stringify(media);
    if (mediaRaw !== mediaSignature) {
      await invoke("save_welcome_media", { payload: mediaRaw });
      mediaSignature = mediaRaw;
    }
    await invoke("save_state", { state: JSON.stringify(slim) });
  };
  saveChain = saveChain.then(task, task);
  return saveChain;
}

export async function pickApplications(): Promise<ImportedApplication[]> {
  if (!isTauri) {
    throw new Error("添加本机应用需要在 HORIZON 桌面版中操作");
  }
  return invoke<ImportedApplication[]>("pick_applications");
}

export async function pickIcon(): Promise<string | null> {
  if (!isTauri) return null;
  return invoke<string | null>("pick_icon");
}

export async function launchPath(path: string, args?: string): Promise<void> {
  if (!isTauri) {
    throw new Error("启动本机应用需要在 HORIZON 桌面版中操作");
  }
  await invoke("launch_path", { path, args: args || null });
}

export async function openBuiltin(action: Exclude<BuiltinAction, "whiteboard" | "annotation" | "filetransfer" | "welcome">): Promise<string | null> {
  if (!isTauri) {
    if (action === "browser") {
      window.open("https://www.bing.com", "_blank", "noopener,noreferrer");
      return null;
    }
    if (action === "projection") {
      window.open("ms-settings:projections", "_blank");
      return null;
    }
    throw new Error("该功能需要在 HORIZON 桌面版中运行");
  }
  return invoke<string | null>("open_builtin", { action });
}

export async function getSystemStatus(): Promise<SystemStatus> {
  if (isTauri) {
    return invoke<SystemStatus>("get_system_status");
  }

  try {
    const nav = navigator as Navigator & {
      getBattery?: () => Promise<{ level: number; charging: boolean }>;
    };
    const battery = await nav.getBattery?.();
    if (!battery) return { batteryPercent: null, charging: false };
    return {
      batteryPercent: Math.round(battery.level * 100),
      charging: battery.charging,
    };
  } catch {
    return { batteryPercent: null, charging: false };
  }
}

export async function windowControl(action: WindowAction | "startDrag"): Promise<boolean> {
  if (!isTauri) {
    if (action === "toggleFullscreen" || action === "fullscreenOn" || action === "fullscreenOff") {
      if (document.fullscreenElement) {
        await document.exitFullscreen();
        return false;
      }
      if (action !== "fullscreenOff") {
        await document.documentElement.requestFullscreen();
        return true;
      }
    }
    return false;
  }
  return invoke<boolean>("window_control", { action });
}

export async function saveWhiteboard(dataUrl: string, suggestedName: string): Promise<string | null> {
  if (!isTauri) {
    const link = document.createElement("a");
    link.href = dataUrl;
    link.download = suggestedName;
    link.click();
    return null;
  }
  return invoke<string | null>("save_whiteboard", { dataUrl, suggestedName });
}

const DESKTOP_ONLY = "投屏接收需要在 HORIZON 桌面版中运行";

export async function startAirplay(name: string): Promise<AirplayStatus> {
  if (!isTauri) throw new Error(DESKTOP_ONLY);
  return invoke<AirplayStatus>("airplay_start", { name });
}

export async function stopAirplay(): Promise<void> {
  if (!isTauri) return;
  await invoke("airplay_stop");
}

/** True when the webview can decode the mirror stream itself. */
export const supportsAirplayPreview =
  typeof window !== "undefined" && "VideoDecoder" in window;

/**
 * Attaches a decoder channel to the running receiver. Each message is one
 * frame: 4 bytes of kind/flags, a 64-bit microsecond PTS, two 16-bit
 * dimensions, a 32-bit payload length, then the H.264 Annex B bytes.
 *
 * `owner` identifies the preview instance: a detach from an instance that has
 * already been replaced must not tear down the live channel.
 */
export async function attachAirplayPreview(
  owner: string,
  channel: Channel<ArrayBuffer>,
): Promise<void> {
  if (!isTauri) throw new Error(DESKTOP_ONLY);
  await invoke("airplay_preview", { owner, channel });
}

export async function detachAirplayPreview(owner: string): Promise<void> {
  if (!isTauri) return;
  try {
    await invoke("airplay_preview_stop", { owner });
  } catch {
    // The receiver may already be gone, which is the same end state.
  }
}

/**
 * Hands the preview's own measurements to the shell so 投屏 panel can show them.
 * The engine sees a frame arrive; only the webview knows how long that frame
 * waited in a decoder queue before it reached the canvas.
 */
export async function reportMirrorPerf(perf: MirrorPerf): Promise<void> {
  if (!isTauri) return;
  try {
    await invoke("mirror_perf_report", { perf });
  } catch {
    // A preview that cannot report still plays; metrics are not worth an error.
  }
}

export async function startDlna(name: string): Promise<DlnaStatus> {
  if (!isTauri) throw new Error(DESKTOP_ONLY);
  return invoke<DlnaStatus>("dlna_start", { name });
}

export async function stopDlna(): Promise<void> {
  if (!isTauri) return;
  await invoke("dlna_stop");
}

export async function miracastStatus(): Promise<MiracastStatus> {
  if (!isTauri) return { launcher: null };
  return invoke<MiracastStatus>("miracast_status");
}

/** Launches the system receiver, or opens 投影到此电脑 when it is not installed. */
export async function miracastLaunch(): Promise<MiracastLaunch> {
  if (!isTauri) throw new Error(DESKTOP_ONLY);
  return invoke<MiracastLaunch>("miracast_launch");
}

export async function startBrowser(name: string): Promise<BrowserStatus> {
  if (!isTauri) throw new Error(DESKTOP_ONLY);
  return invoke<BrowserStatus>("browser_start", { name });
}

export async function stopBrowser(): Promise<void> {
  if (!isTauri) return;
  await invoke("browser_stop");
}

/** Same frame channel as AirPlay, but fed by the browser extension. */
export async function attachBrowserPreview(
  owner: string,
  channel: Channel<ArrayBuffer>,
): Promise<void> {
  if (!isTauri) throw new Error(DESKTOP_ONLY);
  await invoke("browser_preview", { owner, channel });
}

export async function detachBrowserPreview(owner: string): Promise<void> {
  if (!isTauri) return;
  try {
    await invoke("browser_preview_stop", { owner });
  } catch {
    // The receiver may already be gone, which is the same end state.
  }
}

/**
 * Asks the connected sender for an IDR right away. A preview that fell behind
 * uses this to resynchronise instead of decoding a growing backlog.
 */
export async function requestBrowserKeyframe(): Promise<void> {
  if (!isTauri) return;
  try {
    await invoke("browser_keyframe");
  } catch {
    // No sender is connected; the next periodic keyframe will land anyway.
  }
}

export async function openExtensionFolder(): Promise<string> {
  if (!isTauri) throw new Error(DESKTOP_ONLY);
  return invoke<string>("open_extension_folder");
}

export async function startSnapdropServer(port: number): Promise<string> {
  if (!isTauri) throw new Error(DESKTOP_ONLY);
  return invoke<string>("start_snapdrop_server", { port });
}

export async function setSnapdropDeviceName(name: string): Promise<void> {
  if (!isTauri) throw new Error(DESKTOP_ONLY);
  await invoke("set_snapdrop_device_name", { name });
}

export async function setSnapdropStorageDir(path: string): Promise<void> {
  if (!isTauri) throw new Error(DESKTOP_ONLY);
  await invoke("set_snapdrop_storage_dir", { path });
}

export interface SnapdropConfig {
  deviceName: string;
  storageDir: string;
}

export async function getSnapdropConfig(): Promise<SnapdropConfig> {
  if (!isTauri) {
    return { deviceName: "HORIZON-Device", storageDir: "" };
  }
  return invoke<SnapdropConfig>("get_snapdrop_config");
}

/** Whether the firewall lets other devices reach the SnapDrop port. */
export async function snapdropFirewallOk(): Promise<boolean> {
  if (!isTauri) return true;
  return invoke<boolean>("snapdrop_firewall_ok");
}

/** Raises one UAC prompt. Resolves to whether the rule is in place afterwards. */
export async function allowSnapdropFirewall(): Promise<boolean> {
  if (!isTauri) throw new Error(DESKTOP_ONLY);
  return invoke<boolean>("allow_snapdrop_firewall");
}

/** The Run key is the source of truth, so this reads the registry, not state. */
export async function autostartStatus(): Promise<boolean> {
  if (!isTauri) return false;
  return invoke<boolean>("autostart_status");
}

export async function setAutostart(enabled: boolean): Promise<boolean> {
  if (!isTauri) throw new Error(DESKTOP_ONLY);
  return invoke<boolean>("set_autostart", { enabled });
}

export async function generateSnapdropQr(): Promise<string> {
  if (!isTauri) throw new Error(DESKTOP_ONLY);
  return invoke<string>("generate_snapdrop_qr");
}

export async function pickSnapdropStorageFolder(): Promise<string | null> {
  if (!isTauri) throw new Error(DESKTOP_ONLY);
  return invoke<string | null>("pick_snapdrop_storage_folder");
}

export async function detectOfficeApps(): Promise<[string, string][]> {
  if (!isTauri) throw new Error(DESKTOP_ONLY);
  return invoke<[string, string][]>("detect_office_apps");
}

export async function detectAnnotationApps(): Promise<[string, string][]> {
  if (!isTauri) throw new Error(DESKTOP_ONLY);
  return invoke<[string, string][]>("detect_annotation_apps");
}

export async function createSidebarWindow(): Promise<void> {
  if (!isTauri) throw new Error(DESKTOP_ONLY);
  return invoke("create_sidebar_window");
}

export async function createAnnotationWindow(): Promise<void> {
  if (!isTauri) throw new Error(DESKTOP_ONLY);
  return invoke("create_annotation_window");
}

export async function closeSidebarWindow(): Promise<void> {
  if (!isTauri) throw new Error(DESKTOP_ONLY);
  return invoke("close_sidebar_window");
}

export async function closeAnnotationWindow(): Promise<void> {
  if (!isTauri) throw new Error(DESKTOP_ONLY);
  return invoke("close_annotation_window");
}

export async function receiverStatus(): Promise<ReceiverStatus> {
  if (!isTauri) return { airplay: null, dlna: null, browser: null };
  return invoke<ReceiverStatus>("receiver_status");
}

export async function discoverPeers(seconds = 3): Promise<LanPeer[]> {
  if (!isTauri) return [];
  return invoke<LanPeer[]>("discover_peers", { seconds });
}

export async function castSend(
  device: string,
  host: string,
  url: string,
): Promise<CastSenderStatus> {
  if (!isTauri) throw new Error(DESKTOP_ONLY);
  return invoke<CastSenderStatus>("cast_send", { device, host, url });
}

export async function castControl(
  action: "play" | "pause" | "stop",
): Promise<CastSenderStatus> {
  if (!isTauri) throw new Error(DESKTOP_ONLY);
  return invoke<CastSenderStatus>("cast_control", { action });
}

export async function castStatus(): Promise<CastSenderStatus | null> {
  if (!isTauri) return null;
  return invoke<CastSenderStatus | null>("cast_status");
}
