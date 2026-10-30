import { useEffect, useMemo, useState, type MouseEvent } from "react";
import type { SystemStatus, WindowAction } from "../types";
import {
  BatteryIcon,
  BrandMark,
  CloseIcon,
  FullscreenIcon,
  GearIcon,
  MinimizeIcon,
  WifiIcon,
} from "./Icons";

interface TopBarProps {
  showSeconds: boolean;
  status: SystemStatus;
  online: boolean;
  isFullscreen: boolean;
  onOpenSettings: () => void;
  onWindowAction: (action: WindowAction | "startDrag") => void;
}

export function TopBar({
  showSeconds,
  status,
  online,
  isFullscreen,
  onOpenSettings,
  onWindowAction,
}: TopBarProps) {
  const [now, setNow] = useState(() => new Date());

  useEffect(() => {
    const interval = window.setInterval(
      () => setNow(new Date()),
      showSeconds ? 1000 : 30_000,
    );
    return () => window.clearInterval(interval);
  }, [showSeconds]);

  const date = useMemo(
    () =>
      new Intl.DateTimeFormat("zh-CN", {
        month: "long",
        day: "numeric",
        weekday: "short",
      }).format(now),
    [now],
  );

  const time = useMemo(() => {
    const formatter = new Intl.DateTimeFormat("zh-CN", {
      hour: "2-digit",
      minute: "2-digit",
      ...(showSeconds ? { second: "2-digit" as const } : {}),
      hour12: false,
    });
    return formatter.format(now);
  }, [now, showSeconds]);

  const startDrag = (event: MouseEvent<HTMLElement>) => {
    if (event.button !== 0) return;
    if ((event.target as HTMLElement).closest("button, input, a, [data-no-drag]")) return;
    onWindowAction("startDrag");
  };

  return (
    <header className="topbar" onMouseDown={startDrag}>
      <div className="brand">
        <BrandMark size={31} className="brand__mark" />
        <span className="brand__word">HORIZON</span>
        <span className="brand__rule" />
        <span className="brand__sub">MEETING HUB</span>
      </div>

      <div className="topbar__right">
        <div className="status-pill" title={online ? "网络已连接" : "网络离线"}>
          <WifiIcon size={17} className={online ? "" : "is-offline"} />
          <span className={`status-pill__dot ${online ? "" : "is-offline"}`} />
        </div>

        {status.batteryPercent !== null && (
          <div className="status-pill" title={status.charging ? "正在充电" : "电池电量"}>
            <BatteryIcon size={19} percent={status.batteryPercent} charging={status.charging} />
            <span>{status.batteryPercent}%</span>
          </div>
        )}

        <div className="topbar__clock">
          <span className="topbar__date">{date}</span>
          <span className="topbar__time">{time}</span>
        </div>

        <span className="topbar__divider" />

        <button
          className="chrome-button"
          onClick={onOpenSettings}
          title="设置"
          aria-label="打开设置"
        >
          <GearIcon size={19} />
        </button>

        <div className="window-controls" data-no-drag>
          <button
            className="chrome-button chrome-button--window"
            onClick={() => onWindowAction("minimize")}
            title="最小化"
            aria-label="最小化"
          >
            <MinimizeIcon size={16} />
          </button>
          <button
            className="chrome-button chrome-button--window"
            onClick={() => onWindowAction("toggleFullscreen")}
            title={isFullscreen ? "退出全屏 (F11)" : "进入全屏 (F11)"}
            aria-label={isFullscreen ? "退出全屏" : "进入全屏"}
          >
            <FullscreenIcon size={15} />
          </button>
          <button
            className="chrome-button chrome-button--window chrome-button--close"
            onClick={() => onWindowAction("close")}
            title="关闭"
            aria-label="关闭"
          >
            <CloseIcon size={16} />
          </button>
        </div>
      </div>
    </header>
  );
}
