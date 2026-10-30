import { useCallback, useEffect, useState } from "react";
import {
  MAX_PINNED,
  type AppEntry,
  type LauncherSettings,
} from "../types";
import { BUILTIN_WALLPAPERS } from "../lib/wallpapers";
import { autostartStatus, setAutostart } from "../lib/bridge";
import { ApplicationPickerSection } from "./ApplicationPickerSection";
import { EntryIcon } from "./AppIcon";
import {
  CloseIcon,
  EditIcon,
  GripIcon,
  ImageIcon,
  PinIcon,
  PlusIcon,
  SlidersIcon,
  TrashIcon,
} from "./Icons";
import { Modal, Switch } from "./Overlay";

interface SettingsProps {
  entries: AppEntry[];
  settings: LauncherSettings;
  importing: boolean;
  onUpdateSettings: (patch: Partial<LauncherSettings>) => void;
  onImport: () => void;
  onEditEntry: (entry: AppEntry) => void;
  onTogglePin: (entry: AppEntry) => void;
  onDeleteEntry: (entry: AppEntry) => void;
  onReorderPinned: (ids: string[]) => void;
  onReset: () => void;
  onClose: () => void;
  onNotify: (text: string, tone?: "success" | "error" | "info") => void;
}

export function Settings({
  entries,
  settings,
  importing,
  onUpdateSettings,
  onImport,
  onEditEntry,
  onTogglePin,
  onDeleteEntry,
  onReorderPinned,
  onReset,
  onClose,
  onNotify,
}: SettingsProps) {
  const [draggedId, setDraggedId] = useState<string | null>(null);
  const [dropTarget, setDropTarget] = useState<string | null>(null);
  const [autostart, setAutostartState] = useState<boolean | null>(null);
  const pinned = entries.filter((entry) => entry.pinned);

  // The Run key is the truth, not a copy in launcher-state.json: a value the
  // user removed by hand must show up as off here.
  useEffect(() => {
    let alive = true;
    autostartStatus()
      .then((value) => {
        if (alive) setAutostartState(value);
      })
      .catch(() => {
        if (alive) setAutostartState(false);
      });
    return () => {
      alive = false;
    };
  }, []);

  const handleAutostart = useCallback(
    async (enabled: boolean) => {
      try {
        const actual = await setAutostart(enabled);
        setAutostartState(actual);
        onNotify(actual ? "已设置为开机自动启动" : "已取消开机自动启动", "success");
      } catch (reason) {
        setAutostartState(!enabled);
        onNotify(`设置开机启动失败：${String(reason)}`, "error");
      }
    },
    [onNotify],
  );

  const handleSelectAnnotationApp = useCallback(
    (name: string, path: string) => {
      onUpdateSettings({
        preferredAnnotationApp: name,
        preferredAnnotationPath: path,
      });
    },
    [onUpdateSettings],
  );

  const handleClearAnnotationApp = useCallback(() => {
    onUpdateSettings({
      preferredAnnotationApp: undefined,
      preferredAnnotationPath: undefined,
    });
  }, [onUpdateSettings]);

  const handleSelectOfficeApp = useCallback(
    (name: string, path: string) => {
      onUpdateSettings({
        preferredOfficeApp: name,
        preferredOfficePath: path,
      });
    },
    [onUpdateSettings],
  );

  const handleClearOfficeApp = useCallback(() => {
    onUpdateSettings({
      preferredOfficeApp: undefined,
      preferredOfficePath: undefined,
    });
  }, [onUpdateSettings]);

  const handleDrop = (targetId: string) => {
    if (!draggedId || draggedId === targetId) {
      setDraggedId(null);
      setDropTarget(null);
      return;
    }
    const ids = pinned.map((entry) => entry.id);
    const from = ids.indexOf(draggedId);
    const to = ids.indexOf(targetId);
    if (from < 0 || to < 0) return;
    ids.splice(from, 1);
    ids.splice(to, 0, draggedId);
    onReorderPinned(ids);
    setDraggedId(null);
    setDropTarget(null);
  };

  return (
    <Modal onClose={onClose} variant="drawer" className="settings-drawer" labelledBy="settings-title">
      <header className="settings__header">
        <div>
          <p className="section-eyebrow">LAUNCHER SETTINGS</p>
          <h2 id="settings-title">个性化与设置</h2>
        </div>
        <button className="icon-button modal-close" onClick={onClose} aria-label="关闭设置">
          <CloseIcon size={20} />
        </button>
      </header>

      <div className="settings__scroll">
        <section className="settings-section">
          <div className="settings-section__title">
            <div>
              <SlidersIcon size={17} />
              <h3>大屏信息</h3>
            </div>
          </div>
          <label className="field">
            <span className="field__label">
              大屏名称
              <small>{settings.deviceName.length}/30</small>
            </span>
            <input
              value={settings.deviceName}
              maxLength={30}
              onChange={(event) => onUpdateSettings({ deviceName: event.target.value })}
              placeholder="HORIZON 86"
            />
          </label>
          <label className="field">
            <span className="field__label">
              欢迎语
              <small>{settings.tagline.length}/60</small>
            </span>
            <input
              value={settings.tagline}
              maxLength={60}
              onChange={(event) => onUpdateSettings({ tagline: event.target.value })}
              placeholder="让每一次协作，都清晰发生"
            />
          </label>
          <div className="setting-row">
            <div>
              <strong>在大屏上显示</strong>
              <p>关掉后主屏只留壁纸与快捷入口，不遮挡壁纸自带的品牌信息。</p>
            </div>
            <Switch
              checked={settings.showScreenInfo}
              onChange={(value) => onUpdateSettings({ showScreenInfo: value })}
              label="在大屏上显示大屏信息"
            />
          </div>
          <p className="settings-note">
            {settings.showScreenInfo
              ? "名称与欢迎语会实时呈现在主屏中央。"
              : "名称与欢迎语已隐藏，随时可在这里打开。"}
          </p>
        </section>

        <section className="settings-section">
          <div className="settings-section__title">
            <div>
              <ImageIcon size={17} />
              <h3>桌面壁纸</h3>
            </div>
          </div>
          <p className="settings-section__description">
            选择内置壁纸或添加企业自定义壁纸。
          </p>
          <div className="wallpaper-grid">
            {BUILTIN_WALLPAPERS.map((wp) => (
              <button
                key={wp.id}
                type="button"
                className={`wallpaper-option ${settings.wallpaper === wp.id ? "is-selected" : ""}`}
                onClick={() => onUpdateSettings({ wallpaper: wp.id })}
                aria-label={`选择${wp.name}壁纸`}
              >
                <div
                  className="wallpaper-option__preview"
                  style={{ background: wp.value }}
                />
                <span className="wallpaper-option__name">{wp.name}</span>
              </button>
            ))}
            {settings.customWallpapers.map((url, index) => (
              <div key={`custom-${index}`} className="wallpaper-option is-custom">
                <button
                  type="button"
                  className={`wallpaper-option__remove ${settings.wallpaper === `custom-${index}` ? "is-selected" : ""}`}
                  onClick={() => {
                    const next = settings.customWallpapers.filter((_, i) => i !== index);
                    const newWallpaper = settings.wallpaper === `custom-${index}` ? "nebula" : settings.wallpaper;
                    onUpdateSettings({ customWallpapers: next, wallpaper: newWallpaper });
                  }}
                  aria-label="删除此壁纸"
                >
                  <TrashIcon size={12} />
                </button>
                <button
                  type="button"
                  className={`wallpaper-option ${settings.wallpaper === `custom-${index}` ? "is-selected" : ""}`}
                  onClick={() => onUpdateSettings({ wallpaper: `custom-${index}` })}
                  aria-label={`选择自定义壁纸 ${index + 1}`}
                >
                  <div
                    className="wallpaper-option__preview"
                    style={{ backgroundImage: `url(${url})` }}
                  />
                  <span className="wallpaper-option__name">自定义 {index + 1}</span>
                </button>
              </div>
            ))}
            <label className="wallpaper-option wallpaper-option--add">
              <input
                type="file"
                accept="image/*"
                className="wallpaper-option__input"
                onChange={(event) => {
                  const file = event.target.files?.[0];
                  if (!file) return;
                  const reader = new FileReader();
                  reader.onload = () => {
                    const url = reader.result as string;
                    const next = [...settings.customWallpapers, url];
                    onUpdateSettings({
                      customWallpapers: next,
                      wallpaper: `custom-${next.length - 1}`,
                    });
                  };
                  reader.readAsDataURL(file);
                }}
              />
              <div className="wallpaper-option__preview">
                <PlusIcon size={20} />
              </div>
              <span className="wallpaper-option__name">添加壁纸</span>
            </label>
          </div>
        </section>

        <section className="settings-section">
          <div className="settings-section__title">
            <div>
              <PinIcon size={17} />
              <h3>首页快捷方式</h3>
            </div>
            <span className="count-chip">
              {pinned.length} / {MAX_PINNED}
            </span>
          </div>
          <p className="settings-section__description">
            拖动调整顺序。首页展示的快捷应用可自由增减，全部应用入口始终保留。
          </p>

          {pinned.length > 0 ? (
            <div className="pinned-editor">
              {pinned.map((entry, index) => (
                <div
                  key={entry.id}
                  className={`pinned-editor__row ${draggedId === entry.id ? "is-dragging" : ""} ${
                    dropTarget === entry.id ? "is-drop-target" : ""
                  }`}
                  draggable
                  onDragStart={(event) => {
                    setDraggedId(entry.id);
                    event.dataTransfer.effectAllowed = "move";
                    event.dataTransfer.setData("text/plain", entry.id);
                  }}
                  onDragEnd={() => {
                    setDraggedId(null);
                    setDropTarget(null);
                  }}
                  onDragOver={(event) => {
                    event.preventDefault();
                    event.dataTransfer.dropEffect = "move";
                    if (dropTarget !== entry.id) setDropTarget(entry.id);
                  }}
                  onDrop={(event) => {
                    event.preventDefault();
                    handleDrop(entry.id);
                  }}
                >
                  <span className="pinned-editor__grip">
                    <GripIcon size={16} />
                  </span>
                  <span className="pinned-editor__order">
                    {String(index + 1).padStart(2, "0")}
                  </span>
                  <EntryIcon entry={entry} glyphSize={24} className="entry-icon--compact" />
                  <span className="pinned-editor__name">{entry.name}</span>
                  <button
                    className="icon-button icon-button--small"
                    onClick={() => onTogglePin(entry)}
                    title="取消固定"
                    aria-label={`取消固定 ${entry.name}`}
                  >
                    <PinIcon size={15} filled />
                  </button>
                </div>
              ))}
            </div>
          ) : (
            <div className="pinned-editor__empty">还没有固定应用，在下方应用库中点击图钉添加。</div>
          )}
        </section>

        <section className="settings-section">
          <div className="settings-section__title">
            <div>
              <PlusIcon size={17} />
              <h3>应用库</h3>
            </div>
            <button className="button button--small button--primary" onClick={onImport} disabled={importing}>
              <PlusIcon size={14} />
              {importing ? "正在添加…" : "添加应用"}
            </button>
          </div>
          <p className="settings-section__description">
            从电脑中选择 EXE、LNK、BAT 等程序；系统会自动读取名称与高清图标。
          </p>

          <div className="app-list">
            {entries.map((entry) => (
              <div className="app-list__row" key={entry.id}>
                <EntryIcon entry={entry} glyphSize={23} className="entry-icon--compact" />
                <div className="app-list__copy">
                  <div>
                    <strong>{entry.name}</strong>
                    {entry.pinned && <span className="badge">首页</span>}
                    {entry.builtin && <span className="badge badge--muted">内置</span>}
                  </div>
                  <span title={entry.path}>{entry.builtin ? "HORIZON 系统应用" : entry.path}</span>
                </div>
                <div className="app-list__actions">
                  <button
                    className={`icon-button icon-button--small ${entry.pinned ? "is-active" : ""}`}
                    onClick={() => onTogglePin(entry)}
                    title={entry.pinned ? "取消固定" : "固定到首页"}
                    aria-label={entry.pinned ? `取消固定 ${entry.name}` : `固定 ${entry.name} 到首页`}
                  >
                    <PinIcon size={15} filled={entry.pinned} />
                  </button>
                  {!entry.builtin && (
                    <>
                      <button
                        className="icon-button icon-button--small"
                        onClick={() => onEditEntry(entry)}
                        title="编辑应用"
                        aria-label={`编辑 ${entry.name}`}
                      >
                        <EditIcon size={15} />
                      </button>
                      <button
                        className="icon-button icon-button--small icon-button--danger"
                        onClick={() => onDeleteEntry(entry)}
                        title="移除应用"
                        aria-label={`移除 ${entry.name}`}
                      >
                        <TrashIcon size={15} />
                      </button>
                    </>
                  )}
                </div>
              </div>
            ))}
          </div>
        </section>

        <section className="settings-section">
          <div className="settings-section__title">
            <div>
              <SlidersIcon size={17} />
              <h3>工具侧边栏</h3>
            </div>
            <Switch
              checked={settings.sidebarEnabled}
              onChange={(value) => onUpdateSettings({ sidebarEnabled: value })}
              label="启用侧边栏"
            />
          </div>
          <p className="settings-section__description">
            浮动工具栏，包含记事本、截屏、录屏、批注等常用工具。可拖动到任意位置。
          </p>
          <div className="sidebar-position-picker">
            {(["left", "right"] as const).map((pos) => (
              <button
                key={pos}
                type="button"
                className={`sidebar-position-btn ${settings.sidebarPosition === pos ? "is-active" : ""}`}
                onClick={() => onUpdateSettings({ sidebarPosition: pos })}
                disabled={!settings.sidebarEnabled}
              >
                {pos === "left" ? "左侧" : "右侧"}
              </button>
            ))}
          </div>
        </section>

        <ApplicationPickerSection
          kind="annotation"
          selectedApp={settings.preferredAnnotationApp}
          selectedPath={settings.preferredAnnotationPath}
          onSelect={handleSelectAnnotationApp}
          onClear={handleClearAnnotationApp}
        />

        <ApplicationPickerSection
          kind="office"
          selectedApp={settings.preferredOfficeApp}
          selectedPath={settings.preferredOfficePath}
          onSelect={handleSelectOfficeApp}
          onClear={handleClearOfficeApp}
        />

        <section className="settings-section">
          <div className="settings-section__title">
            <div>
              <SlidersIcon size={17} />
              <h3>系统行为</h3>
            </div>
          </div>
          <div className="setting-row">
            <div>
              <strong>启动时进入全屏</strong>
              <p>适用于会议室大屏与会前迎宾模式；按 F11 可随时切换。</p>
            </div>
            <Switch
              checked={settings.fullscreenOnStart}
              onChange={(value) => onUpdateSettings({ fullscreenOnStart: value })}
              label="启动时进入全屏"
            />
          </div>
          <div className="setting-row">
            <div>
              <strong>时钟显示秒数</strong>
              <p>顶部状态栏以秒级精度显示当前时间。</p>
            </div>
            <Switch
              checked={settings.showSeconds}
              onChange={(value) => onUpdateSettings({ showSeconds: value })}
              label="时钟显示秒数"
            />
          </div>
          <div className="setting-row">
            <div>
              <strong>开机自动启动</strong>
              <p>登录 Windows 后自动打开启动器，直接回到会议室大屏。</p>
            </div>
            <Switch
              checked={autostart ?? false}
              onChange={(value) => void handleAutostart(value)}
              label="开机自动启动"
            />
          </div>
        </section>

        <section className="settings-section settings-section--about">
          <div className="about-brand">
            <span>HORIZON</span>
            <small>v0.5.1</small>
          </div>
          <p>本地会议启动器。应用配置与快捷方式仅保存在这台设备上。</p>
          <p>白板引擎基于 Drawnix 与 Plait 构建，遵循 MIT License。</p>
          <p>AirPlay 接收端为独立实现，FairPlay 协议常量参考 Airplayer（MIT）。</p>
          <div className="about-actions">
            <span className="local-badge">
              <span />
              LOCAL ONLY
            </span>
            <button className="text-button text-button--danger" onClick={onReset}>
              恢复默认配置
            </button>
          </div>
        </section>
      </div>

      <footer className="settings__footer">
        <span className="settings__save-state">
          <span />
          更改自动保存
        </span>
        <button className="button button--primary" onClick={onClose}>
          完成
        </button>
      </footer>
    </Modal>
  );
}
