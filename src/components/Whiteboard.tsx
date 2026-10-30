import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type ComponentType,
} from "react";
import { Drawnix, boardToImage } from "@drawnix/drawnix";
import type { BoardChangeData } from "@plait-board/react-board";
import { PlaitBoard, getViewBox, setIsFromViewportChange } from "@plait/core";
import type { PlaitElement, PlaitTheme, Viewport } from "@plait/core";
import { isTauri, saveWhiteboard } from "../lib/bridge";
import {
  loadWhiteboardDocument,
  saveWhiteboardDocument,
  type WhiteboardDocument,
} from "../lib/whiteboardStore";
import {
  ChevronLeftIcon,
  DownloadIcon,
  GridIcon,
  RedoIcon,
  TrashIcon,
  UndoIcon,
} from "./Icons";
import { ConfirmDialog } from "./Overlay";

interface WhiteboardProps {
  onExit: () => void;
  onNotify: (text: string, tone?: "success" | "error" | "info") => void;
}

type SaveState = "saved" | "saving" | "error";
type HistoryState = { undo: boolean; redo: boolean };

const EMPTY_DOCUMENT: WhiteboardDocument = { children: [] };
const AUTOSAVE_DELAY = 520;
const RESTORE_MARGIN = 96;

// Plait sizes its scroll container after the first paint, so restoring during
// initialization clamps to scroll 0. Re-apply once layout is measurable.
function restoreViewport(board: PlaitBoard, viewport: Viewport) {
  const origination = viewport.origination;
  if (!origination) return;
  const zoom = viewport.zoom;
  let attempts = 0;

  const apply = () => {
    const container = PlaitBoard.getViewportContainer(board);
    const host = PlaitBoard.getHost(board);
    if (!container || !host) return;
    const ready =
      container.clientWidth > 0 &&
      container.clientHeight > 0 &&
      container.scrollWidth > container.clientWidth + 1;
    if (!ready) {
      attempts += 1;
      if (attempts < 45) requestAnimationFrame(apply);
      return;
    }

    const box = getViewBox(board);
    if (!box || box.width <= 0 || box.height <= 0) return;
    const width = container.clientWidth / zoom;
    const height = container.clientHeight / zoom;
    const margin = RESTORE_MARGIN / Math.max(zoom, 0.01);
    const left = Math.min(box.x, origination[0] - margin);
    const top = Math.min(box.y, origination[1] - margin);
    const right = Math.max(box.x + box.width, origination[0] + width + margin);
    const bottom = Math.max(box.y + box.height, origination[1] + height + margin);

    if (left !== box.x || top !== box.y || right !== box.x + box.width || bottom !== box.y + box.height) {
      host.setAttribute("viewBox", `${left} ${top} ${right - left} ${bottom - top}`);
      host.style.width = `${(right - left) * zoom}px`;
      host.style.height = `${(bottom - top) * zoom}px`;
    }

    setIsFromViewportChange(board, false);
    container.scrollLeft = (origination[0] - left) * zoom;
    container.scrollTop = (origination[1] - top) * zoom;
    container.dispatchEvent(new Event("scroll"));
  };

  requestAnimationFrame(apply);
}

interface CanvasProps {
  value: PlaitElement[];
  viewport?: Viewport;
  theme?: PlaitTheme;
  onChange?: (value: BoardChangeData) => void;
  afterInit?: (board: PlaitBoard) => void;
}

// DrawnixProps widens onChange with the native div handler type, so narrow it here.
const Canvas = Drawnix as unknown as ComponentType<CanvasProps>;

