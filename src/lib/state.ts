import type {
  Accent,
  AppEntry,
  BuiltinAction,
  LauncherSettings,
  LauncherState,
  WelcomeTheme,
} from "../types";
import { MAX_PINNED } from "../types";

export const STORAGE_KEY = "horizon.launcher.state.v1";

export const ACCENTS: Accent[] = ["cyan", "blue", "violet", "amber", "mint", "rose", "ice"];

export const BUILTIN_NAMES: Record<BuiltinAction, string> = {
  whiteboard: "智能白板",
  projection: "无线投屏",
  files: "文件空间",
  browser: "网页浏览",
  notepad: "记事本",
  screenshot: "截屏",
  screenrec: "录屏",
  annotation: "批注",
  office: "Office",
  filetransfer: "文件传送",
  welcome: "欢迎",
};

export const WELCOME_MIN_INTERVAL = 5;
export const WELCOME_MAX_INTERVAL = 60;

/** The look a fresh 欢迎 app shows: three gradients, no music. It is real state
    rather than a component fallback, because a fallback can never be edited. */
export const DEFAULT_WELCOME_THEME: WelcomeTheme = {
  id: "default",
  name: "欢迎",
  images: [
    "linear-gradient(135deg, #0f2027 0%, #203a43 40%, #2c5364 100%)",
    "linear-gradient(135deg, #1a1a2e 0%, #16213e 45%, #0f3460 100%)",
    "linear-gradient(135deg, #0d1b2a 0%, #1b2838 40%, #2d4059 100%)",
  ],
  interval: 10,
  musicUrls: [],
};

function normalizeWelcomeTheme(value: unknown, index: number): WelcomeTheme | null {
  if (!value || typeof value !== "object") return null;
  const theme = value as Partial<WelcomeTheme> & { musicUrl?: unknown };
  // The single-slot `musicUrl` shipped before playlists did.
  const music = Array.isArray(theme.musicUrls)
    ? theme.musicUrls.filter((item) => typeof item === "string")
    : typeof theme.musicUrl === "string"
      ? [theme.musicUrl]
      : [];
  const rawInterval = Number(theme.interval);
  const interval = Number.isFinite(rawInterval)
    ? Math.max(WELCOME_MIN_INTERVAL, Math.min(WELCOME_MAX_INTERVAL, Math.round(rawInterval)))
    : DEFAULT_WELCOME_THEME.interval;
  return {
    id: typeof theme.id === "string" && theme.id ? theme.id : `theme-${index}`,
    name: cleanText(theme.name, `主题 ${index + 1}`, 30),
    images: Array.isArray(theme.images)
      ? theme.images.filter((item) => typeof item === "string")
      : [],
    interval,
    musicUrls: music,
  };
}

export const DEFAULT_SETTINGS: LauncherSettings = {
  deviceName: "HORIZON 86",
  tagline: "让每一次协作，都清晰发生",
  showScreenInfo: true,
  fullscreenOnStart: false,
  showSeconds: false,
  wallpaper: "nebula",
  customWallpapers: [],
  welcomeThemes: [DEFAULT_WELCOME_THEME],
  welcomeActiveTheme: DEFAULT_WELCOME_THEME.id,
  sidebarEnabled: true,
  sidebarPosition: "right",
  sidebarX: 0,
  sidebarY: 0,
  preferredOfficeApp: undefined,
  preferredOfficePath: undefined,
  preferredAnnotationApp: undefined,
  preferredAnnotationPath: undefined,
};

export const SIDEBAR_BUILTINS: BuiltinAction[] = [
  "notepad",
  "screenshot",
  "screenrec",
  "annotation",
  "office",
  "filetransfer",
  "welcome",
];

export function defaultEntries(): AppEntry[] {
  const seededAt = 1_736_000_000_000;
  return [
    {
      id: "system.whiteboard",
      name: BUILTIN_NAMES.whiteboard,
      builtin: "whiteboard",
      accent: "cyan",
      pinned: true,
      addedAt: seededAt,
    },
    {
      id: "system.projection",
      name: BUILTIN_NAMES.projection,
      builtin: "projection",
      accent: "blue",
      pinned: true,
      addedAt: seededAt + 1,
    },
    {
      id: "system.files",
      name: BUILTIN_NAMES.files,
      builtin: "files",
      accent: "amber",
      pinned: true,
      addedAt: seededAt + 2,
    },
    {
      id: "system.browser",
      name: BUILTIN_NAMES.browser,
      builtin: "browser",
      accent: "violet",
      pinned: true,
      addedAt: seededAt + 3,
    },
    {
      id: "system.notepad",
      name: BUILTIN_NAMES.notepad,
      builtin: "notepad",
      accent: "mint",
      pinned: false,
      addedAt: seededAt + 4,
    },
    {
      id: "system.screenshot",
      name: BUILTIN_NAMES.screenshot,
      builtin: "screenshot",
      accent: "rose",
      pinned: false,
      addedAt: seededAt + 5,
    },
    {
      id: "system.screenrec",
      name: BUILTIN_NAMES.screenrec,
      builtin: "screenrec",
      accent: "ice",
      pinned: false,
      addedAt: seededAt + 6,
    },
    {
      id: "system.annotation",
      name: BUILTIN_NAMES.annotation,
      builtin: "annotation",
      accent: "cyan",
      pinned: false,
      addedAt: seededAt + 7,
    },
    {
      id: "system.office",
      name: BUILTIN_NAMES.office,
      builtin: "office",
      accent: "blue",
      pinned: false,
      addedAt: seededAt + 8,
    },
    {
      id: "system.filetransfer",
      name: BUILTIN_NAMES.filetransfer,
      builtin: "filetransfer",
      accent: "amber",
      pinned: false,
      addedAt: seededAt + 9,
    },
    {
      id: "system.welcome",
      name: BUILTIN_NAMES.welcome,
      builtin: "welcome",
      accent: "violet",
      pinned: false,
      addedAt: seededAt + 10,
    },
  ];
}

