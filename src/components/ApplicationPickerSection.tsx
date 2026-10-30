import { useCallback, useEffect, useState } from "react";
import { detectAnnotationApps, detectOfficeApps, pickApplications } from "../lib/bridge";
import { SlidersIcon } from "./Icons";

export type ApplicationPickerKind = "annotation" | "office";

const COPY: Record<
  ApplicationPickerKind,
  {
    title: string;
    description: string;
    emptyHint: string;
    icon: (name: string) => string;
  }
> = {
  annotation: {
    title: "批注应用",
    description:
      "选择已安装的批注应用（如 InKeys、Ink Canvas、Epic Pen），点击「批注」时将启动该应用。未检测到时可直接浏览电脑选择 EXE 或快捷方式。",
    emptyHint: "未检测到常见批注应用，请点击「浏览本机应用」手动选择",
    icon: () => "✏️",
  },
  office: {
    title: "办公应用",
    description:
      "选择常用的办公应用（如 Word、WPS、LibreOffice），点击「办公应用」时将直接启动它。未检测到时可直接浏览电脑选择 EXE 或快捷方式。",
    emptyHint: "未检测到办公应用，请点击「浏览本机应用」手动选择",
    icon: (name) => name.trim().charAt(0).toUpperCase() || "文",
  },
};

interface ApplicationPickerSectionProps {
  kind: ApplicationPickerKind;
  selectedApp?: string;
  selectedPath?: string;
  onSelect: (name: string, path: string) => void;
  onClear: () => void;
}

export function ApplicationPickerSection({
  kind,
  selectedApp,
  selectedPath,
  onSelect,
  onClear,
}: ApplicationPickerSectionProps) {
  const copy = COPY[kind];
  const [apps, setApps] = useState<[string, string][]>([]);
  const [detecting, setDetecting] = useState(true);
  const [browsing, setBrowsing] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(() => {
    setDetecting(true);
    setError(null);
    const detect = kind === "office" ? detectOfficeApps : detectAnnotationApps;
    detect()
      .then((found) => setApps(found))
      .catch(() => setApps([]))
      .finally(() => setDetecting(false));
  }, [kind]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const handleBrowse = useCallback(async () => {
    if (browsing) return;
    setBrowsing(true);
    setError(null);
    try {
      const first = (await pickApplications())[0];
      if (first) onSelect(first.name, first.path);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : "无法打开文件选择窗口");
    } finally {
      setBrowsing(false);
    }
  }, [browsing, onSelect]);

  return (
    <section className="settings-section">
      <div className="settings-section__title">
        <div>
          <SlidersIcon size={17} />
          <h3>{copy.title}</h3>
        </div>
        <div className="app-picker-actions">
          <button
            type="button"
            className="button button--small"
            onClick={refresh}
            disabled={detecting || browsing}
          >
            {detecting ? "检测中…" : "重新检测"}
          </button>
          <button
            type="button"
            className="button button--small button--primary"
            onClick={() => void handleBrowse()}
            disabled={browsing}
          >
            {browsing ? "选择中…" : "浏览本机应用"}
          </button>
        </div>
      </div>
      <p className="settings-section__description">{copy.description}</p>

      {detecting ? (
        <div className="app-picker-list">
          <span className="app-picker-status">正在检测{copy.title}…</span>
        </div>
      ) : apps.length === 0 ? (
        <div className="app-picker-list">
          <span className="app-picker-status app-picker-status--empty">{copy.emptyHint}</span>
        </div>
      ) : (
        <div className="app-picker-list">
          {apps.map(([name, path]) => {
            const isSelected = selectedPath === path;
            return (
              <button
                key={path}
                type="button"
                className={`app-picker-option${isSelected ? " is-selected" : ""}`}
                onClick={() => onSelect(name, path)}
              >
                <span className="app-picker-option__icon">{copy.icon(name)}</span>
                <span className="app-picker-option__info">
                  <strong>{name}</strong>
                  <small title={path}>{path}</small>
                </span>
                {isSelected && <span className="app-picker-option__check">✓</span>}
              </button>
            );
          })}
        </div>
      )}

      {error && <p className="app-picker-status app-picker-status--empty">{error}</p>}

      {selectedApp && (
        <div className="app-picker-current">
          <span>
            {kind === "office" ? "默认应用：" : "当前首选："}
            <strong>{selectedApp}</strong>
          </span>
          <button type="button" className="text-button text-button--small" onClick={onClear}>
            清除选择
          </button>
        </div>
      )}
    </section>
  );
}
