import { CheckIcon, InfoIcon } from "./Icons";

export interface ToastMessage {
  id: number;
  text: string;
  tone: "success" | "error" | "info";
}

export function Toast({ toast }: { toast: ToastMessage | null }) {
  if (!toast) return null;
  return (
    <div className={`toast toast--${toast.tone}`} role="status" aria-live="polite" key={toast.id}>
      <span className="toast__icon">
        {toast.tone === "success" ? <CheckIcon size={15} /> : <InfoIcon size={15} />}
      </span>
      <span>{toast.text}</span>
    </div>
  );
}