export function defaultState(): LauncherState {
  return {
    version: 1,
    entries: defaultEntries(),
    settings: { ...DEFAULT_SETTINGS },
  };
}

export function createId(): string {
  return `app.${Date.now().toString(36)}.${Math.random().toString(36).slice(2, 8)}`;
}

export function nextAccent(entries: AppEntry[]): Accent {
  const counts = new Map<Accent, number>();
  for (const entry of entries) {
    counts.set(entry.accent, (counts.get(entry.accent) ?? 0) + 1);
  }
  return [...ACCENTS].sort((a, b) => (counts.get(a) ?? 0) - (counts.get(b) ?? 0))[0];
}

export function normalizeState(value: unknown): LauncherState {
  if (!value || typeof value !== "object") return defaultState();
  const candidate = value as Partial<LauncherState>;
  const fallback = defaultState();
  const entries = Array.isArray(candidate.entries)
    ? candidate.entries.filter(isAppEntry).map((entry) => ({ ...entry }))
    : fallback.entries;
  const settings = { ...DEFAULT_SETTINGS, ...(candidate.settings ?? {}) };

  const existingBuiltins = new Set(
    entries.filter((e) => e.builtin).map((e) => e.builtin),
  );
  const defaults = fallback.entries;
  for (const def of defaults) {
    if (def.builtin && !existingBuiltins.has(def.builtin)) {
      entries.push({ ...def });
    }
  }

  let pinned = 0;
  const normalizedEntries = entries.map((entry) => {
    if (!entry.pinned) return entry;
    pinned += 1;
    return pinned > MAX_PINNED ? { ...entry, pinned: false } : entry;
  });

  const welcomeThemes = Array.isArray(settings.welcomeThemes)
    ? settings.welcomeThemes
        .map((theme, index) => normalizeWelcomeTheme(theme, index))
        .filter((theme): theme is WelcomeTheme => theme !== null)
    : [];
  // A state file from before this feature, or one whose themes were all invalid,
  // still has to open on something.
  const themes = welcomeThemes.length ? welcomeThemes : [DEFAULT_WELCOME_THEME];
  const welcomeActiveTheme = themes.some((theme) => theme.id === settings.welcomeActiveTheme)
    ? settings.welcomeActiveTheme
    : themes[0].id;

  return {
    version: 1,
    entries: normalizedEntries,
    settings: {
      deviceName: cleanText(settings.deviceName, DEFAULT_SETTINGS.deviceName, 30),
      tagline: cleanText(settings.tagline, DEFAULT_SETTINGS.tagline, 60),
      showScreenInfo: Boolean(settings.showScreenInfo ?? true),
      fullscreenOnStart: Boolean(settings.fullscreenOnStart),
      showSeconds: Boolean(settings.showSeconds),
      wallpaper: cleanText(settings.wallpaper, DEFAULT_SETTINGS.wallpaper, 30),
      customWallpapers: Array.isArray(settings.customWallpapers)
        ? settings.customWallpapers.filter((w) => typeof w === "string")
        : [],
      welcomeThemes: themes,
      welcomeActiveTheme,
      sidebarEnabled: Boolean(settings.sidebarEnabled ?? true),
      sidebarPosition:
        settings.sidebarPosition === "left" ||
        settings.sidebarPosition === "right" ||
        settings.sidebarPosition === "custom"
          ? settings.sidebarPosition
          : "right",
      sidebarX: typeof settings.sidebarX === "number" ? settings.sidebarX : 0,
      sidebarY: typeof settings.sidebarY === "number" ? settings.sidebarY : 0,
      preferredOfficeApp:
        typeof settings.preferredOfficeApp === "string" ? settings.preferredOfficeApp : undefined,
      preferredOfficePath:
        typeof settings.preferredOfficePath === "string" ? settings.preferredOfficePath : undefined,
      preferredAnnotationApp:
        typeof settings.preferredAnnotationApp === "string"
          ? settings.preferredAnnotationApp
          : undefined,
      preferredAnnotationPath:
        typeof settings.preferredAnnotationPath === "string"
          ? settings.preferredAnnotationPath
          : undefined,
    },
  };
}

function isAppEntry(value: unknown): value is AppEntry {
  if (!value || typeof value !== "object") return false;
  const entry = value as Partial<AppEntry>;
  return (
    typeof entry.id === "string" &&
    typeof entry.name === "string" &&
    typeof entry.pinned === "boolean" &&
    typeof entry.accent === "string" &&
    (typeof entry.builtin === "string" || typeof entry.path === "string")
  );
}

function cleanText(value: unknown, fallback: string, maxLength: number): string {
  if (typeof value !== "string") return fallback;
  return value.trim().slice(0, maxLength) || fallback;
}
