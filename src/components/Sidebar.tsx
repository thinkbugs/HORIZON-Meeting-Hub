import { useRef, useState } from "react";
import type { AppEntry } from "../types";
import { EntryIcon } from "./AppIcon";
import { CloseIcon, GripIcon } from "./Icons";

interface SidebarProps {
  entries: AppEntry[];
  position: "left" | "right" | "custom";
  x: number;
  y: number;
  onLaunch: (entry: AppEntry) => void;
  onClose: () => void;
  onPositionChange: (position: "left" | "right" | "custom", x: number, y: number) => void;
}

export function Sidebar({
  entries,
  position,
  x,
  y,
  onLaunch,
  onClose,
  onPositionChange,
}: SidebarProps) {
  const [dragging, setDragging] = useState(false);
  const [dragPos, setDragPos] = useState<{ x: number; y: number } | null>(null);
  const dragState = useRef<{
    offsetX: number;
    offsetY: number;
    moved: boolean;
    pointerId: number;
  } | null>(null);
  const posRef = useRef({ x, y });
  const sidebarRef = useRef<HTMLDivElement>(null);

  const handlePointerDown = (event: React.PointerEvent<HTMLDivElement>) => {
    if (event.button !== 0) return;
    if ((event.target as HTMLElement).closest(".sidebar__close")) return;
    const rect = sidebarRef.current?.getBoundingClientRect();
    if (!rect) return;
    dragState.current = {
      offsetX: event.clientX - rect.left,
      offsetY: event.clientY - rect.top,
      moved: false,
      pointerId: event.pointerId,
    };
    posRef.current = { x: rect.left, y: rect.top };
  };

  const handlePointerMove = (event: React.PointerEvent<HTMLDivElement>) => {
    const state = dragState.current;
    if (!state) return;
    const nextX = Math.min(
      Math.max(0, event.clientX - state.offsetX),
      window.innerWidth - 64,
    );
    const nextY = Math.min(
      Math.max(0, event.clientY - state.offsetY),
      window.innerHeight - 64,
    );
    if (!state.moved && Math.abs(nextX - posRef.current.x) + Math.abs(nextY - posRef.current.y) < 3) {
      return;
    }
    if (!state.moved) {
      state.moved = true;
      // Capture only once this is a real drag. Capturing on pointerdown would
      // retarget the trailing `click` to the container, so the tool buttons
      // would never receive it.
      try {
        sidebarRef.current?.setPointerCapture(state.pointerId);
      } catch {
        // Pointer already released; the drag still works through the element.
      }
      setDragging(true);
    }
    posRef.current = { x: nextX, y: nextY };
    setDragPos({ x: nextX, y: nextY });
  };

  const finishDrag = () => {
    const state = dragState.current;
    dragState.current = null;
    setDragging(false);
    if (!state?.moved) {
      setDragPos(null);
      return;
    }
    const { x: nextX, y: nextY } = posRef.current;
    setDragPos(null);
    let nextPosition: "left" | "right" | "custom" = "custom";
    if (nextX < 120) nextPosition = "left";
    else if (nextX > window.innerWidth - 160) nextPosition = "right";
    onPositionChange(nextPosition, nextX, nextY);
  };

  const anchored = position !== "custom" && !dragPos;
  const style = anchored ? undefined : { left: dragPos?.x ?? x, top: dragPos?.y ?? y };

  return (
    <div
      ref={sidebarRef}
      className={`sidebar sidebar--${anchored ? position : "custom"}${
        dragging ? " is-dragging" : ""
      }`}
      style={style}
      onPointerDown={handlePointerDown}
      onPointerMove={handlePointerMove}
      onPointerUp={finishDrag}
      onPointerCancel={finishDrag}
    >
      <div className="sidebar__handle">
        <GripIcon size={14} />
      </div>
      <button
        className="sidebar__close"
        onClick={onClose}
        aria-label="关闭工具栏"
      >
        <CloseIcon size={14} />
      </button>
      <div className="sidebar__tools">
        {entries.map((entry) => (
          <button
            key={entry.id}
            type="button"
            className="sidebar__tool"
            onClick={() => onLaunch(entry)}
            title={entry.name}
            aria-label={entry.name}
          >
            <EntryIcon entry={entry} glyphSize={20} className="entry-icon--compact" />
          </button>
        ))}
      </div>
    </div>
  );
}
