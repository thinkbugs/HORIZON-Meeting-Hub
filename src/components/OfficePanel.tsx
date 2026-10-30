import { useCallback, useEffect, useMemo, useState } from "react";
import { createPortal } from "react-dom";
import { detectOfficeApps, launchPath, pickApplications } from "../lib/bridge";
import { CloseIcon } from "./Icons";

interface OfficePanelProps {
  preferredApp?: string;
  preferredPath?: string;
  onSelect: (name: string, path: string) => void;
  onClose: () => void;
  onNotify: (text: string, tone?: "success" | "error" | "info") => void;
}

function errorText(reason: unknown, fallback: string): string {
  return reason instanceof Error && reason.message ? reason.message : fallback;
}

export function OfficePanel({
  preferredApp,
  preferredPath,
  onSelect,
  onClose,
  onNotify,
}: OfficePanelProps) {
  const [apps, setApps] = useState<[string, string][]>([]);
  const [detecting, setDetecting] = useState(true);
  const [browsing, setBrowsing] = useState(false);
  const [launching, setLaunching] = useState<string | null>(null);

  const refresh = useCallback(() => {
    setDetecting(true);
    detectOfficeApps()
      .then(setApps)
      .catch(() => setApps([]))
      .finally(() => setDetecting(false));
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const handleBrowse = useCallback(async () => {
    if (browsing) return;
    setBrowsing(true);
    try {
      const picked = await pickApplications();
      const first = picked[0];
      if (!first) return;
      setApps((prev) =>
        prev.some(([, path]) => path.toLowerCase() === first.path.toLowerCase())
          ? prev
          : [[first.name, first.path], ...prev],
      );
      onSelect(first.name, first.path);
    } catch (reason) {
      onNotify(errorText(reason, "无法打开文件选择窗口"), "error");
    } finally {
      setBrowsing(false);
    }
  }, [browsing, onNotify, onSelect]);

  const handleLaunch = useCallback(
    async (name: string, path: string) => {
      if (launching) return;
      setLaunching(path);
      try {
        await launchPath(path);
        onSelect(name, path);
        onNotify(`已启动 ${name}`, "success");
        onClose();
      } catch (reason) {
        onNotify(`无法启动「${name}」：${errorText(reason, "请确认该程序仍然存在")}`, "error");
      } finally {
        setLaunching(null);
      }
    },
    [launching, onClose, onNotify, onSelect],
  );

  const handleSetDefault = useCallback(
    (name: string, path: string) => {
      onSelect(name, path);
      onNotify(`已将 ${name} 设为默认办公应用`, "success");
    },
    [onSelect, onNotify],
  );

  const handleBackdropClick = useCallback(
    (event: React.MouseEvent<HTMLDivElement>) => {
      if (event.target === event.currentTarget) onClose();
    },
    [onClose],
  );

  const handleKeyDown = useCallback(
    (event: React.KeyboardEvent<HTMLDivElement>) => {
      if (event.key === "Escape") {
        event.stopPropagation();
        onClose();
      }
    },
    [onClose],
  );

  const defaultName = useMemo(() => {
    if (!preferredPath) return null;
    return preferredApp ?? preferredPath.split(/[\\/]/).pop() ?? preferredPath;
  }, [preferredApp, preferredPath]);

  const panel = (
    <div
      className="office-backdrop"
      onClick={handleBackdropClick}
      onKeyDown={handleKeyDown}
    >
      <div
        className="office-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="office-title"
      >
        <header className="office-header">
          <div>
            <p className="section-eyebrow">OFFICE LAUNCHER</p>
            <h2 id="office-title" className="office-title">
              办公应用
            </h2>
          </div>
          <div className="app-picker-actions">
            <button
              type="button"
              className="text-button office-header-action"
              onClick={refresh}
              disabled={detecting}
            >
              重新检测
            </button>
            <button
              type="button"
              className="text-button office-header-action"
              onClick={() => void handleBrowse()}
              disabled={browsing}
            >
              {browsing ? "选择中…" : "浏览本机应用"}
            </button>
            <button className="icon-button modal-close" onClick={onClose} aria-label="关闭">
              <CloseIcon size={18} />
            </button>
          </div>
        </header>

        <div className="office-body">
          <p className="office-hint">
            {preferredPath
              ? "点击应用即可启动；设为默认后，之后点击「办公应用」会直接打开它。"
              : "选择一个常用的办公应用，之后点击「办公应用」会直接启动它。"}
          </p>

          {detecting ? (
            <div className="app-picker-list office-app-list">
              <span className="app-picker-status">正在检测本机办公应用…</span>
            </div>
          ) : apps.length === 0 ? (
            <div className="app-picker-list office-app-list">
              <span className="app-picker-status app-picker-status--empty">
                未检测到办公应用，请点击「浏览本机应用」手动选择 Word、WPS、Excel 等程序
              </span>
            </div>
          ) : (
            <div className="app-picker-list office-app-list">
              {apps.map(([name, path]) => {
                const isPreferred = path === preferredPath;
                const isLaunching = launching === path;
                return (
                  <div
                    key={path}
                    className={`app-picker-option office-app-row${
                      isPreferred ? " is-selected" : ""
                    }`}
                  >
                    <button
                      type="button"
                      className="office-app-launch"
                      onClick={() => void handleLaunch(name, path)}
                      disabled={Boolean(launching)}
                      title={path}
                    >
                      <span className="app-picker-option__icon">
                        {name.trim().charAt(0).toUpperCase()}
                      </span>
                      <span className="app-picker-option__info">
                        <strong>{name}</strong>
                        <small>{isLaunching ? "启动中…" : path}</small>
                      </span>
                    </button>
                    {isPreferred ? (
                      <span className="app-picker-option__check">默认</span>
                    ) : (
                      <button
                        type="button"
                        className="text-button office-app-default"
                        onClick={() => handleSetDefault(name, path)}
                      >
                        设为默认
                      </button>
                    )}
                  </div>
                );
              })}
            </div>
          )}
        </div>

        <footer className="office-footer">
          <span className="office-footer-hint">
            {defaultName ? `默认应用：${defaultName}` : "尚未设置默认办公应用"}
          </span>
          <button className="button button--primary" onClick={onClose}>
            完成
          </button>
        </footer>
      </div>
    </div>
  );

  return createPortal(panel, document.body);
}