export function Whiteboard({ onExit, onNotify }: WhiteboardProps) {
  const boardRef = useRef<PlaitBoard | null>(null);
  const pendingDocument = useRef<WhiteboardDocument | null>(null);
  const persistTimer = useRef<number | null>(null);
  const aliveRef = useRef(true);
  const spacePanRef = useRef(false);

  const [initialDocument, setInitialDocument] = useState<WhiteboardDocument | null>(null);
  const [hasContent, setHasContent] = useState(false);
  const [history, setHistory] = useState<HistoryState>({ undo: false, redo: false });
  const [saveState, setSaveState] = useState<SaveState>("saved");
  const [grid, setGrid] = useState(() => {
    try {
      return localStorage.getItem("horizon.whiteboard.grid") !== "off";
    } catch {
      return true;
    }
  });
  const [exporting, setExporting] = useState(false);
  const [pendingClear, setPendingClear] = useState(false);

  const flushSave = useCallback(async () => {
    const document = pendingDocument.current;
    if (!document) return;
    pendingDocument.current = null;
    if (aliveRef.current) setSaveState("saving");
    try {
      await saveWhiteboardDocument(document);
      if (aliveRef.current && !pendingDocument.current) setSaveState("saved");
    } catch {
      if (aliveRef.current) setSaveState("error");
    }
  }, []);

  useEffect(() => {
    let active = true;
    loadWhiteboardDocument()
      .then((document) => {
        if (!active) return;
        const resolved = document ?? EMPTY_DOCUMENT;
        setInitialDocument(resolved);
        setHasContent(resolved.children.length > 0);
      })
      .catch(() => {
        if (active) setInitialDocument(EMPTY_DOCUMENT);
      });
    return () => {
      active = false;
    };
  }, []);

  useEffect(() => {
    aliveRef.current = true;
    return () => {
      aliveRef.current = false;
      if (persistTimer.current) window.clearTimeout(persistTimer.current);
      persistTimer.current = null;
      const document = pendingDocument.current;
      pendingDocument.current = null;
      if (document) void saveWhiteboardDocument(document);
    };
  }, []);

  const queueSave = useCallback(
    (document: WhiteboardDocument) => {
      pendingDocument.current = document;
      if (persistTimer.current) window.clearTimeout(persistTimer.current);
      persistTimer.current = window.setTimeout(() => {
        persistTimer.current = null;
        void flushSave();
      }, AUTOSAVE_DELAY);
    },
    [flushSave],
  );

  const handleChange = useCallback(
    (data: BoardChangeData) => {
      queueSave({
        children: data.children,
        viewport: data.viewport,
        theme: data.theme,
      });
      const board = boardRef.current;
      if (board) {
        setHistory((previous) => {
          const undo = board.history.undos.length > 0;
          const redo = board.history.redos.length > 0;
          return previous.undo === undo && previous.redo === redo
            ? previous
            : { undo, redo };
        });
      }
      const nextHasContent = data.children.length > 0;
      setHasContent((previous) => (previous === nextHasContent ? previous : nextHasContent));
    },
    [queueSave],
  );

  const handleBoardReady = useCallback(
    (board: PlaitBoard) => {
      boardRef.current = board;
      setHistory({
        undo: board.history.undos.length > 0,
        redo: board.history.redos.length > 0,
      });
      const saved = initialDocument?.viewport;
      if (saved) restoreViewport(board, saved);
    },
    [initialDocument],
  );

  const syncHistory = useCallback(() => {
    const board = boardRef.current;
    if (!board) return;
    setHistory({
      undo: board.history.undos.length > 0,
      redo: board.history.redos.length > 0,
    });
  }, []);

  const undo = () => {
    boardRef.current?.undo();
    window.setTimeout(syncHistory, 0);
  };

  const redo = () => {
    boardRef.current?.redo();
    window.setTimeout(syncHistory, 0);
  };

  const exportImage = useCallback(async () => {
    const board = boardRef.current;
    if (!board || !hasContent) {
      onNotify("画布还是空的，先写下一些想法吧", "info");
      return;
    }
    if (exporting) return;
    setExporting(true);
    try {
      const dataUrl = await boardToImage(board, {
        fillStyle: "#f5f3ed",
        padding: 48,
        ratio: 2,
      });
      if (!dataUrl) throw new Error("无法生成画布图片");
      const stamp = new Date();
      const pad = (value: number) => String(value).padStart(2, "0");
      const fileName = `HORIZON白板_${stamp.getFullYear()}${pad(stamp.getMonth() + 1)}${pad(
        stamp.getDate(),
      )}_${pad(stamp.getHours())}${pad(stamp.getMinutes())}.png`;
      const savedPath = await saveWhiteboard(dataUrl, fileName);
      if (savedPath) {
        onNotify(`白板已保存至 ${savedPath}`, "success");
      } else if (!isTauri) {
        onNotify("白板图片已导出", "success");
      }
    } catch (reason) {
      onNotify(`保存失败：${String(reason)}`, "error");
    } finally {
      setExporting(false);
    }
  }, [exporting, hasContent, onNotify]);

  useEffect(() => {
    const handleShortcut = (event: KeyboardEvent) => {
      if (!(event.ctrlKey || event.metaKey)) return;
      const target = event.target as HTMLElement | null;
      const typing =
        !!target &&
        (target instanceof HTMLInputElement ||
          target instanceof HTMLTextAreaElement ||
          target.isContentEditable);
      const key = event.key.toLowerCase();
      if (key === "s" || (key === "e" && event.shiftKey)) {
        event.preventDefault();
        event.stopImmediatePropagation();
        void exportImage();
        return;
      }
      if (!typing && (key === "z" || key === "y")) {
        event.preventDefault();
        event.stopImmediatePropagation();
        if (key === "y" || event.shiftKey) {
          boardRef.current?.redo();
        } else {
          boardRef.current?.undo();
        }
        window.setTimeout(syncHistory, 0);
        return;
      }
      if ((event.key === "Backspace" || event.key === "Delete") && !typing) {
        event.preventDefault();
        event.stopImmediatePropagation();
        setPendingClear(true);
      }
    };
    window.addEventListener("keydown", handleShortcut, true);
    return () => window.removeEventListener("keydown", handleShortcut, true);
  }, [exportImage, syncHistory]);

  // Plait forwards Space to the board only while it has focus, so bridge the
  // unfocused case into the engine's own hand-mode handling.
  useEffect(() => {
    if (pendingClear) return;

    const handleDown = (event: KeyboardEvent) => {
      if (event.code !== "Space" || event.ctrlKey || event.metaKey || event.altKey) return;
      const target = event.target as HTMLElement | null;
      const typing =
        !!target &&
        (target instanceof HTMLInputElement ||
          target instanceof HTMLTextAreaElement ||
          target.isContentEditable);
      if (typing || spacePanRef.current) return;
      const board = boardRef.current;
      if (!board || PlaitBoard.isFocus(board) || PlaitBoard.hasBeenTextEditing(board)) return;
      spacePanRef.current = true;
      board.keyDown(event);
    };

    const handleUp = (event: KeyboardEvent) => {
      if (event.code !== "Space" || !spacePanRef.current) return;
      spacePanRef.current = false;
      boardRef.current?.keyUp(event);
    };

    window.addEventListener("keydown", handleDown, true);
    window.addEventListener("keyup", handleUp, true);
    return () => {
      window.removeEventListener("keydown", handleDown, true);
      window.removeEventListener("keyup", handleUp, true);
      if (spacePanRef.current) {
        spacePanRef.current = false;
        boardRef.current?.keyUp(new KeyboardEvent("keyup", { code: "Space" }));
      }
    };
  }, [pendingClear]);

  const requestExit = () => {
    if (persistTimer.current) {
      window.clearTimeout(persistTimer.current);
      persistTimer.current = null;
    }
    const document = pendingDocument.current;
    pendingDocument.current = null;
    if (document) void saveWhiteboardDocument(document);
    onExit();
  };

  const toggleGrid = () => {
    const next = !grid;
    setGrid(next);
    try {
      localStorage.setItem("horizon.whiteboard.grid", next ? "on" : "off");
    } catch {
      // A blocked storage backend should not break the canvas.
    }
  };

  const clearBoard = () => {
    const board = boardRef.current;
    if (!board || board.children.length === 0) return;
    board.deleteFragment(board.children);
    setPendingClear(false);
    onNotify("画布已清空，可按撤销恢复", "info");
  };

  return (
    <div className="whiteboard">
      <header className="wb-topbar">
        <button className="wb-back" onClick={requestExit}>
          <ChevronLeftIcon size={18} />
          <span>返回主页</span>
        </button>

        <div className="wb-title">
          <span className="wb-title__dot" />
          <div>
            <strong>智能白板</strong>
            <small>INFINITE CANVAS</small>
          </div>
        </div>

        <div className="wb-actions">
          {hasContent && (
            <span className="wb-autosave" data-state={saveState}>
              <span className="wb-autosave__dot" />
              {saveState === "saving"
                ? "保存中"
                : saveState === "error"
                  ? "保存失败"
                  : "已自动保存"}
            </span>
          )}

          <div className="wb-action-group">
            <button
              className="wb-icon-button"
              onClick={undo}
              disabled={!history.undo}
              title="撤销 (Ctrl+Z)"
              aria-label="撤销"
            >
              <UndoIcon size={18} />
            </button>
            <button
              className="wb-icon-button"
              onClick={redo}
              disabled={!history.redo}
              title="重做 (Ctrl+Shift+Z)"
              aria-label="重做"
            >
              <RedoIcon size={18} />
            </button>
          </div>

          <button
            className={`wb-pill-button ${grid ? "is-active" : ""}`}
            onClick={toggleGrid}
            title="显示或隐藏点阵背景"
            aria-pressed={grid}
          >
            <GridIcon size={16} />
            点阵
          </button>
          <button
            className="wb-pill-button wb-pill-button--clear"
            onClick={() => setPendingClear(true)}
            disabled={!hasContent}
            title="清空画布"
          >
            <TrashIcon size={16} />
            清空
          </button>
          <button
            className="wb-save-button"
            onClick={() => void exportImage()}
            disabled={exporting || !hasContent}
            title="导出 PNG 图片 (Ctrl+S)"
          >
            <DownloadIcon size={16} />
            {exporting ? "正在导出…" : "保存为图片"}
          </button>
        </div>
      </header>

      <div className={`wb-surface ${grid ? "has-grid" : ""}`}>
        {initialDocument ? (
          <div className="wb-board">
            <Canvas
              value={initialDocument.children}
              viewport={initialDocument.viewport ? { zoom: initialDocument.viewport.zoom } : undefined}
              theme={initialDocument.theme}
              onChange={handleChange}
              afterInit={handleBoardReady}
            />

            {!hasContent && (
              <div className="wb-empty">
                <span className="wb-empty__eyebrow">HORIZON WHITEBOARD</span>
                <h2>把想法，直接画出来</h2>
                <p>手绘、文字、思维导图、流程图与图片，都在一块无限画布上完成。双击文字即可编辑。</p>
                <div className="wb-empty__chips">
                  <span><b>P</b> 画笔</span>
                  <span><b>T</b> 文字</span>
                  <span><b>M</b> 思维导图</span>
                  <span><b>R</b> 形状</span>
                  <span><b>A</b> 箭头</span>
                </div>
              </div>
            )}

            <div className="wb-canvas-hint">
              <span>H / 空格 平移</span>
              <i />
              <span>Ctrl + 滚轮 缩放</span>
              <i />
              <span>双击文字 编辑</span>
              <i />
              <span>V 选择 · P 画笔</span>
            </div>

            <span className="wb-watermark">HORIZON · INFINITE CANVAS</span>
          </div>
        ) : (
          <div className="wb-loading">
            <span className="wb-loading__ring" />
            正在准备画布…
          </div>
        )}
      </div>

      {pendingClear && (
        <ConfirmDialog
          title="清空画布？"
          description="画布上的全部内容将被移除。清空后仍可立即使用撤销恢复。"
          confirmLabel="清空画布"
          cancelLabel="取消"
          destructive
          onConfirm={clearBoard}
          onCancel={() => setPendingClear(false)}
        />
      )}
    </div>
  );
}
