// Minimal CDP caller: evaluate an expression in the launcher webview.
// Usage: node tools/cdp-eval.mjs "<expression>"
const expression = process.argv[2];
const targets = await (await fetch("http://127.0.0.1:9333/json")).json();
const page = targets.find((target) => target.type === "page");
if (!page) {
  console.error("no page target");
  process.exit(1);
}
const socket = new WebSocket(page.webSocketDebuggerUrl);
const done = new Promise((resolve, reject) => {
  const timer = setTimeout(() => reject(new Error("CDP timeout")), 15000);
  socket.addEventListener("open", () => {
    socket.send(
      JSON.stringify({
        id: 1,
        method: "Runtime.evaluate",
        params: { expression, awaitPromise: true, returnByValue: true },
      }),
    );
  });
  socket.addEventListener("message", (event) => {
    const message = JSON.parse(event.data);
    if (message.id !== 1) return;
    clearTimeout(timer);
    resolve(message.result);
    socket.close();
  });
  socket.addEventListener("error", (error) => reject(error));
});
const result = await done;
console.log(JSON.stringify(result, null, 2));
