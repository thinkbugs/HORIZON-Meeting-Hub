import { useCallback, useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import {
  CloseIcon,
  PenIcon,
  EraserIcon,
  UndoIcon,
  TrashIcon,
  DownloadIcon,
} from "./Icons";

interface AnnotationProps {
  onExit: () => void;
  onNotify: (text: string, tone?: "success" | "error" | "info") => void;
}

type Tool = "pen" | "eraser";

interface Point {
  x: number;
  y: number;
}

interface Stroke {
  points: Point[];
  color: string;
  size: number;
  tool: Tool;
}

const PRESET_COLORS = [
  { name: "白色", value: "#ffffff" },
  { name: "红色", value: "#ff4757" },
  { name: "黄色", value: "#ffa502" },
  { name: "绿色", value: "#2ed573" },
  { name: "青色", value: "#1e90ff" },
  { name: "蓝色", value: "#5352ed" },
];

export function Annotation({ onExit, onNotify }: AnnotationProps) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const containerRef = useRef<HTMLDivElement>(null);
  const strokesRef = useRef<Stroke[]>([]);
  const currentStrokeRef = useRef<Stroke | null>(null);
  const isDrawingRef = useRef(false);

  const [tool, setTool] = useState<Tool>("pen");
  const [color, setColor] = useState(PRESET_COLORS[0].value);
  const [size, setSize] = useState(4);
  const [, forceUpdate] = useState({});

  const drawStroke = useCallback(
    (ctx: CanvasRenderingContext2D, stroke: Stroke) => {
      if (stroke.points.length === 0) return;

      ctx.save();
      ctx.lineCap = "round";
      ctx.lineJoin = "round";
      ctx.lineWidth = stroke.size;

      if (stroke.tool === "eraser") {
        ctx.globalCompositeOperation = "destination-out";
        ctx.strokeStyle = "rgba(0, 0, 0, 1)";
      } else {
        ctx.globalCompositeOperation = "source-over";
        ctx.strokeStyle = stroke.color;
      }

      ctx.beginPath();
      const [first, ...rest] = stroke.points;
      ctx.moveTo(first.x, first.y);

      if (rest.length === 1) {
        ctx.lineTo(rest[0].x, rest[0].y);
      } else {
        for (let i = 0; i < rest.length - 1; i++) {
          const current = rest[i];
          const next = rest[i + 1];
          const midX = (current.x + next.x) / 2;
          const midY = (current.y + next.y) / 2;
          ctx.quadraticCurveTo(current.x, current.y, midX, midY);
        }
        const last = rest[rest.length - 1];
        ctx.lineTo(last.x, last.y);
      }

      ctx.stroke();
      ctx.restore();
    },
    [],
  );

  const redrawCanvas = useCallback(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;

    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    const dpr = window.devicePixelRatio || 1;

    // Reset transform to identity so clearRect covers exact pixel bounds
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.clearRect(0, 0, canvas.width, canvas.height);

    // Apply DPR scaling so all drawing uses CSS-pixel coordinates
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);

    for (const stroke of strokesRef.current) {
      drawStroke(ctx, stroke);
    }

    if (currentStrokeRef.current) {
      drawStroke(ctx, currentStrokeRef.current);
    }
  }, [drawStroke]);

  const getCanvasCoordinates = useCallback(
    (event: MouseEvent | TouchEvent): Point | null => {
      const canvas = canvasRef.current;
      if (!canvas) return null;

      const rect = canvas.getBoundingClientRect();
      let clientX: number;
      let clientY: number;

      if ("touches" in event) {
        if (event.touches.length === 0) return null;
        clientX = event.touches[0].clientX;
        clientY = event.touches[0].clientY;
      } else {
        clientX = event.clientX;
        clientY = event.clientY;
      }

      return {
        x: clientX - rect.left,
        y: clientY - rect.top,
      };
    },
    [],
  );

  const handlePointerDown = useCallback(
    (event: MouseEvent | TouchEvent) => {
      event.preventDefault();
      const point = getCanvasCoordinates(event);
      if (!point) return;

      isDrawingRef.current = true;
      currentStrokeRef.current = {
        points: [point],
        color,
        size: tool === "eraser" ? size * 3 : size,
        tool,
      };
    },
    [color, size, tool, getCanvasCoordinates],
  );

  const handlePointerMove = useCallback(
    (event: MouseEvent | TouchEvent) => {
      if (!isDrawingRef.current || !currentStrokeRef.current) return;
      event.preventDefault();

      const point = getCanvasCoordinates(event);
      if (!point) return;

      currentStrokeRef.current.points.push(point);
      redrawCanvas();
    },
    [getCanvasCoordinates, redrawCanvas],
  );

  const handlePointerUp = useCallback(() => {
    if (!isDrawingRef.current || !currentStrokeRef.current) return;

    isDrawingRef.current = false;
    strokesRef.current.push(currentStrokeRef.current);
    currentStrokeRef.current = null;
    redrawCanvas();
    forceUpdate({});
  }, [redrawCanvas]);

  const handleUndo = useCallback(() => {
    if (strokesRef.current.length === 0) return;
    strokesRef.current.pop();
    redrawCanvas();
    forceUpdate({});
    onNotify("已撤销上一笔", "info");
  }, [redrawCanvas, onNotify]);

  const handleClear = useCallback(() => {
    if (strokesRef.current.length === 0) return;
    strokesRef.current = [];
    redrawCanvas();
    forceUpdate({});
    onNotify("画布已清空", "info");
  }, [redrawCanvas, onNotify]);

  const handleSave = useCallback(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;

    if (strokesRef.current.length === 0) {
      onNotify("画布是空的，无法保存", "info");
      return;
    }

    try {
      const dataUrl = canvas.toDataURL("image/png");
      const link = document.createElement("a");
      const timestamp = new Date();
      const pad = (n: number) => String(n).padStart(2, "0");
      const fileName = `HORIZON标注_${timestamp.getFullYear()}${pad(timestamp.getMonth() + 1)}${pad(timestamp.getDate())}_${pad(timestamp.getHours())}${pad(timestamp.getMinutes())}.png`;

      link.download = fileName;
      link.href = dataUrl;
      link.click();

      onNotify("标注已保存", "success");
    } catch (error) {
      onNotify(`保存失败: ${String(error)}`, "error");
    }
  }, [onNotify]);

  const handleExit = useCallback(() => {
    onExit();
  }, [onExit]);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;

    const resizeCanvas = () => {
      const container = containerRef.current;
      if (!container) return;

      const dpr = window.devicePixelRatio || 1;
      const width = container.clientWidth;
      const height = container.clientHeight;

      canvas.width = width * dpr;
      canvas.height = height * dpr;
      canvas.style.width = `${width}px`;
      canvas.style.height = `${height}px`;

      redrawCanvas();
    };

    resizeCanvas();
    window.addEventListener("resize", resizeCanvas);

    return () => {
      window.removeEventListener("resize", resizeCanvas);
    };
  }, [redrawCanvas]);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;

    const mouseDown = (e: MouseEvent) => handlePointerDown(e);
    const mouseMove = (e: MouseEvent) => handlePointerMove(e);
    const mouseUp = () => handlePointerUp();
    const touchStart = (e: TouchEvent) => handlePointerDown(e);
    const touchMove = (e: TouchEvent) => handlePointerMove(e);
    const touchEnd = () => handlePointerUp();

    canvas.addEventListener("mousedown", mouseDown);
    canvas.addEventListener("mousemove", mouseMove);
    window.addEventListener("mouseup", mouseUp);
    canvas.addEventListener("touchstart", touchStart, { passive: false });
    canvas.addEventListener("touchmove", touchMove, { passive: false });
    window.addEventListener("touchend", touchEnd);

    return () => {
      canvas.removeEventListener("mousedown", mouseDown);
      canvas.removeEventListener("mousemove", mouseMove);
      window.removeEventListener("mouseup", mouseUp);
      canvas.removeEventListener("touchstart", touchStart);
      canvas.removeEventListener("touchmove", touchMove);
      window.removeEventListener("touchend", touchEnd);
    };
  }, [handlePointerDown, handlePointerMove, handlePointerUp]);

  useEffect(() => {
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        handleExit();
        return;
      }

      if ((event.ctrlKey || event.metaKey) && event.key === "z") {
        event.preventDefault();
        handleUndo();
      }
    };

    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [handleExit, handleUndo]);

  const hasStrokes = strokesRef.current.length > 0;

  return createPortal(
    <div className="annotation-overlay">
      <div className="annotation-container" ref={containerRef}>
        <canvas
          ref={canvasRef}
          className="annotation-canvas"
          style={{
            cursor:
              tool === "pen"
                ? "crosshair"
                : "url('data:image/svg+xml,<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"24\" height=\"24\" viewBox=\"0 0 24 24\"><circle cx=\"12\" cy=\"12\" r=\"8\" fill=\"none\" stroke=\"white\" stroke-width=\"2\"/></svg>') 12 12, auto",
          }}
        />

        <div className="annotation-toolbar">
          <div className="annotation-tool-group">
            <button
              className={`annotation-tool-button ${tool === "pen" ? "is-active" : ""}`}
              onClick={() => setTool("pen")}
              title="画笔工具"
              aria-label="画笔工具"
            >
              <PenIcon size={18} />
            </button>
            <button
              className={`annotation-tool-button ${tool === "eraser" ? "is-active" : ""}`}
              onClick={() => setTool("eraser")}
              title="橡皮擦工具"
              aria-label="橡皮擦工具"
            >
              <EraserIcon size={18} />
            </button>
          </div>

          <div className="annotation-divider" />

          <div className="annotation-tool-group">
            <div className="annotation-color-picker">
              {PRESET_COLORS.map((preset) => (
                <button
                  key={preset.value}
                  className={`annotation-color-swatch ${color === preset.value ? "is-active" : ""}`}
                  style={{ backgroundColor: preset.value }}
                  onClick={() => setColor(preset.value)}
                  title={preset.name}
                  aria-label={preset.name}
                />
              ))}
            </div>
          </div>

          <div className="annotation-divider" />

          <div className="annotation-tool-group annotation-size-control">
            <label className="annotation-size-label">
              <span>粗细</span>
              <input
                type="range"
                min="1"
                max="20"
                value={size}
                onChange={(e) => setSize(Number(e.target.value))}
                className="annotation-size-slider"
              />
              <span className="annotation-size-value">{size}px</span>
            </label>
          </div>

          <div className="annotation-divider" />

          <div className="annotation-tool-group">
            <button
              className="annotation-action-button"
              onClick={handleUndo}
              disabled={!hasStrokes}
              title="撤销 (Ctrl+Z)"
              aria-label="撤销"
            >
              <UndoIcon size={18} />
            </button>
            <button
              className="annotation-action-button"
              onClick={handleClear}
              disabled={!hasStrokes}
              title="清空画布"
              aria-label="清空画布"
            >
              <TrashIcon size={18} />
            </button>
          </div>

          <div className="annotation-divider" />

          <div className="annotation-tool-group">
            <button
              className="annotation-save-button"
              onClick={handleSave}
              disabled={!hasStrokes}
              title="保存为 PNG"
            >
              <DownloadIcon size={16} />
              <span>保存</span>
            </button>
          </div>

          <div className="annotation-divider" />

          <div className="annotation-tool-group">
            <button
              className="annotation-close-button"
              onClick={handleExit}
              title="退出标注 (Esc)"
              aria-label="退出标注"
            >
              <CloseIcon size={18} />
            </button>
          </div>
        </div>
      </div>
    </div>,
    document.body,
  );
}
