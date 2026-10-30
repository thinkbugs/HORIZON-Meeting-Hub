const $ = (id) => document.getElementById(id);
let view = { phase: "idle", detail: null };
let busy = false;
let localError = null;
let receivers = [];
let selected = null;
let scanning = false;
let userPicked = false;
let remember = { code: "", address: "" };

document.addEventListener("DOMContentLoaded", async () => {
  try {
    const stored = await chrome.storage.local.get([
      "address",
      "code",
      "receiverHost",
      "discovery",
    ]);
    remember = {
      code: stored.code ?? "",
      address: stored.address ?? "",
    };
    $("code").value = remember.code;
    $("address").value = remember.address;
    selected = stored.receiverHost ?? null;
    // Paint the last sweep before asking for a new one: the operator should see
    // the room's receiver on the first frame, not after an eight-second spinner.
    applyDiscovery(stored.discovery);
  } catch {}

  $("tab").addEventListener("click", () => start("tab"));
  $("screen").addEventListener("click", () => start("screen"));
  $("stop").addEventListener("click", () => stop());
  $("receiver").addEventListener("submit", (event) => {
    event.preventDefault();
    start("tab");
  });
  $("scan").addEventListener("click", () => void scan(true));
  $("code").addEventListener("input", () => {
    const input = $("code");
    const digits = input.value.replace(/\D/g, "").slice(0, 6);
    if (digits !== input.value) input.value = digits;
    setHint(hintFor());
  });
  $("address").addEventListener("input", () => setHint(hintFor()));
  $("receivers").addEventListener("change", (event) => {
    if (event.target.name !== "receiver") return;
    selected = event.target.value;
    userPicked = true;
    setHint(hintFor());
    void chrome.storage.local.set({ receiverHost: selected }).catch(() => {});
  });

  refresh();
  void scan(false);
  setInterval(refresh, 700);
});

async function send(message) {
  try {
    return await chrome.runtime.sendMessage({ to: "background", ...message });
  } catch (error) {
    return { ok: false, error: error?.message ?? String(error) };
  }
}

// ------------------------------------------------------------- discovery
//
// The sweep belongs to the service worker and its answers land in
// chrome.storage.local.discovery. That split is the whole point: the popup dies
// the moment the operator alt-tabs to read the 投屏码 off the big screen, and a
// 254-address scan must not die with it. The popup paints whatever the worker has
// already found and then follows its progress.

const CACHE_FRESH_MS = 10 * 60 * 1000;
const WATCH_TIMEOUT_MS = 20000;
let watchToken = 0;

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function scan(manual) {
  if (scanning) return;
  scanning = true;
  userPicked = false;
  setScanState(true);
  const reply = await send({ type: "scan", preferred: selected });
  applyDiscovery(reply?.cached);
  void watchDiscovery(manual);
}

async function watchDiscovery(manual) {
  const token = ++watchToken;
  const deadline = Date.now() + WATCH_TIMEOUT_MS;
  while (Date.now() < deadline) {
    await sleep(400);
    if (token !== watchToken) return;
    const { discovery } = await chrome.storage.local.get(["discovery"]);
    applyDiscovery(discovery);
    if (discovery?.done) break;
  }
  if (token !== watchToken) return;
  scanning = false;
  setScanState(false);
  if (manual) {
    setHint(
      receivers.length
        ? `发现 ${receivers.length} 台接收端，输入大屏上的投屏码即可投放。`
        : "没有发现接收端。请确认大屏的浏览器投屏已开启，或手动填写地址。",
    );
  }
  if (selected) void chrome.storage.local.set({ receiverHost: selected }).catch(() => {});
}

function applyDiscovery(next) {
  if (!next || !Array.isArray(next.receivers)) return;
  // A cache from last week is not a receiver list; a cache from the sweep that is
  // repainting this very popup is exactly one.
  if (!next.done && next.at && Date.now() - next.at > CACHE_FRESH_MS) return;
  receivers = next.receivers;
  // The operator's own choice outranks whatever the sweep prefers.
  if (!userPicked) selected = next.selected ?? selected;
  render();
}


