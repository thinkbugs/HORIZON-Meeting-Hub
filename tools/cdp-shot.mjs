// Capture the launcher window through CDP. Usage: node tools/cdp-shot.mjs <out.png>
import { writeFileSync } from "node:fs";

const output = process.argv[2] ?? "shot.png";
const targets = await (await fetch("http://127.0.0.1:9333/json")).json();
const page = targets.find((target) => target.type === "page");
if (!page) {
  console.error("no page target");
  process.exit(1);
}
const socket = new WebSocket(page.webSocketDebuggerUrl);
const capture = new Promise((resolve, reject) => {
  const timer = setTimeout(() => reject(new Error("CDP timeout")), 15000);
  socket.addEventListener("open", () => {
    socket.send(JSON.stringify({ id: 1, method: "Page.captureScreenshot", params: { format: "png" } }));
  });
  socket.addEventListener("message", (event) => {
    const message = JSON.parse(event.data);
    if (message.id !== 1) return;
    clearTimeout(timer);
    resolve(message.result?.data ?? "");
    socket.close();
  });
  socket.addEventListener("error", reject);
});
const data = await capture;
if (!data) {
  console.error("empty screenshot");
  process.exit(1);
}
writeFileSync(output, Buffer.from(data, "base64"));
console.log(`${output} ${Buffer.from(data, "base64").length} bytes`);
