import { useMemo, useState } from "react";
import type { AppEntry } from "../types";
import { EntryIcon } from "./AppIcon";
import { CloseIcon, PinIcon, PlusIcon, SearchIcon, SlidersIcon } from "./Icons";
import { Modal } from "./Overlay";

type Filter = "all" | "system" | "local";

interface AllAppsProps {
  entries: AppEntry[];
  onLaunch: (entry: AppEntry) => void;
  onTogglePin: (entry: AppEntry) => void;
  onImport: () => void;
  onOpenSettings: () => void;
  onClose: () => void;
}

export function AllApps({
  entries,
  onLaunch,
  onTogglePin,
  onImport,
  onOpenSettings,
  onClose,
}: AllAppsProps) {
  const [query, setQuery] = useState("");
  const [filter, setFilter] = useState<Filter>("all");

  const filtered = useMemo(() => {
    const needle = query.trim().toLocaleLowerCase();
    return entries.filter((entry) => {
      if (filter === "system" && !entry.builtin) return false;
      if (filter === "local" && entry.builtin) return false;
      if (!needle) return true;
      return `${entry.name} ${entry.path ?? ""}`.toLocaleLowerCase().includes(needle);
    });
  }, [entries, filter, query]);

  return (
    <Modal onClose={onClose} className="all-apps-modal" labelledBy="all-apps-title">
      <header className="all-apps__header">
        <div>
          <p className="section-eyebrow">APPLICATION LIBRARY</p>
          <h2 id="all-apps-title">
            全部应用
            <span>{entries.length}</span>
          </h2>
        </div>
        <div className="all-apps__search">
          <SearchIcon size={18} />
          <input
            autoFocus
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            placeholder="搜索应用名称或路径"
            spellCheck={false}
            aria-label="搜索应用"
          />
          {query && (
            <button className="search-clear" onClick={() => setQuery("")} aria-label="清空搜索">
              <CloseIcon size={14} />
            </button>
          )}
          <kbd>Esc</kbd>
        </div>
        <button className="icon-button modal-close" onClick={onClose} aria-label="关闭">
          <CloseIcon size={20} />
        </button>
      </header>

      <div className="all-apps__toolbar">
        <div className="filter-tabs" role="tablist" aria-label="筛选应用">
          <button
            role="tab"
            aria-selected={filter === "all"}
            className={filter === "all" ? "is-active" : ""}
            onClick={() => setFilter("all")}
          >
            全部
          </button>
          <button
            role="tab"
            aria-selected={filter === "system"}
            className={filter === "system" ? "is-active" : ""}
            onClick={() => setFilter("system")}
          >
            系统应用
          </button>
          <button
            role="tab"
            aria-selected={filter === "local"}
            className={filter === "local" ? "is-active" : ""}
            onClick={() => setFilter("local")}
          >
            本机应用
          </button>
        </div>
        <span className="all-apps__result-count">{filtered.length} 个应用</span>
      </div>

      <div className="all-apps__body">
        {filtered.length === 0 && query ? (
          <div className="empty-state">
            <div className="empty-state__orb">
              <SearchIcon size={25} />
            </div>
            <h3>没有找到「{query}」</h3>
            <p>换个关键词试试，或从本机添加一个新应用。</p>
            <button className="button button--ghost" onClick={onImport}>
              <PlusIcon size={16} />
              添加本机应用
            </button>
          </div>
        ) : (
          <div className="app-grid">
            {filtered.map((entry, index) => (
              <div
                key={entry.id}
                className={`app-tile accent-${entry.accent}`}
                role="button"
                tabIndex={0}
                style={{ animationDelay: `${index * 28}ms` }}
                onClick={() => onLaunch(entry)}
                onKeyDown={(event) => {
                  if (event.key === "Enter" || event.key === " ") {
                    event.preventDefault();
                    onLaunch(entry);
                  }
                }}
                aria-label={`打开 ${entry.name}`}
              >
                <div className="app-tile__top">
                  <EntryIcon entry={entry} glyphSize={30} />
                  <button
                    className={`app-tile__pin ${entry.pinned ? "is-pinned" : ""}`}
                    onClick={(event) => {
                      event.stopPropagation();
                      onTogglePin(entry);
                    }}
                    title={entry.pinned ? "从首页移除" : "固定到首页"}
                    aria-label={entry.pinned ? "从首页移除" : "固定到首页"}
                  >
                    <PinIcon size={15} filled={entry.pinned} />
                  </button>
                </div>
                <div className="app-tile__copy">
                  <strong>{entry.name}</strong>
                  <span>
                    {entry.builtin ? "HORIZON 系统应用" : compactPath(entry.path)}
                  </span>
                </div>
              </div>
            ))}

            {filter !== "system" && !query && (
              <button className="app-tile app-tile--add" onClick={onImport}>
                <span className="app-tile__add-icon">
                  <PlusIcon size={22} />
                </span>
                <span className="app-tile__copy">
                  <strong>添加本机应用</strong>
                  <span>EXE · LNK · BAT · 更多</span>
                </span>
              </button>
            )}
          </div>
        )}
      </div>

      <footer className="all-apps__footer">
        <span>支持从本机选择任意程序，自动提取应用图标。</span>
        <button className="text-button" onClick={onOpenSettings}>
          <SlidersIcon size={15} />
          管理应用与首页
        </button>
      </footer>
    </Modal>
  );
}

function compactPath(path?: string) {
  if (!path) return "本机应用";
  const parts = path.split(/[\\/]/).filter(Boolean);
  if (parts.length <= 2) return path;
  return `…\\${parts.slice(-2).join("\\")}`;
}
