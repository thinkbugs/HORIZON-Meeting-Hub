import type { AppEntry } from "../types";
import { AllAppsIcon, EntryIcon } from "./AppIcon";
import { AppsGlyph } from "./Icons";

interface DockProps {
  entries: AppEntry[];
  focusIndex: number;
  onFocus: (index: number) => void;
  onLaunch: (entry: AppEntry) => void;
  onOpenAll: () => void;
  onOpenSettings: () => void;
}

export function Dock({
  entries,
  focusIndex,
  onFocus,
  onLaunch,
  onOpenAll,
  onOpenSettings,
}: DockProps) {
  const pinned = entries.filter((entry) => entry.pinned);
  const itemCount = pinned.length + 1;

  return (
    <section className="dock-region" aria-label="快捷启动">
      <div className="dock">
        <div className="dock__lintel" />
        {pinned.map((entry, index) => {
          const focused = focusIndex === index;
          return (
            <button
              key={entry.id}
              className={`dock-card accent-${entry.accent} ${focused ? "is-focused" : ""}`}
              style={{ animationDelay: `${120 + index * 55}ms` }}
              onMouseEnter={() => onFocus(index)}
              onFocus={() => onFocus(index)}
              onClick={() => onLaunch(entry)}
              aria-label={`打开 ${entry.name}`}
            >
              <span className="dock-card__halo" />
              <EntryIcon entry={entry} glyphSize={36} />
              <span className="dock-card__name">{entry.name}</span>
              <span className="dock-card__focus-dot" />
            </button>
          );
        })}

        <button
          className={`dock-card dock-card--all ${focusIndex === itemCount - 1 ? "is-focused" : ""}`}
          style={{ animationDelay: `${120 + pinned.length * 55}ms` }}
          onMouseEnter={() => onFocus(itemCount - 1)}
          onFocus={() => onFocus(itemCount - 1)}
          onClick={onOpenAll}
          aria-label="查看全部应用"
        >
          <span className="dock-card__halo" />
          <AllAppsIcon />
          <span className="dock-card__name">全部应用</span>
          <span className="dock-card__count">
            <AppsGlyph size={13} />
            {entries.length}
          </span>
          <span className="dock-card__focus-dot" />
        </button>
      </div>

      <div className="dock-hint">
        <span>
          <kbd>←</kbd>
          <kbd>→</kbd>
          选择
        </span>
        <span className="dock-hint__sep" />
        <span>
          <kbd>Enter</kbd>
          打开
        </span>
        <span className="dock-hint__sep" />
        <button className="dock-hint__action" onClick={onOpenSettings}>
          自定义快捷应用
          <span>+</span>
        </button>
      </div>
    </section>
  );
}
