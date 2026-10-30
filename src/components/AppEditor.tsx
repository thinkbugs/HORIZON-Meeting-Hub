import { useState, type FormEvent } from "react";
import { pickIcon } from "../lib/bridge";
import { MAX_PINNED, type AppEntry } from "../types";
import { EntryIcon } from "./AppIcon";
import { CloseIcon, ImageIcon, TrashIcon } from "./Icons";
import { Modal, Switch } from "./Overlay";

interface AppEditorProps {
  entry: AppEntry;
  isNew: boolean;
  pinnedCount: number;
  onSave: (entry: AppEntry) => void;
  onDelete?: (entry: AppEntry) => void;
  onClose: () => void;
}

export function AppEditor({
  entry,
  isNew,
  pinnedCount,
  onSave,
  onDelete,
  onClose,
}: AppEditorProps) {
  const [name, setName] = useState(entry.name);
  const [args, setArgs] = useState(entry.args ?? "");
  const [icon, setIcon] = useState<string | null | undefined>(entry.icon);
  const [pinned, setPinned] = useState(entry.pinned);
  const [pickingIcon, setPickingIcon] = useState(false);
  const [error, setError] = useState("");

  const pinLimitReached = !entry.pinned && pinnedCount >= MAX_PINNED;
  const primaryLabel = isNew
    ? pinned
      ? "添加到首页"
      : "添加到应用库"
    : "保存更改";

  const chooseIcon = async () => {
    setPickingIcon(true);
    setError("");
    try {
      const selected = await pickIcon();
      if (selected) setIcon(selected);
    } catch (reason) {
      setError(String(reason));
    } finally {
      setPickingIcon(false);
    }
  };

  const submit = (event: FormEvent) => {
    event.preventDefault();
    const trimmed = name.trim();
    if (!trimmed) {
      setError("请为应用填写一个名称");
      return;
    }
    onSave({
      ...entry,
      name: trimmed.slice(0, 24),
      args: args.trim() || undefined,
      icon,
      pinned,
    });
  };

  return (
    <Modal onClose={onClose} className="app-editor" labelledBy="editor-title">
      <header className="app-editor__header">
        <div>
          <p className="section-eyebrow">{isNew ? "NEW APPLICATION" : "APPLICATION DETAILS"}</p>
          <h2 id="editor-title">{isNew ? primaryLabel : "编辑应用"}</h2>
        </div>
        <button className="icon-button modal-close" onClick={onClose} aria-label="关闭">
          <CloseIcon size={20} />
        </button>
      </header>

      <form onSubmit={submit} className="app-editor__form">
        <div className="app-editor__preview">
          <div className="editor-preview__ambient" />
          <EntryIcon entry={{ ...entry, name, icon }} glyphSize={48} />
          <div className="editor-preview__copy">
            <span>{name.trim() || "应用名称"}</span>
            <small>{entry.builtin ? "HORIZON 系统应用" : compactPath(entry.path)}</small>
          </div>
          {!entry.builtin && (
            <button
              type="button"
              className="button button--quiet editor-preview__icon-button"
              onClick={chooseIcon}
              disabled={pickingIcon}
            >
              <ImageIcon size={15} />
              {pickingIcon ? "正在读取…" : "更换图标"}
            </button>
          )}
        </div>

        <label className="field">
          <span className="field__label">
            应用名称
            <small>{name.length}/24</small>
          </span>
          <input
            value={name}
            maxLength={24}
            onChange={(event) => setName(event.target.value)}
            placeholder="例如：腾讯会议"
            autoFocus
          />
        </label>

        {entry.builtin ? (
          <div className="field">
            <span className="field__label">系统能力</span>
            <div className="field__readonly">HORIZON 内置 · 无需配置</div>
          </div>
        ) : (
          <>
            <div className="field">
              <span className="field__label">应用路径</span>
              <div className="field__readonly field__readonly--path" title={entry.path}>
                {entry.path}
              </div>
            </div>
            <label className="field">
              <span className="field__label">
                启动参数
                <small>可选</small>
              </span>
              <input
                value={args}
                onChange={(event) => setArgs(event.target.value)}
                placeholder="例如：--kiosk"
                spellCheck={false}
              />
            </label>
          </>
        )}

        <div className="setting-row setting-row--boxed">
          <div>
            <strong>固定到首页</strong>
            <p>固定后可从主屏快速启动，拖动设置中的快捷卡片可调整顺序。</p>
          </div>
          <Switch
            checked={pinned}
            onChange={(next) => {
              if (next && pinLimitReached) {
                setError(`首页最多固定 ${MAX_PINNED} 个应用，请先取消一个`);
                return;
              }
              setError("");
              setPinned(next);
            }}
            label="固定到首页"
          />
        </div>

        {error && <p className="form-error">{error}</p>}

        <footer className="app-editor__footer">
          {!isNew && !entry.builtin && onDelete ? (
            <button
              type="button"
              className="button button--danger-ghost"
              onClick={() => onDelete(entry)}
            >
              <TrashIcon size={16} />
              移除应用
            </button>
          ) : (
            <span className="app-editor__hint">配置会安全保存在本机</span>
          )}
          <div className="app-editor__actions">
            <button type="button" className="button button--ghost" onClick={onClose}>
              取消
            </button>
            <button type="submit" className="button button--primary">
              {primaryLabel}
            </button>
          </div>
        </footer>
      </form>
    </Modal>
  );
}

function compactPath(path?: string) {
  if (!path) return "本机应用";
  const parts = path.split(/[\\/]/).filter(Boolean);
  return parts.length > 2 ? `…\\${parts.slice(-2).join("\\")}` : path;
}
