import { closeAnnotationWindow } from "./lib/bridge";

const canvas = document.getElementById("canvas") as HTMLCanvasElement;
const ctx = canvas.getContext("2d")!;

function resizeCanvas() {
  canvas.width = window.innerWidth * window.devicePixelRatio;
  canvas.height = window.innerHeight * window.devicePixelRatio;
  canvas.style.width = window.innerWidth + "px";
  canvas.style.height = window.innerHeight + "px";
  ctx.scale(window.devicePixelRatio, window.devicePixelRatio);
}
resizeCanvas();
window.addEventListener("resize", resizeCanvas);

let isDrawing = false;
let currentTool = "pen";
let currentColor = "#ff3b30";
let currentSize = 3;
let strokes: any[] = [];
let currentStroke: any = null;

const penBtn = document.getElementById("pen-btn")!;
const eraserBtn = document.getElementById("eraser-btn")!;

penBtn.addEventListener("click", () => {
  currentTool = "pen";
  penBtn.classList.add("active");
  eraserBtn.classList.remove("active");
});

eraserBtn.addEventListener("click", () => {
  currentTool = "eraser";
  eraserBtn.classList.add("active");
  penBtn.classList.remove("active");
});

document.querySelectorAll(".color-swatch").forEach((swatch) => {
  swatch.addEventListener("click", () => {
    document.querySelectorAll(".color-swatch").forEach((s) => s.classList.remove("active"));
    swatch.classList.add("active");
    currentColor = (swatch as HTMLElement).dataset.color!;
  });
});

const sizeSlider = document.getElementById("size-slider") as HTMLInputElement;
sizeSlider.addEventListener("input", (e) => {
  currentSize = parseInt((e.target as HTMLInputElement).value);
});

function startStroke(x: number, y: number) {
  isDrawing = true;
  currentStroke = {
    tool: currentTool,
    color: currentColor,
    size: currentSize,
    points: [{ x, y }],
  };
}

function continueStroke(x: number, y: number) {
  if (!isDrawing || !currentStroke) return;
  currentStroke.points.push({ x, y });
  drawStroke(currentStroke);
}

function endStroke() {
  if (currentStroke) {
    strokes.push(currentStroke);
    currentStroke = null;
  }
  isDrawing = false;
}

function drawStroke(stroke: any) {
  if (stroke.points.length < 2) return;

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
  ctx.moveTo(stroke.points[0].x, stroke.points[0].y);

  for (let i = 1; i < stroke.points.length; i++) {
    const prev = stroke.points[i - 1];
    const curr = stroke.points[i];
    const midX = (prev.x + curr.x) / 2;
    const midY = (prev.y + curr.y) / 2;
    ctx.quadraticCurveTo(prev.x, prev.y, midX, midY);
  }

  ctx.stroke();
  ctx.restore();
}

function redrawAll() {
  ctx.clearRect(0, 0, canvas.width, canvas.height);
  strokes.forEach((stroke) => drawStroke(stroke));
}

canvas.addEventListener("mousedown", (e) => {
  startStroke(e.offsetX, e.offsetY);
});

canvas.addEventListener("mousemove", (e) => {
  continueStroke(e.offsetX, e.offsetY);
});

canvas.addEventListener("mouseup", endStroke);
canvas.addEventListener("mouseleave", endStroke);

canvas.addEventListener("touchstart", (e) => {
  e.preventDefault();
  const touch = e.touches[0];
  const rect = canvas.getBoundingClientRect();
  startStroke(touch.clientX - rect.left, touch.clientY - rect.top);
});

canvas.addEventListener("touchmove", (e) => {
  e.preventDefault();
  const touch = e.touches[0];
  const rect = canvas.getBoundingClientRect();
  continueStroke(touch.clientX - rect.left, touch.clientY - rect.top);
});

canvas.addEventListener("touchend", (e) => {
  e.preventDefault();
  endStroke();
});

document.getElementById("undo-btn")!.addEventListener("click", () => {
  strokes.pop();
  redrawAll();
});

document.getElementById("clear-btn")!.addEventListener("click", () => {
  strokes = [];
  redrawAll();
});

document.getElementById("save-btn")!.addEventListener("click", () => {
  const dataUrl = canvas.toDataURL("image/png");
  const link = document.createElement("a");
  link.download = `annotation-${Date.now()}.png`;
  link.href = dataUrl;
  link.click();
});

document.getElementById("close-btn")!.addEventListener("click", async () => {
  try {
    await closeAnnotationWindow();
  } catch (error) {
    console.error("Failed to close window:", error);
  }
});

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    document.getElementById("close-btn")!.click();
  } else if (e.ctrlKey && e.key === "z") {
    e.preventDefault();
    document.getElementById("undo-btn")!.click();
  }
});