function render() {
  const box = $("receivers");
  box.textContent = "";
  for (const receiver of receivers) {
    const label = document.createElement("label");
    label.className = "pick";

    const input = document.createElement("input");
    input.type = "radio";
    input.name = "receiver";
    input.value = receiver.host;
    input.checked = receiver.host === selected;

    const card = document.createElement("span");
    card.className = "pick__card";

    const dot = document.createElement("span");
    dot.className = "pick__dot";
    dot.setAttribute("aria-hidden", "true");

    const copy = document.createElement("span");
    copy.className = "pick__copy";
    const name = document.createElement("strong");
    name.textContent = receiver.name;
    const address = document.createElement("span");
    address.textContent = receiver.host;
    copy.append(name, address);

    const badge = document.createElement("span");
    badge.className = "pick__badge";
    badge.textContent = receiver.legacy
      ? "旧版，需投屏码"
      : receiver.paired
        ? "需要投屏码"
        : "开放接入";

    card.append(dot, copy, badge);
    label.append(input, card);
    box.append(label);
  }
  $("receiverEmpty").hidden = receivers.length > 0 || scanning;
  setHint(hintFor());
}

function setScanState(active) {
  $("scan").disabled = active;
  $("scan").classList.toggle("is-busy", active);
  $("scanLabel").textContent = active ? "搜索中…" : "搜索局域网";
  if (active) {
    setHint("正在扫描本机所在的网段…");
    return;
  }
  render();
}

function hintFor() {
  if (scanning) {
    return receivers.length
      ? `正在搜索局域网，已发现 ${receivers.length} 台接收端…`
      : "正在扫描本机所在的网段…";
  }
  const manual = $("address").value.trim();
  if (manual) return "使用手动地址投屏，地址需带大屏上的投屏码。";
  if (!receivers.length) return "没有发现接收端，可点“搜索局域网”重试，或手动填写地址。";
  if ($("code").value.length !== 6) return `已选 ${nameFor(selected)}，输入大屏上的 6 位投屏码即可投放。`;
  return `已选 ${nameFor(selected)}，投屏码 ${$("code").value}。`;
}

function nameFor(host) {
  return receivers.find((item) => item.host === host)?.name ?? "HORIZON 接收端";
}

function setHint(text) {
  $("receiverHint").textContent = text;
}

// ----------------------------------------------------------------- control

function currentUrl() {
  const code = $("code").value.trim();
  const manual = $("address").value.trim();
  if (manual) {
    const url = normalize(manual);
    // A typed IP still needs the code: without it the receiver answers 403.
    if (url && !/[?&]token=/.test(url) && /^\d{6}$/.test(code)) return `${url}?token=${code}`;
    return url;
  }
  if (!selected) return "";
  if (!/^\d{6}$/.test(code)) return "";
  return `ws://${selected}/stream?token=${code}`;
}

async function start(mode) {
  if (busy) return;
  const url = currentUrl();
  if (!url) {
    localError = {
      phase: "error",
      detail: selected
        ? "请输入大屏上显示的 6 位投屏码"
        : "请先选择接收端，或手动填写接收端地址",
    };
    show(localError);
    return;
  }
  localError = null;
  busy = true;
  show({
    phase: "connecting",
    detail: mode === "tab" ? "正在接管当前标签页…" : "正在等待你选择共享内容…",
  });
  try {
    await chrome.storage.local
      .set({ code: $("code").value.trim(), receiverHost: selected, address: $("address").value.trim() })
      .catch(() => {});
    const ensured = await send({ type: "ensure" });
    if (!ensured?.ok) throw new Error(ensured?.error ?? "无法打开投屏通道");
    let streamId;
    if (mode === "tab") {
      const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
      if (!tab?.id) throw new Error("找不到当前标签页");
      streamId = await new Promise((resolve, reject) => {
        chrome.tabCapture.getMediaStreamId({ targetTabId: tab.id }, (id) => {
          const error = chrome.runtime.lastError;
          if (error) reject(new Error(error.message));
          else resolve(id);
        });
      });
    }
    const reply = await send({ type: "start", url, mode, streamId });
    if (!reply?.ok) throw new Error(reply?.error ?? "投屏启动失败");
  } catch (error) {
    localError = { phase: "error", detail: explain(error, url) };
  } finally {
    busy = false;
    if (localError) show(localError);
    else refresh();
  }
}

