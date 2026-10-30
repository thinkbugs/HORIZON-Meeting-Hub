import { useEffect, type ReactNode } from "react";
import { CloseIcon, InfoIcon } from "./Icons";

interface ModalProps {
  children: ReactNode;
  onClose: () => void;
  className?: string;
  variant?: "dialog" | "drawer";
  labelledBy?: string;
}

export function Modal({
  children,
  onClose,
  className = "",
  variant = "dialog",
  labelledBy,
}: ModalProps) {
  useEffect(() => {
    const handleKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.stopPropagation();
        onClose();
      }
    };
    window.addEventListener("keydown", handleKey, true);
    return () => window.removeEventListener("keydown", handleKey, true);
  }, [onClose]);

  return (
    <div
      className={`overlay overlay--${variant}`}
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <div
        className={`modal-surface ${className}`}
        role="dialog"
        aria-modal="true"
        aria-labelledby={labelledBy}
      >
        {children}
      </div>
    </div>
  );
}

interface ConfirmDialogProps {
  title: string;
  description: string;
  confirmLabel?: string;
  cancelLabel?: string;
  destructive?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}

export function ConfirmDialog({
  title,
  description,
  confirmLabel = "确认",
  cancelLabel = "取消",
  destructive = false,
  onConfirm,
  onCancel,
}: ConfirmDialogProps) {
  useEffect(() => {
    const handleKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onCancel();
    };
    window.addEventListener("keydown", handleKey);
    return () => window.removeEventListener("keydown", handleKey);
  }, [onCancel]);

  return (
    <div className="overlay overlay--confirm">
      <div className="confirm-card" role="alertdialog" aria-modal="true" aria-labelledby="confirm-title">
        <div className={`confirm-card__icon ${destructive ? "is-danger" : ""}`}>
          <InfoIcon size={23} />
        </div>
        <div className="confirm-card__copy">
          <h3 id="confirm-title">{title}</h3>
          <p>{description}</p>
        </div>
        <button className="icon-button confirm-card__close" onClick={onCancel} aria-label="关闭">
          <CloseIcon size={17} />
        </button>
        <div className="confirm-card__actions">
          <button className="button button--ghost" onClick={onCancel}>
            {cancelLabel}
          </button>
          <button
            className={`button ${destructive ? "button--danger" : "button--primary"}`}
            onClick={onConfirm}
            autoFocus
          >
            {confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
}

export function Switch({
  checked,
  onChange,
  label,
}: {
  checked: boolean;
  onChange: (value: boolean) => void;
  label: string;
}) {
  return (
    <button
      type="button"
      className={`switch ${checked ? "is-on" : ""}`}
      role="switch"
      aria-checked={checked}
      aria-label={label}
      onClick={() => onChange(!checked)}
    >
      <span />
    </button>
  );
}
