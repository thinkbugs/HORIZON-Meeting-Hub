import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { AllApps } from "./components/AllApps";
import { AmbientBackground } from "./components/AmbientBackground";
import { AppEditor } from "./components/AppEditor";
import { CastPanel } from "./components/CastPanel";
import { DlnaStage } from "./components/DlnaStage";
import { ConfirmDialog } from "./components/Overlay";
import { Dock } from "./components/Dock";
import { FileTransferPanel } from "./components/FileTransferPanel";
import { Hero } from "./components/Hero";
import { OfficePanel } from "./components/OfficePanel";
import { Settings } from "./components/Settings";
import { Sidebar } from "./components/Sidebar";
import { Toast, type ToastMessage } from "./components/Toast";
import { TopBar } from "./components/TopBar";
import { WelcomePanel } from "./components/WelcomePanel";
import { Whiteboard } from "./components/Whiteboard";
import {
  getSystemStatus,
  launchPath,
  loadState,
  openBuiltin,
  pickApplications,
  receiverStatus,
  saveState,
  startSnapdropServer,
  windowControl,
} from "./lib/bridge";
import {
  createId,
  defaultState,
  nextAccent,
  normalizeState,
  SIDEBAR_BUILTINS,
} from "./lib/state";
import { suppressNativeContextMenu } from "./lib/contextMenu";
import { isTypingTarget } from "./lib/dom";
import {
  MAX_PINNED,
  type AppEntry,
  type BuiltinAction,
  type ImportedApplication,
  type LauncherSettings,
  type LauncherState,
  type ReceiverStatus,
  type SystemStatus,
  type WindowAction,
} from "./types";

interface EditingState {
  entry: AppEntry;
  isNew: boolean;
}

