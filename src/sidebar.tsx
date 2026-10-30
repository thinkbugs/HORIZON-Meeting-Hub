import { emit } from "@tauri-apps/api/event";
import { openBuiltin } from "./lib/bridge";
import { suppressNativeContextMenu } from "./lib/contextMenu";

suppressNativeContextMenu();

const items = [
  { id: "notepad", icon: "📝", action: "open_builtin", param: "notepad" },
  { id: "screenshot", icon: "📷", action: "open_builtin", param: "screenshot" },
  { id: "screenrec", icon: "🎥", action: "open_builtin", param: "screenrec" },
  { id: "annotation", icon: "✏️", action: "emit_event", param: "sidebar-open-annotation" },
  { id: "whiteboard", icon: "🎨", action: "emit_event", param: "sidebar-open-whiteboard" },
  { id: "office", icon: "📄", action: "open_builtin", param: "office" },
  { id: "filetransfer", icon: "📁", action: "emit_event", param: "sidebar-open-filetransfer" },
];

const container = document.createElement("div");
container.className = "sidebar-container";

items.forEach((item) => {
  const btn = document.createElement("div");
  btn.className = "sidebar-item";
  btn.innerHTML = `<span style="font-size: 24px;">${item.icon}</span>`;
  btn.title = item.id;

  btn.addEventListener("click", async () => {
    try {
      if (item.action === "emit_event") {
        await emit(item.param);
      } else {
        const hint = await openBuiltin(item.param as any);
        if (hint) await emit("sidebar-notify", { text: hint, tone: "info" });
      }
    } catch (error) {
      const text = error instanceof Error ? error.message : String(error);
      await emit("sidebar-notify", { text, tone: "error" });
    }
  });

  container.appendChild(btn);
});

document.getElementById("root")!.appendChild(container);
