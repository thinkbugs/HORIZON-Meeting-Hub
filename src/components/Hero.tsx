import { useEffect, useState } from "react";
import type { LauncherSettings } from "../types";

interface HeroProps {
  settings: LauncherSettings;
  pinnedCount: number;
}

export function Hero({ settings, pinnedCount }: HeroProps) {
  const [now, setNow] = useState(() => new Date());

  useEffect(() => {
    if (!settings.showScreenInfo) return;
    const interval = window.setInterval(() => setNow(new Date()), 30_000);
    return () => window.clearInterval(interval);
  }, [settings.showScreenInfo]);

  const date = new Intl.DateTimeFormat("zh-CN", {
    month: "long",
    day: "numeric",
    weekday: "long",
  }).format(now);

  // Brand wallpapers already carry their own typography: the block keeps its
  // flex space so the dock does not slide up, and only the text goes away.
  if (!settings.showScreenInfo) {
    return <main className="hero" />;
  }

  const cjk = /[\u3400-\u9fff]/.test(settings.deviceName);
  const long = settings.deviceName.length > 16;

  return (
    <main className="hero">
      <div className="hero__kicker">
        <span className="hero__signal" />
        <span>MEETING READY</span>
        <span className="hero__kicker-cn">会前就绪</span>
      </div>

      <h1 className={`hero__title ${cjk ? "is-cjk" : ""} ${long ? "is-long" : ""}`}>
        {settings.deviceName}
      </h1>
      <p className="hero__tagline">{settings.tagline}</p>

      <div className="hero__meta">
        <span>{date}</span>
        <span className="hero__meta-dot" />
        <span>{pinnedCount} 个快捷入口</span>
        <span className="hero__meta-dot" />
        <span className="hero__local">
          <span className="hero__local-dot" />
          本地模式
        </span>
      </div>
    </main>
  );
}