export default function App() {
  const [state, setState] = useState<LauncherState>(defaultState);
  const [hydrated, setHydrated] = useState(false);
  const [focusIndex, setFocusIndex] = useState(0);
  const [allAppsOpen, setAllAppsOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [castOpen, setCastOpen] = useState(false);
  const [castStage, setCastStage] = useState<"airplay" | "browser" | null>(null);
  const [receiver, setReceiver] = useState<ReceiverStatus | null>(null);
  const [whiteboardOpen, setWhiteboardOpen] = useState(false);
  const [fileTransferOpen, setFileTransferOpen] = useState(false);
  const [welcomeOpen, setWelcomeOpen] = useState(false);
  const [officeOpen, setOfficeOpen] = useState(false);
  const [dlnaStageOpen, setDlnaStageOpen] = useState(false);
  const [editing, setEditing] = useState<EditingState | null>(null);
  const [pendingDelete, setPendingDelete] = useState<AppEntry | null>(null);
  const [pendingReset, setPendingReset] = useState(false);
  const [importing, setImporting] = useState(false);
  const [toast, setToast] = useState<ToastMessage | null>(null);
  const [systemStatus, setSystemStatus] = useState<SystemStatus>({
    batteryPercent: null,
    charging: false,
  });
  const [online, setOnline] = useState(() => navigator.onLine);
  const [isFullscreen, setIsFullscreen] = useState(false);
  const toastTimer = useRef<number | null>(null);
  const saveErrorShown = useRef(false);
  const castLive = useRef<Record<"airplay" | "browser", boolean | null>>({
    airplay: null,
    browser: null,
  });
  const castArmed = useRef({ airplay: true, browser: true });
  const castAutoReady = useRef(false);
  const stageWasLive = useRef({ airplay: false, browser: false });
  const receiverSignature = useRef("");
  const dlnaUri = useRef<string | null>(null);
  const enteredStageFullscreen = useRef(false);

  const pinnedEntries = useMemo(() => state.entries.filter((entry) => entry.pinned), [state.entries]);
  const pinnedCount = pinnedEntries.length;

  const sidebarEntries = useMemo(
    () => state.entries.filter((entry) => entry.builtin && SIDEBAR_BUILTINS.includes(entry.builtin as BuiltinAction)),
    [state.entries],
  );

  const notify = useCallback(
    (text: string, tone: "success" | "error" | "info" = "success") => {
      if (toastTimer.current) window.clearTimeout(toastTimer.current);
      setToast({ id: Date.now(), text, tone });
      toastTimer.current = window.setTimeout(() => setToast(null), 3800);
    },
    [],
  );

  useEffect(() => {
    let active = true;
    loadState()
      .then((stored) => {
        if (!active) return;
        setState(normalizeState(stored));
        setHydrated(true);
      })
      .catch((reason) => {
        if (!active) return;
        setState(defaultState());
        setHydrated(true);
        notify(`读取配置失败：${displayError(reason)}`, "error");
      });
    return () => {
      active = false;
    };
  }, [notify]);

  useEffect(() => {
    if (!hydrated) return;
    const timer = window.setTimeout(() => {
      saveState(state)
        .then(() => {
          saveErrorShown.current = false;
        })
        .catch((reason) => {
          if (saveErrorShown.current) return;
          saveErrorShown.current = true;
          notify(`保存配置失败：${displayError(reason)}`, "error");
        });
    }, 240);
    return () => window.clearTimeout(timer);
  }, [state, hydrated, notify]);

  useEffect(() => {
    if (!hydrated || !state.settings.fullscreenOnStart) return;
    windowControl("fullscreenOn")
      .then(setIsFullscreen)
      .catch(() => undefined);
    // Only apply the saved startup mode once, on hydration.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [hydrated]);

  useEffect(() => suppressNativeContextMenu(), []);

  // Auto-start the media library hub so the browser extension always finds it.
  useEffect(() => {
    startSnapdropServer(7878).catch(() => undefined);
  }, []);

  // The sidebar is rendered inline. A second WebView2 window cannot be created
  // reliably on this system (0x8007139F), and retrying made the UI flash.

  // Listen for sidebar events
  useEffect(() => {
    const unlistenWhiteboard = listen("sidebar-open-whiteboard", () => {
      setWhiteboardOpen(true);
    });
    const unlistenFileTransfer = listen("sidebar-open-filetransfer", () => {
      setFileTransferOpen(true);
    });
    const unlistenAnnotation = listen("sidebar-open-annotation", () => {
      void launchAnnotationRef.current();
    });
    // The sidebar window has no toast of its own; a failed launch used to die
    // as a console.error nobody sees.
    const unlistenNotify = listen<{ text: string; tone?: "success" | "error" | "info" }>(
      "sidebar-notify",
      (event) => notify(event.payload.text, event.payload.tone ?? "error"),
    );
    // A phone can send at any time, panel open or not.
    const unlistenFile = listen<{ name: string }>("snapdrop-file", (event) => {
      notify(`已收到「${event.payload.name}」`, "success");
    });
    return () => {
      unlistenWhiteboard.then((fn) => fn());
      unlistenFileTransfer.then((fn) => fn());
      unlistenAnnotation.then((fn) => fn());
      unlistenNotify.then((fn) => fn());
      unlistenFile.then((fn) => fn());
    };
  }, [notify]);

  useEffect(() => {
    const refresh = () => {
      getSystemStatus()
        .then(setSystemStatus)
        .catch(() => undefined);
    };
    refresh();
    const interval = window.setInterval(refresh, 30_000);
    return () => window.clearInterval(interval);
  }, []);

  useEffect(() => {
    let active = true;
    const refresh = () => {
      receiverStatus()
        .then((next) => {
          if (!active) return;
          const signature = JSON.stringify(next);
          if (signature === receiverSignature.current) return;
          receiverSignature.current = signature;
          setReceiver(next);
        })
        .catch(() => undefined);
    };
    refresh();
    const interval = window.setInterval(refresh, 1000);
    return () => {
      active = false;
      window.clearInterval(interval);
    };
  }, []);

  // The first sample only establishes a baseline: a stream that was already
  // running before the app saw it is not a reason to take over the screen.
  useEffect(() => {
    if (!receiver) return;
    const live = {
      airplay: Boolean(receiver.airplay?.mirroring),
      browser: Boolean(receiver.browser?.connected),
    };
    if (!castAutoReady.current) {
      castAutoReady.current = true;
      castLive.current = live;
      return;
    }
    for (const source of ["airplay", "browser"] as const) {
      const previous = castLive.current[source];
      castLive.current[source] = live[source];
      if (!live[source]) {
        castArmed.current[source] = true;
        continue;
      }
      if (previous === false && castArmed.current[source]) {
        castArmed.current[source] = false;
        setCastStage(source);
        return;
      }
    }
  }, [receiver]);

  useEffect(() => {
    if (!castStage) {
      stageWasLive.current = { airplay: false, browser: false };
      return;
    }
    if (!receiver) return;
    const active = castStage === "airplay" ? receiver.airplay : receiver.browser;
    if (!active) {
      setCastStage(null);
      return;
    }
    const live =
      castStage === "airplay" ? Boolean(receiver.airplay?.mirroring) : Boolean(receiver.browser?.connected);
    if (live) {
      stageWasLive.current[castStage] = true;
    } else if (stageWasLive.current[castStage]) {
      // The sender hung up: leave the viewer instead of freezing on a dead frame.
      setCastStage(null);
    }
  }, [castStage, receiver]);

  // A DLNA cast has no window of its own: the picture is placed inside the
  // stage, so a new item has to bring the stage up or the operator sees black.
  useEffect(() => {
    const uri = receiver?.dlna?.uri ?? null;
    if (uri && uri !== dlnaUri.current) setDlnaStageOpen(true);
    dlnaUri.current = uri;
  }, [receiver]);

  useEffect(() => {
    const goOnline = () => setOnline(true);
    const goOffline = () => setOnline(false);
    window.addEventListener("online", goOnline);
    window.addEventListener("offline", goOffline);
    return () => {
      window.removeEventListener("online", goOnline);
      window.removeEventListener("offline", goOffline);
    };
  }, []);

  useEffect(() => {
    setFocusIndex((current) => Math.min(current, pinnedCount));
  }, [pinnedCount]);

  useEffect(() => {
    document.title = `${state.settings.deviceName || "HORIZON"} · 会议中枢`;
  }, [state.settings.deviceName]);

  useEffect(() => {
    return () => {
      if (toastTimer.current) window.clearTimeout(toastTimer.current);
    };
  }, []);

  const handleWindowAction = useCallback(
    async (action: WindowAction | "startDrag") => {
      try {
        if (action === "startDrag") {
          if (!isFullscreen) await windowControl("startDrag");
          return;
        }
        if (action === "toggleFullscreen") {
          const next = await windowControl(action);
          setIsFullscreen(next);
          return;
        }
        await windowControl(action);
      } catch (reason) {
        notify(`窗口操作失败：${displayError(reason)}`, "error");
      }
    },
    [isFullscreen, notify],
  );

  const setWindowFullscreen = useCallback(
    async (on: boolean) => {
      try {
        const next = await windowControl(on ? "fullscreenOn" : "fullscreenOff");
        setIsFullscreen(next);
      } catch (reason) {
        notify(`窗口操作失败：${displayError(reason)}`, "error");
      }
    },
    [notify],
  );

  const presentCast = useCallback((source: "airplay" | "browser") => {
    // Closing the stage by hand must not immediately reopen the same session.
    castArmed.current[source] = false;
    setCastStage(source);
  }, []);

  const presentingCast = castStage !== null;

  useEffect(() => {
    if (!presentingCast) return;
    // The window may already be fullscreen from F11 or the startup setting;
    // in that case the stage must leave it alone when it closes.
    if (!isFullscreen) {
      enteredStageFullscreen.current = true;
      void setWindowFullscreen(true);
    }
    return () => {
      if (!enteredStageFullscreen.current) return;
      enteredStageFullscreen.current = false;
      void setWindowFullscreen(false);
    };
    // Fullscreen is sampled once, when the stage opens. A later F11 press is
    // the user overriding it and must not be undone on close.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [presentingCast]);

  const launchAnnotation = useCallback(async () => {
    const path = state.settings.preferredAnnotationPath;
    if (!path) {
      notify("尚未配置批注应用，请在设置中选择", "info");
      setAllAppsOpen(false);
      setSettingsOpen(true);
      return;
    }
    try {
      await launchPath(path);
      notify(`已启动 ${state.settings.preferredAnnotationApp ?? "批注应用"}`, "success");
    } catch (reason) {
      notify(`无法启动批注应用：${displayError(reason)}`, "error");
    }
  }, [state.settings.preferredAnnotationPath, state.settings.preferredAnnotationApp, notify]);

  const launchAnnotationRef = useRef(launchAnnotation);
  launchAnnotationRef.current = launchAnnotation;

  const launchOffice = useCallback(async () => {
    const path = state.settings.preferredOfficePath;
    if (!path) {
      setAllAppsOpen(false);
      setSettingsOpen(false);
      setOfficeOpen(true);
      return;
    }
    try {
      await launchPath(path);
      notify(`已启动 ${state.settings.preferredOfficeApp ?? "办公应用"}`, "success");
    } catch (reason) {
      notify(`无法启动办公应用：${displayError(reason)}`, "error");
      // The saved path can go stale after an upgrade; offer the picker instead
      // of failing silently on every later click.
      setOfficeOpen(true);
    }
  }, [state.settings.preferredOfficePath, state.settings.preferredOfficeApp, notify]);

  const launchEntry = useCallback(
    async (entry: AppEntry) => {
      if (entry.builtin === "whiteboard") {
        setAllAppsOpen(false);
        setSettingsOpen(false);
        setWhiteboardOpen(true);
        return;
      }

      if (entry.builtin === "annotation") {
        setAllAppsOpen(false);
        setSettingsOpen(false);
        await launchAnnotation();
        return;
      }

      if (entry.builtin === "filetransfer") {
        setAllAppsOpen(false);
        setSettingsOpen(false);
        setFileTransferOpen(true);
        return;
      }

      if (entry.builtin === "welcome") {
        setAllAppsOpen(false);
        setSettingsOpen(false);
        setWelcomeOpen(true);
        return;
      }

      if (entry.builtin === "office") {
        setAllAppsOpen(false);
        setSettingsOpen(false);
        await launchOffice();
        return;
      }

      if (entry.builtin === "projection") {
        setAllAppsOpen(false);
        setSettingsOpen(false);
        setCastOpen(true);
        return;
      }

      setAllAppsOpen(false);
      setSettingsOpen(false);
      try {
        if (entry.builtin) {
          const hint = await openBuiltin(entry.builtin as Exclude<BuiltinAction, "whiteboard" | "annotation" | "filetransfer" | "welcome">);
          if (hint) notify(hint, "info");
          return;
        }
        if (!entry.path) {
          notify(`「${entry.name}」尚未配置启动路径`, "error");
          return;
        }
        notify(`正在打开 ${entry.name}`, "info");
        await launchPath(entry.path, entry.args);
      } catch (reason) {
        notify(`无法打开「${entry.name}」：${displayError(reason)}`, "error");
      }
    },
    [notify, launchAnnotation, launchOffice],
  );

  const buildEntry = useCallback(
    (application: ImportedApplication, existing: AppEntry[], shouldPin: boolean): AppEntry => {
      return {
        id: createId(),
        name: application.name.trim().slice(0, 24) || "未命名应用",
        path: application.path,
        icon: application.icon ?? null,
        accent: nextAccent(existing),
        pinned: shouldPin,
        addedAt: Date.now(),
      };
    },
    [],
  );

  const importApplications = useCallback(async () => {
    if (importing) return;
    setImporting(true);
    try {
      const picked = await pickApplications();
      if (!picked.length) return;

      const existingPaths = new Set(
        state.entries.map((entry) => entry.path?.toLocaleLowerCase()).filter(Boolean),
      );
      const fresh = picked.filter(
        (application) => !existingPaths.has(application.path.toLocaleLowerCase()),
      );
      if (!fresh.length) {
        notify("所选应用已在应用库中", "info");
        return;
      }

      if (fresh.length === 1) {
        const shouldPin = pinnedCount < MAX_PINNED;
        const draft = buildEntry(fresh[0], state.entries, shouldPin);
        setEditing({ entry: draft, isNew: true });
        return;
      }

      let availableSlots = Math.max(0, MAX_PINNED - pinnedCount);
      const working = [...state.entries];
      const additions = fresh.map((application) => {
        const entry = buildEntry(application, working, availableSlots > 0);
        working.push(entry);
        if (entry.pinned) availableSlots -= 1;
        return entry;
      });
      setState((previous) => ({ ...previous, entries: [...previous.entries, ...additions] }));
      const hiddenCount = additions.filter((entry) => !entry.pinned).length;
      notify(
        hiddenCount > 0
          ? `已添加 ${additions.length} 个应用，其中 ${hiddenCount} 个位于应用库`
          : `已添加 ${additions.length} 个应用`,
        "success",
      );
    } catch (reason) {
      notify(`添加应用失败：${displayError(reason)}`, "error");
    } finally {
      setImporting(false);
    }
  }, [buildEntry, importing, notify, pinnedCount, state.entries]);

  const saveEditedEntry = useCallback(
    (entry: AppEntry) => {
      const currentCount = state.entries.filter((item) => item.pinned).length;
      const wasPinned = editing?.entry.pinned ?? false;
      let nextEntry = entry;
      let limitHit = false;

      if (entry.pinned && !wasPinned && currentCount >= MAX_PINNED) {
        nextEntry = { ...entry, pinned: false };
        limitHit = true;
      }

      if (editing?.isNew) {
        setState((previous) => ({ ...previous, entries: [...previous.entries, nextEntry] }));
        notify(limitHit ? `首页已满，已将「${entry.name}」加入应用库` : `已添加「${entry.name}」`, limitHit ? "info" : "success");
      } else if (editing) {
        setState((previous) => ({
          ...previous,
          entries: previous.entries.map((item) =>
            item.id === editing.entry.id ? nextEntry : item,
          ),
        }));
        notify(limitHit ? "首页已满，应用已保存在应用库中" : "应用设置已保存", limitHit ? "info" : "success");
      }
      setEditing(null);
    },
    [editing, notify, state.entries],
  );

  const togglePin = useCallback(
    (entry: AppEntry) => {
      const current = state.entries.find((item) => item.id === entry.id);
      if (!current) return;
      if (!current.pinned && state.entries.filter((item) => item.pinned).length >= MAX_PINNED) {
        notify(`首页最多固定 ${MAX_PINNED} 个应用`, "info");
        return;
      }
      setState((previous) => ({
        ...previous,
        entries: previous.entries.map((item) =>
          item.id === entry.id ? { ...item, pinned: !current.pinned } : item,
        ),
      }));
      notify(current.pinned ? `已将「${entry.name}」从首页移除` : `已将「${entry.name}」固定到首页`, "success");
    },
    [notify, state.entries],
  );

  const reorderPinned = useCallback((ids: string[]) => {
    setState((previous) => {
      const pinnedById = new Map(
        previous.entries.filter((entry) => entry.pinned).map((entry) => [entry.id, entry]),
      );
      let cursor = 0;
      const entries = previous.entries.map((entry) => {
        if (!entry.pinned) return entry;
        const ordered = pinnedById.get(ids[cursor]);
        cursor += 1;
        return ordered ?? entry;
      });
      return { ...previous, entries };
    });
  }, []);

  const requestDelete = useCallback((entry: AppEntry) => {
    if (entry.builtin) return;
    setPendingDelete(entry);
  }, []);

  const confirmDelete = useCallback(() => {
    if (!pendingDelete) return;
    setState((previous) => ({
      ...previous,
      entries: previous.entries.filter((entry) => entry.id !== pendingDelete.id),
    }));
    if (editing?.entry.id === pendingDelete.id) setEditing(null);
    setPendingDelete(null);
    notify(`已移除「${pendingDelete.name}」`, "success");
  }, [editing, notify, pendingDelete]);

  const confirmReset = useCallback(() => {
    setState(defaultState());
    setPendingReset(false);
    setSettingsOpen(false);
    setAllAppsOpen(false);
    setEditing(null);
    setFocusIndex(0);
    notify("已恢复默认布局", "success");
  }, [notify]);

  const updateSettings = useCallback((patch: Partial<LauncherSettings>) => {
    setState((previous) => ({
      ...previous,
      settings: { ...previous.settings, ...patch },
    }));
  }, []);

  const openSettings = useCallback(() => {
    setAllAppsOpen(false);
    // The stage lives inside the cast panel, so closing that panel has to end
    // the presentation as well — otherwise the window stays fullscreen with no
    // stage on screen.
    setCastStage(null);
    setCastOpen(false);
    setSettingsOpen(true);
  }, []);

  const openAllApps = useCallback(() => {
    setSettingsOpen(false);
    setCastStage(null);
    setCastOpen(false);
    setAllAppsOpen(true);
  }, []);

  useEffect(() => {
    const handleKey = (event: KeyboardEvent) => {
      if (castStage) {
        // The stage is a modal viewer: only its own exit key should reach the launcher.
        if (event.key === "Escape") {
          event.preventDefault();
          setCastStage(null);
        }
        return;
      }
      if (event.key === "F11") {
        event.preventDefault();
        void handleWindowAction("toggleFullscreen");
        return;
      }
      if (whiteboardOpen || fileTransferOpen || welcomeOpen || officeOpen || dlnaStageOpen || settingsOpen || castOpen || allAppsOpen || editing || pendingDelete || pendingReset) {
        return;
      }
      if (isTypingTarget(event.target)) return;

      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        openAllApps();
        return;
      }
      if ((event.ctrlKey || event.metaKey) && event.key === ",") {
        event.preventDefault();
        openSettings();
        return;
      }
      if (event.key === "ArrowRight" || event.key === "ArrowLeft") {
        event.preventDefault();
        const lastIndex = pinnedEntries.length;
        setFocusIndex((current) => {
          if (event.key === "ArrowRight") return current >= lastIndex ? 0 : current + 1;
          return current <= 0 ? lastIndex : current - 1;
        });
        return;
      }
      if (event.key === "Enter") {
        event.preventDefault();
        if (focusIndex >= pinnedEntries.length) {
          openAllApps();
          return;
        }
        const entry = pinnedEntries[focusIndex];
        if (entry) void launchEntry(entry);
      }
    };
    window.addEventListener("keydown", handleKey);
    return () => window.removeEventListener("keydown", handleKey);
  }, [
    allAppsOpen,
    castOpen,
    castStage,
    dlnaStageOpen,
    editing,
    fileTransferOpen,
    focusIndex,
    handleWindowAction,
    launchEntry,
    officeOpen,
    openAllApps,
    openSettings,
    pendingDelete,
    pendingReset,
    pinnedEntries,
    settingsOpen,
    welcomeOpen,
    whiteboardOpen,
  ]);

  return (
    <div className={`app-shell ${hydrated ? "is-ready" : ""}`}>
      {!whiteboardOpen && !welcomeOpen && !dlnaStageOpen && (
        <AmbientBackground
          wallpaper={state.settings.wallpaper}
          customWallpapers={state.settings.customWallpapers}
        />
      )}

      <div className="launcher">
        <TopBar
          showSeconds={state.settings.showSeconds}
          status={systemStatus}
          online={online}
          isFullscreen={isFullscreen}
          onOpenSettings={openSettings}
          onWindowAction={handleWindowAction}
        />

        <Hero settings={state.settings} pinnedCount={pinnedCount} />

        <Dock
          entries={state.entries}
          focusIndex={focusIndex}
          onFocus={setFocusIndex}
          onLaunch={(entry) => void launchEntry(entry)}
          onOpenAll={openAllApps}
          onOpenSettings={openSettings}
        />
      </div>

      {allAppsOpen && (
        <AllApps
          entries={state.entries}
          onLaunch={(entry) => void launchEntry(entry)}
          onTogglePin={togglePin}
          onImport={() => void importApplications()}
          onOpenSettings={openSettings}
          onClose={() => setAllAppsOpen(false)}
        />
      )}

      {settingsOpen && (
        <Settings
          entries={state.entries}
          settings={state.settings}
          importing={importing}
          onUpdateSettings={updateSettings}
          onImport={() => void importApplications()}
          onEditEntry={(entry) => setEditing({ entry, isNew: false })}
          onTogglePin={togglePin}
          onDeleteEntry={requestDelete}
          onReorderPinned={reorderPinned}
          onReset={() => setPendingReset(true)}
          onClose={() => setSettingsOpen(false)}
          onNotify={notify}
        />
      )}

      {castOpen && (
        <CastPanel
          deviceName={state.settings.deviceName}
          presenting={castStage}
          onPresent={presentCast}
          onStageClose={() => setCastStage(null)}
          onPresentDlna={() => setDlnaStageOpen(true)}
          onNotify={notify}
          onClose={() => setCastOpen(false)}
        />
      )}

      {dlnaStageOpen && receiver?.dlna && (
        <DlnaStage status={receiver.dlna} onClose={() => setDlnaStageOpen(false)} />
      )}

      {editing && (
        <AppEditor
          key={`${editing.entry.id}-${editing.isNew}`}
          entry={editing.entry}
          isNew={editing.isNew}
          pinnedCount={pinnedCount}
          onSave={saveEditedEntry}
          onDelete={requestDelete}
          onClose={() => setEditing(null)}
        />
      )}

      {whiteboardOpen && (
        <Whiteboard onExit={() => setWhiteboardOpen(false)} onNotify={notify} />
      )}

      {state.settings.sidebarEnabled && !whiteboardOpen && !welcomeOpen && (
        <Sidebar
          entries={sidebarEntries}
          position={state.settings.sidebarPosition}
          x={state.settings.sidebarX}
          y={state.settings.sidebarY}
          onLaunch={(entry) => void launchEntry(entry)}
          onClose={() => updateSettings({ sidebarEnabled: false })}
          onPositionChange={(position, x, y) =>
            updateSettings({ sidebarPosition: position, sidebarX: x, sidebarY: y })
          }
        />
      )}

      {fileTransferOpen && (
        <FileTransferPanel
          onClose={() => setFileTransferOpen(false)}
          onNotify={notify}
        />
      )}

      {welcomeOpen && (
        <WelcomePanel
          themes={state.settings.welcomeThemes}
          activeThemeId={state.settings.welcomeActiveTheme}
          onThemesChange={(welcomeThemes) => updateSettings({ welcomeThemes })}
          onActiveThemeChange={(welcomeActiveTheme) => updateSettings({ welcomeActiveTheme })}
          onClose={() => setWelcomeOpen(false)}
          onNotify={notify}
        />
      )}

      {officeOpen && (
        <OfficePanel
          preferredApp={state.settings.preferredOfficeApp}
          preferredPath={state.settings.preferredOfficePath}
          onSelect={(name, path) =>
            updateSettings({ preferredOfficeApp: name, preferredOfficePath: path })
          }
          onClose={() => setOfficeOpen(false)}
          onNotify={notify}
        />
      )}

      <Toast toast={toast} />

      {pendingDelete && (
        <ConfirmDialog
          title={`移除「${pendingDelete.name}」？`}
          description="应用会从应用库和首页快捷方式中移除，但这不会卸载或删除本机上的程序文件。"
          confirmLabel="移除应用"
          destructive
          onConfirm={confirmDelete}
          onCancel={() => setPendingDelete(null)}
        />
      )}

      {pendingReset && (
        <ConfirmDialog
          title="恢复默认配置？"
          description="这将清除所有自定义应用、首页排序和大屏个性化设置，并恢复预置应用。此操作无法撤销。"
          confirmLabel="恢复默认"
          destructive
          onConfirm={confirmReset}
          onCancel={() => setPendingReset(false)}
        />
      )}
    </div>
  );
}

function displayError(reason: unknown) {
  if (typeof reason === "string") return reason;
  if (reason instanceof Error) return reason.message;
  return "发生未知错误";
}