function explain(error, url) {
  const text = error?.message ?? String(error ?? "未知错误");
  if (/token|403|拒绝|refuse/i.test(text)) {
    return `投屏码不被 ${url} 接受：${text}`;
  }
  return text;
}

async function stop() {
  if (busy) return;
  busy = true;
  show(view);
  await send({ type: "stop" });
  busy = false;
  localError = null;
  show({ phase: "idle", detail: "已停止投屏" });
  refresh();
}

async function refresh() {
  if (busy) return;
  const state = await send({ type: "status" });
  if (!state || !state.phase) return;
  if (localError && state.phase === "idle") {
    show(localError);
    return;
  }
  if (state.phase !== "idle") localError = null;
  show(state);
}

const PHASE_LABEL = {
  idle: "空闲",
  connecting: "连接中",
  live: "正在投屏",
  error: "出错了",
};

const PHASE_TITLE = {
  idle: "准备投放",
  connecting: "正在连接",
  live: "正在投屏",
  error: "投放未完成",
};

const PHASE_DETAIL = {
  idle: "选择接收端，输入大屏上的投屏码。",
  connecting: "正在建立局域网通道…",
  live: "画面与声音已接通",
  error: "请检查接收端与投屏码。",
};

function titleFor(state, phase) {
  if (phase === "live") {
    return state.mode === "tab" ? "正在投放标签页" : "正在投放屏幕";
  }
  return PHASE_TITLE[phase];
}

function show(state) {
  view = state;
  const phase = PHASE_LABEL[state.phase] ? state.phase : "idle";
  const active = phase === "connecting" || phase === "live";
  const detail = (state.detail ?? "").trim();

  document.body.dataset.phase = phase;
  $("state").textContent = busy ? "处理中" : PHASE_LABEL[phase];
  $("heroTitle").textContent = titleFor(state, phase);
  $("detail").textContent = detail || PHASE_DETAIL[phase];

  renderMetrics(state, active);

  const editable = !busy && !active;
  $("tab").disabled = !editable;
  $("screen").disabled = !editable;
  $("code").disabled = !editable;
  $("address").disabled = !editable;
  $("scan").disabled = !editable || scanning;
  $("stop").disabled = busy || !active;
  $("stop").hidden = !active;
}

function renderMetrics(state, active) {
  const stats = $("stats");
  stats.textContent = "";
  if (!active) return;

  const cells = [];
  if (state.width && state.height) {
    cells.push(["画面", `${state.width}×${state.height}`]);
  }
  if (state.fps) cells.push(["帧率", `${state.fps} fps`]);
  if (state.kbps) cells.push(["码率", formatRate(state.kbps)]);
  if (state.seconds >= 60) {
    cells.push(["时长", formatDuration(state.seconds)]);
  }

  for (const [label, value] of cells) {
    const cell = document.createElement("div");
    cell.className = "metric";
    const strong = document.createElement("strong");
    strong.textContent = value;
    const span = document.createElement("span");
    span.textContent = label;
    cell.append(strong, span);
    stats.append(cell);
  }

  const audio = document.createElement("span");
  audio.className = state.hasAudio ? "audio-chip is-on" : "audio-chip";
  audio.textContent = state.hasAudio ? "含声音" : "无声音";
  stats.append(audio);
}

function formatRate(kbps) {
  return kbps >= 1000 ? `${(kbps / 1000).toFixed(1)} Mb/s` : `${kbps} kb/s`;
}

function formatDuration(seconds) {
  const total = Math.max(0, Math.round(seconds));
  const minutes = Math.floor(total / 60);
  const rest = total % 60;
  return `${minutes}:${String(rest).padStart(2, "0")}`;
}

function normalize(value) {
  let text = (value ?? "").trim();
  if (!text) return "";
  if (!/^wss?:\/\//i.test(text)) text = `ws://${text}`;
  const match = /^(wss?:\/\/[^/]+)(\/.*)?$/i.exec(text);
  if (!match) return "";
  const origin = match[1];
  let rest = match[2] ?? "";
  if (!rest || rest === "/") rest = "/stream";
  else if (!rest.startsWith("/stream")) rest = `/stream${rest.startsWith("?") ? rest : ""}`;
  return origin + rest;
}
