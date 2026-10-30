import { createPortal } from "react-dom";
import { CloseIcon } from "./Icons";
import { MirrorSurface } from "./MirrorPreview";
import type { MirrorView } from "./MirrorPreview";

interface CastStageProps {
  title: string;
  detail: string;
  live: boolean;
  /** The 投屏码, shown across the empty stage so the room can pair without a panel. */
  code?: string;
  mirror: MirrorView;
  onMirrorHost: (element: HTMLDivElement | null) => void;
  onClose: () => void;
}

/**
 * Full-window receiver view. It is a portal so the drawer's transform and
 * backdrop filter cannot become the containing block of the overlay. The
 * picture itself belongs to the panel: the stage only lends this box to the
 * canvas the session is already decoding into.
 */
export function CastStage({
  title,
  detail,
  live,
  code,
  mirror,
  onMirrorHost,
  onClose,
}: CastStageProps) {
  // While the pairing code is on screen it owns the empty stage, so the
  // borrowed preview box must stop printing its own waiting text through the
  // digits.
  const waiting = !live && Boolean(code);
  return createPortal(
    <div
      className={`cast-stage${waiting ? " cast-stage--waiting" : ""}`}
      role="dialog"
      aria-modal="true"
      aria-label={`${title}投屏画面`}
    >
      <MirrorSurface view={mirror} variant="stage" onHost={onMirrorHost} />
      {waiting && code && (
        <div className="cast-stage__code">
          <small>投屏码</small>
          <strong>{code.split("").join(" ")}</strong>
          <span>在浏览器扩展里输入这串数字，即可把画面投到这块大屏</span>
        </div>
      )}
      <div className="cast-stage__bar">
        <div className="cast-stage__meta">
          <span className="cast-stage__eyebrow">
            <span className="cast-stage__dot" aria-hidden="true" />
            {live ? "正在接收" : "等待投放"}
          </span>
          <strong>{title}</strong>
          {detail && <span className="cast-stage__detail">{detail}</span>}
        </div>
        <button
          type="button"
          className="cast-stage__exit"
          onClick={onClose}
          aria-label="退出全屏观看"
        >
          <CloseIcon size={15} />
          退出全屏
          <kbd>Esc</kbd>
        </button>
      </div>
    </div>,
    document.body,
  );
}
