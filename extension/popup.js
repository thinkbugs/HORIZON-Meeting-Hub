const $ = (id) => document.getElementById(id);
let view = { phase: "idle", detail: null };
let busy = false;
let localError = null;
let receivers = [];
let selected = null;
let scanning = false;
let userPicked = false;
let remember = { code: "", address: "" };

// The 文件 and 媒体 panes. Held apart from the mirror state because their cost is
// the thing under test: a request to the hub is only allowed while one of those
// two panes is showing, and a 投屏 page must stay at zero.
let pane = "cast";
let mediaBase = "";
let mediaMirror = "";
let mediaName = "";
let mediaReason = "no-receiver";
let mediaKey = "";
let mediaBusy = false;
let library = null;
let queue = null;
let queueTimer = null;
let mediaNote = "";
let mediaNoteKind = "";
let pendingFiles = [];
let fileNote = "还没有选择文件。";
let fileNoteKind = "";
let fileSending = false;

// DLNA 直投: the hub's own DLNA renderer at port 8290. The extension uploads a
// file to the library, then sends its URL via SOAP SetAVTransportURI+Play.
let dlnaPort = 8290;
let dlnaActive = false;
let dlnaUri = "";
let dlnaTitle = "";
let dlnaPollTimer = null;

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
  for (const button of document.querySelectorAll(".segment__item")) {
    button.addEventListener("click", () => void openPane(button.dataset.pane));
  }
  $("fileDrop").addEventListener("click", () => $("files").click());
  $("files").addEventListener("change", () => {
    addFiles([...$("files").files]);
    // Cleared so the same file can be picked again after being removed.
    $("files").value = "";
  });
  // Dropping anywhere on the page would otherwise navigate away from the popup;
  // the whole pane accepts files and lights up the picker while dragging.
  const filePane = $("pane-file");
  filePane.addEventListener("dragover", (event) => {
    if (![...(event.dataTransfer?.types ?? [])].includes("Files")) return;
    event.preventDefault();
    $("fileDrop").classList.add("is-drag");
  });
  filePane.addEventListener("dragleave", (event) => {
    if (!filePane.contains(event.relatedTarget)) $("fileDrop").classList.remove("is-drag");
  });
  filePane.addEventListener("drop", (event) => {
    if (!event.dataTransfer?.files?.length) return;
    event.preventDefault();
    $("fileDrop").classList.remove("is-drag");
    addFiles([...event.dataTransfer.files]);
  });
  $("sendFiles").addEventListener("click", () => void sendFiles());
  $("fileList").addEventListener("click", (event) => {
    const id = event.target.closest("[data-drop]")?.dataset.drop;
    if (id === undefined) return;
    pendingFiles = pendingFiles.filter((_, index) => String(index) !== id);
    fileNote = pendingFiles.length ? `已选 ${pendingFiles.length} 个文件。` : "还没有选择文件。";
    fileNoteKind = "";
    paintFiles();
  });
  $("pickMedia").addEventListener("click", () => $("mediaFiles").click());
  $("mediaFiles").addEventListener("change", () => {
    const picked = [...$("mediaFiles").files];
    $("mediaFiles").value = "";
    void uploadToLibrary(picked);
  });
  $("dlnaPick").addEventListener("click", () => $("dlnaFiles").click());
  $("dlnaFiles").addEventListener("change", () => {
    const picked = [...$("dlnaFiles").files];
    $("dlnaFiles").value = "";
    void castToDlna(picked);
  });
  $("dlnaPlayPause").addEventListener("click", () => void dlnaToggle());
  $("dlnaStop").addEventListener("click", () => void dlnaStopCast());
  $("refreshMedia").addEventListener("click", () => void retryMedia());
  $("playToggle").addEventListener("click", () => void run("transport", { action: queue?.playing ? "pause" : "play" }));
  $("stopPlayback").addEventListener("click", () => void run("transport", { action: "stop" }));
  $("urlInput").addEventListener("input", () => paintMedia());
  $("urlForm").addEventListener("submit", (event) => {
    event.preventDefault();
    void addUrl();
  });
  $("libraryList").addEventListener("click", (event) => void onLibraryOp(event));
  $("queueList").addEventListener("click", (event) => void onQueueOp(event));
  $("code").addEventListener("input", () => {
    const input = $("code");
    const digits = input.value.replace(/\D/g, "").slice(0, 6);
    if (digits !== input.value) input.value = digits;
    setHint(hintFor());
  });
  $("address").addEventListener("input", () => {
    setHint(hintFor());
    mediaBase = "";
    mediaKey = "";
  });
  $("receivers").addEventListener("change", (event) => {
    if (event.target.name !== "receiver") return;
    selected = event.target.value;
    userPicked = true;
    setHint(hintFor());
    // A different receiver is a different hub address: drop the cached base so the
    // next pane open resolves against this one. Dropping it issues no request.
    mediaBase = "";
    mediaKey = "";
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
// A sweep leaves the page's socket pool without a free slot for about ten seconds
// (see discovery.js). Starting another one inside that window aborts every probe
// at its 700 ms budget, and the second sweep then reports loopback only — the
// receiver that answered a moment ago disappears from the list. Measured: opening
// the popup 2 s after a completed sweep listed 127.0.0.1 and nothing else for the
// whole scan. So an automatic open reuses a list the worker finished moments ago;
// 「搜索局域网」is the one entry point that always re-sweeps.
const RESWEEP_MIN_MS = 12000;
let watchToken = 0;

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function scan(manual) {
  if (scanning) return;
  if (!manual) {
    const { discovery } = await chrome.storage.local.get(["discovery"]);
    if (discovery?.done && Date.now() - (discovery.at ?? 0) < RESWEEP_MIN_MS) {
      applyDiscovery(discovery);
      return;
    }
  }
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

// ---------------------------------------------------------------- panes
//
// One popup, three functions. Switching to 投屏 stops every hub request: the
// mirror is a websocket and the sweep belongs to the worker, so the page that is
// actually showing is the only thing allowed to poll the queue.

const PANES = ["cast", "file", "media"];

async function openPane(name) {
  if (!PANES.includes(name)) return;
  pane = name;
  for (const button of document.querySelectorAll(".segment__item")) {
    const active = button.dataset.pane === name;
    button.classList.toggle("is-active", active);
    button.setAttribute("aria-selected", active ? "true" : "false");
  }
  for (const section of document.querySelectorAll(".pane")) {
    section.hidden = section.id !== `pane-${name}`;
  }
  if (name === "file") paintFiles();
  if (name !== "media") {
    stopQueuePoll();
    paintMedia();
    return;
  }
  paintMedia();
  const reachable = await ensureBase(false);
  if (!reachable) {
    setMediaNote(statusLine(), "error");
    paintMedia();
    return;
  }
  await reloadMedia(false);
  startQueuePoll();
}

// ------------------------------------------------------------------ media

const KIND_LABEL = { video: "视频", audio: "音乐", image: "图片" };
const LIBRARY_DIRTY = new Set(["addUrl", "remove", "rename"]);
const ACTION_DONE = {
  setQueue: "已排入队列并开始播放",
  enqueue: "已追加到队列",
  queueMove: "队列顺序已更新",
  queueRemove: "已从队列移除",
  setNext: "已预排为下一条",
  jump: "已切到这一条",
  clearQueue: "队列已清空",
  transport: "播放状态已更新",
  addUrl: "地址已存入片库",
  remove: "已从片库删除",
};

function manualAddress() {
  return $("address").value.trim();
}

function currentReceiver() {
  // The manual address, when there is one, outranks this inside resolveBase —
  // one rule, in one file, the same way 投屏 already picks its url.
  return receivers.find((item) => item.host === selected) ?? null;
}

function receiverKey() {
  const typed = manualAddress();
  return typed ? `manual:${typed}` : `host:${selected ?? ""}`;
}

async function ensureBase(force) {
  const key = receiverKey();
  if (!force && mediaBase && key === mediaKey) return true;
  const outcome = await horizonMedia.resolveBase({ receiver: currentReceiver(), manual: manualAddress() });
  mediaBase = outcome.base;
  mediaMirror = outcome.mirror;
  mediaName = outcome.name ?? "";
  mediaReason = outcome.reason;
  mediaKey = outcome.base ? key : "";
  return Boolean(mediaBase);
}

function statusLine() {
  if (mediaReason === "no-receiver") return "先在「投屏」页选一台接收端，或手动填写它的投屏地址。";
  if (mediaReason === "unreachable") return `连不上 ${mediaMirror}，请确认大屏的浏览器投屏已经开启。`;
  if (mediaReason === "no-hub") return `该接收端没有开启片库服务：${mediaMirror} 的 /info 里没有 hub 端口。`;
  return `${mediaName || "HORIZON 接收端"} · ${mediaBase.replace(/^http:\/\//, "")}`;
}

function setMediaNote(text, kind = "") {
  mediaNote = text;
  mediaNoteKind = kind;
}

function describeMediaError(error) {
  const text = error?.message ?? String(error ?? "未知错误");
  if (/failed to fetch|networkerror|err_(connection|name|network)/i.test(text)) {
    return `连不上 ${mediaBase || mediaMirror || "接收端"}，投屏地址与片库端口都要通。`;
  }
  return text;
}

async function reloadMedia(withLibrary) {
  if (!mediaBase) return;
  mediaBusy = true;
  paintMedia();
  try {
    if (withLibrary || !library) library = await horizonMedia.api(mediaBase, "library");
    queue = await horizonMedia.api(mediaBase, "queue");
    setMediaNote(statusLine(), mediaNoteKind === "error" ? "" : mediaNoteKind);
  } catch (error) {
    setMediaNote(`${statusLine()} · ${describeMediaError(error)}`, "error");
  } finally {
    mediaBusy = false;
    paintMedia();
  }
}

/** 「重新读取」is also the retry button: when the base could not be resolved the
 *  lists are dead ends, so this clears the cached answer and walks the open path
 *  again instead of asking the operator to switch pages twice. */
async function retryMedia() {
  if (mediaBusy) return;
  mediaKey = "";
  mediaBase = "";
  library = null;
  queue = null;
  setMediaNote("");
  await openPane("media");
}

function startQueuePoll() {
  stopQueuePoll();
  queueTimer = setInterval(() => void tickQueue(), 1000);
}

function stopQueuePoll() {
  if (queueTimer !== null) {
    clearInterval(queueTimer);
    queueTimer = null;
  }
}

/** One `queue` read per second, and only while the 媒体 pane is on screen — the
 *  same budget the big screen spends, measured at node 6. The library is never
 *  polled: it only changes through this popup's own actions or an upload. */
async function tickQueue() {
  if (pane !== "media" || mediaBusy || !mediaBase) return;
  try {
    queue = await horizonMedia.api(mediaBase, "queue");
    if (mediaNoteKind === "error") setMediaNote(statusLine(), "");
  } catch (error) {
    setMediaNote(`${statusLine()} · ${describeMediaError(error)}`, "error");
  }
  // The whole page, not just the rows: a phone that adds a track between two of
  // this popup's own actions changes `queue` through here alone, and a toolbar
  // painted from the previous value stays disabled over a list with items in it.
  paintMedia();
}

async function run(action, args) {
  if (mediaBusy) return false;
  if (!(await ensureBase(false))) {
    setMediaNote(statusLine(), "error");
    paintMedia();
    return false;
  }
  mediaBusy = true;
  paintMedia();
  let ok = false;
  try {
    const result = await horizonMedia.api(mediaBase, action, args);
    // The queue actions answer with the queue view itself; only `remove` nests it
    // under `queue`. Reading one shape only left the toolbar disabled until the
    // next poll, because `setQueue` looked like it returned nothing.
    if (Array.isArray(result?.items)) queue = result;
    else if (result?.queue) queue = result.queue;
    if (LIBRARY_DIRTY.has(action)) library = await horizonMedia.api(mediaBase, "library");
    setMediaNote(`${statusLine()} · ${ACTION_DONE[action] ?? "已更新"}`, "ok");
    ok = true;
  } catch (error) {
    setMediaNote(`${statusLine()} · ${describeMediaError(error)}`, "error");
  } finally {
    mediaBusy = false;
    paintMedia();
    // The operator may have switched to 文件 mid-request; its send button was
    // disabled by mediaBusy and must come back without another click.
    if (pane === "file") paintFiles();
  }
  return ok;
}

async function uploadToLibrary(files) {
  if (!files.length) return;
  if (!(await ensureBase(false))) {
    setMediaNote(statusLine(), "error");
    paintMedia();
    return;
  }
  mediaBusy = true;
  setMediaNote(`正在上传 ${files.length} 个文件…`);
  paintMedia();
  try {
    const items = await horizonMedia.upload(mediaBase, "/api/media/upload", files);
    library = await horizonMedia.api(mediaBase, "library");
    setMediaNote(`${statusLine()} · 已入库 ${items.length} 个文件`, "ok");
  } catch (error) {
    setMediaNote(`${statusLine()} · 上传失败：${describeMediaError(error)}`, "error");
  } finally {
    mediaBusy = false;
    paintMedia();
    if (pane === "file") paintFiles();
  }
}

async function addUrl() {
  const url = $("urlInput").value.trim();
  if (!url) return;
  if (await run("addUrl", { url, title: "" })) $("urlInput").value = "";
}

// --------------------------------------------------------------- DLNA 直投

async function castToDlna(files) {
  if (!files.length) return;
  if (!(await ensureBase(false))) {
    setDlnaNote("连不上主机，无法上传媒体。", "error");
    return;
  }
  setDlnaNote(`正在上传 ${files.length} 个文件…`);
  paintDlna();
  try {
    const items = await horizonMedia.upload(mediaBase, "/api/media/upload", files);
    if (!items.length) throw new Error("上传成功但没有返回条目");
    const first = items[0];
    const playUrl = `${mediaBase}/media/${first.id}/${encodeURIComponent(first.title)}`;
    await horizonDlna.setUri(mediaBase, dlnaPort, playUrl, first.kind, first.title);
    await horizonDlna.play(mediaBase, dlnaPort);
    dlnaActive = true;
    dlnaUri = playUrl;
    dlnaTitle = first.title;
    setDlnaNote(`已投到 DLNA 接收器（端口 ${dlnaPort}）`, "ok");
    startDlnaPoll();
  } catch (error) {
    setDlnaNote(`DLNA 投送失败：${error?.message ?? error}`, "error");
    dlnaActive = false;
  }
  paintDlna();
}

async function dlnaToggle() {
  if (!dlnaActive) return;
  try {
    const state = await horizonDlna.getPosition(mediaBase, dlnaPort);
    const playing = state?.state === "PLAYING";
    if (playing) await horizonDlna.pause(mediaBase, dlnaPort);
    else await horizonDlna.play(mediaBase, dlnaPort);
  } catch (error) {
    setDlnaNote(`操作失败：${error?.message ?? error}`, "error");
  }
}

async function dlnaStopCast() {
  if (!dlnaActive) return;
  try {
    await horizonDlna.stop(mediaBase, dlnaPort);
  } catch {}
  dlnaActive = false;
  dlnaUri = "";
  dlnaTitle = "";
  stopDlnaPoll();
  setDlnaNote("已停止 DLNA 投送。");
  paintDlna();
}

function startDlnaPoll() {
  stopDlnaPoll();
  dlnaPollTimer = setInterval(() => void dlnaPoll(), 1000);
}

function stopDlnaPoll() {
  if (dlnaPollTimer !== null) {
    clearInterval(dlnaPollTimer);
    dlnaPollTimer = null;
  }
}

async function dlnaPoll() {
  if (!dlnaActive || !mediaBase) return;
  try {
    const state = await horizonDlna.getPosition(mediaBase, dlnaPort);
    if (!state) return;
    const dot = $("dlnaDot");
    const title = $("dlnaTitle");
    const stateLabel = $("dlnaState");
    const playPause = $("dlnaPlayPause");
    dot.classList.toggle("is-on", state.state === "PLAYING");
    title.textContent = dlnaTitle || "—";
    const phase = state.state === "PLAYING" ? "播放中" : state.state === "PAUSED_PLAYBACK" ? "已暂停" : "已停止";
    const clock = state.duration > 0 ? ` · ${formatDuration(state.seconds)} / ${formatDuration(state.duration)}` : "";
    stateLabel.textContent = `${phase}${clock}`;
    playPause.textContent = state.state === "PLAYING" ? "❚❚" : "▶";
    playPause.disabled = false;
    $("dlnaStop").disabled = false;
    if (state.state === "STOPPED" || state.state === "NO_MEDIA_PRESENT") {
      dlnaActive = false;
      stopDlnaPoll();
      setDlnaNote("播放已结束。");
      paintDlna();
    }
  } catch {}
}

function setDlnaNote(text, kind = "") {
  const note = $("dlnaNote");
  note.textContent = text;
  note.classList.toggle("is-error", kind === "error");
  note.classList.toggle("is-ok", kind === "ok");
}

function paintDlna() {
  if (pane !== "media") return;
  const usable = Boolean(mediaBase) && !mediaBusy;
  $("dlnaPick").disabled = !usable;
  $("dlnaNow").hidden = !dlnaActive && !dlnaUri;
  $("dlnaPlayPause").disabled = !dlnaActive;
  $("dlnaStop").disabled = !dlnaActive;
  $("dlnaCount").textContent = dlnaActive ? "投送中" : "—";
}

// --------------------------------------------------------------- rendering

function paintMedia() {
  if (pane !== "media") return;
  const status = $("mediaStatus");
  status.textContent = mediaNote || statusLine();
  status.classList.toggle("is-error", mediaNoteKind === "error");
  status.classList.toggle("is-ok", mediaNoteKind === "ok");

  const usable = Boolean(mediaBase) && !mediaBusy;
  $("pickMedia").disabled = !usable;
  // Retry has to stay clickable when there is no base at all — that is exactly
  // the case where the rest of the page is inert.
  $("refreshMedia").disabled = mediaBusy;
  $("urlAdd").disabled = !usable || $("urlInput").value.trim() === "";
  $("playToggle").disabled = !usable || !(queue?.items?.length ?? 0);
  $("stopPlayback").disabled = !usable || !(queue?.playing || queue?.state?.uri);
  $("playToggle").textContent = queue?.playing ? "❚❚" : "▶";
  $("playToggle").title = queue?.playing ? "暂停" : "播放";
  paintQueue();
  paintLibrary();
  paintDlna();
}

function appendEmpty(box, text) {
  const li = document.createElement("li");
  li.className = "rows__empty";
  li.textContent = text;
  box.append(li);
}

function kindChip(kind) {
  const chip = document.createElement("span");
  chip.className = "row__kind";
  chip.textContent = KIND_LABEL[kind] ?? kind;
  return chip;
}

function opButton(op, title, kind = "") {
  const button = document.createElement("button");
  button.type = "button";
  button.className = kind ? `op op--${kind}` : "op";
  button.textContent = op;
  button.title = title;
  button.dataset.op = op;
  return button;
}

function rowShell(index) {
  const li = document.createElement("li");
  li.className = "row";
  if (index !== undefined) {
    const slot = document.createElement("span");
    slot.className = "row__slot";
    slot.textContent = String(index + 1);
    li.append(slot);
  }
  return li;
}

function rowMain(title, meta, badge, asButton) {
  const main = document.createElement(asButton ? "button" : "span");
  main.className = "row__main";
  if (asButton) main.type = "button";
  const strong = document.createElement("span");
  strong.className = "row__title";
  strong.textContent = title;
  strong.title = title;
  if (badge) {
    const tag = document.createElement("em");
    tag.className = "row__badge";
    tag.textContent = badge;
    strong.append(tag);
  }
  const detail = document.createElement("span");
  detail.className = "row__meta";
  detail.textContent = meta;
  main.append(strong, detail);
  return main;
}

function paintQueue() {
  const box = $("queueList");
  box.textContent = "";
  $("queueCount").textContent = queue ? `${queue.items.length} 项` : "—";
  if (!queue) {
    appendEmpty(box, mediaBase ? "正在读取队列…" : "连接上接收端以后才能看到队列。");
    paintNowPlaying();
    return;
  }
  if (!queue.items.length) {
    appendEmpty(box, "队列是空的。下方片库点 ▶ 只投这一条，点 ＋ 追加到队尾。");
    paintNowPlaying();
    return;
  }
  queue.items.forEach((item, index) => {
    const li = rowShell(index);
    if (queue.current === index) li.classList.add("is-current");
    li.append(kindChip(item.kind));
    const main = rowMain(item.title, "", item.local ? "" : "外链", true);
    main.dataset.jump = item.id;
    li.append(main);
    const ops = document.createElement("span");
    ops.className = "row__ops";
    const up = opButton("↑", "上移一位");
    const down = opButton("↓", "下移一位");
    up.disabled = index === 0;
    down.disabled = index === queue.items.length - 1;
    up.dataset.move = String(index - 1);
    down.dataset.move = String(index + 1);
    up.dataset.id = item.id;
    down.dataset.id = item.id;
    const next = opButton("⇥", "预排为下一条");
    next.dataset.next = item.id;
    const drop = opButton("✕", "从队列移除", "danger");
    drop.dataset.remove = item.id;
    ops.append(up, down, next, drop);
    li.append(ops);
    box.append(li);
  });
  paintNowPlaying();
}

function paintNowPlaying() {
  const dot = $("nowDot");
  const title = $("nowTitle");
  const state = $("nowState");
  if (!queue) {
    dot.classList.remove("is-on");
    title.textContent = "队列是空的";
    state.textContent = "未连接";
    return;
  }
  const current = queue.current === null || queue.current === undefined ? null : queue.items[queue.current];
  dot.classList.toggle("is-on", Boolean(queue.playing));
  title.textContent = current ? current.title : "没有正在播放的条目";
  const phase = queue.state?.phase ?? "idle";
  if (queue.halted) {
    state.textContent = `队列自行停止：${queue.halted}`;
    return;
  }
  const clock =
    queue.state?.duration > 0
      ? ` · ${formatDuration(queue.state.position ?? 0)} / ${formatDuration(queue.state.duration)}`
      : "";
  state.textContent = `${PHASE_CN[phase] ?? phase}${queue.state?.error ? ` · ${queue.state.error}` : ""}${clock}`;
}

const PHASE_CN = {
  idle: "空闲",
  loading: "加载中",
  playing: "正在播放",
  paused: "已暂停",
  stopped: "已停止",
  ended: "播完",
  error: "播放出错",
};

function paintLibrary() {
  const box = $("libraryList");
  box.textContent = "";
  $("libraryCount").textContent = library ? `${library.items.length} 项` : "—";
  if (!library) {
    appendEmpty(box, mediaBase ? "正在读取片库…" : "连接上接收端以后才能看到片库。");
    return;
  }
  if (!library.items.length) {
    appendEmpty(box, "片库还没有文件。点「上传到片库」，或让手机投一条进来。");
    return;
  }
  // No thumbnails here: the hub serves the whole file at /media/<id>, so a row of
  // previews in a 372px popup would download every picture in the library just to
  // draw it small. The big screen has the thumbnails; this page has names, kinds
  // and sizes.
  for (const entry of library.items) {
    const li = rowShell();
    li.append(kindChip(entry.kind));
    const main = rowMain(entry.title, `${formatBytes(entry.bytes)} · ${KIND_LABEL[entry.kind] ?? entry.kind}`, entry.file ? "" : "直投", false);
    li.append(main);
    const ops = document.createElement("span");
    ops.className = "row__ops";
    const play = opButton("▶", "只投这一条");
    play.dataset.play = entry.id;
    const add = opButton("＋", "追加到队列");
    add.dataset.enqueue = entry.id;
    const drop = opButton("✕", "从片库删除", "danger");
    drop.dataset.drop = entry.id;
    ops.append(play, add, drop);
    li.append(ops);
    box.append(li);
  }
}

function onLibraryOp(event) {
  const button = event.target;
  const id = button.dataset?.play ?? button.dataset?.enqueue ?? button.dataset?.drop;
  if (!id) return;
  if (button.dataset.play) void run("setQueue", { ids: [id], autoplay: true });
  else if (button.dataset.enqueue) void run("enqueue", { ids: [id], autoplay: false });
  else void run("remove", { id });
}

function onQueueOp(event) {
  const target = event.target;
  if (target.dataset?.jump) {
    void run("jump", { id: target.dataset.jump });
    return;
  }
  if (target.dataset?.move !== undefined) {
    void run("queueMove", { id: target.dataset.id, to: Number(target.dataset.move) });
    return;
  }
  if (target.dataset.next) void run("setNext", { id: target.dataset.next });
  else if (target.dataset.remove) void run("queueRemove", { id: target.dataset.remove });
}

// ------------------------------------------------------------ file transfer

function setFileNote(text, kind = "") {
  fileNote = text;
  fileNoteKind = kind;
}

function fileKey(file) {
  return `${file.name}\u0000${file.size}\u0000${file.lastModified}`;
}

function addFiles(list) {
  const seen = new Set(pendingFiles.map(fileKey));
  let added = 0;
  for (const file of list) {
    const key = fileKey(file);
    if (seen.has(key)) continue;
    seen.add(key);
    pendingFiles.push(file);
    added += 1;
  }
  if (!added && pendingFiles.length) {
    fileNote = `${pendingFiles.length} 个文件已在列表里，重复的没有再加。`;
  } else {
    fileNote = pendingFiles.length
      ? `已选 ${pendingFiles.length} 个文件，发送后落在接收端的接收目录。`
      : "还没有选择文件。";
  }
  fileNoteKind = "";
  paintFiles();
}

function fileBadge(name) {
  const match = /\.([a-z0-9]{1,5})$/i.exec(name ?? "");
  return match ? match[1].toUpperCase() : "文件";
}

function paintFiles() {
  const box = $("fileList");
  box.textContent = "";
  const count = pendingFiles.length;
  const total = pendingFiles.reduce((sum, file) => sum + (file.size || 0), 0);
  $("fileCount").textContent = count ? `${count} 个 · ${formatBytes(total)}` : "0 个";

  const status = $("fileStatus");
  $("fileStatusText").textContent = fileNote;
  status.classList.toggle("is-error", fileNoteKind === "error");
  status.classList.toggle("is-ok", fileNoteKind === "ok");

  const send = $("sendFiles");
  send.disabled = count === 0 || mediaBusy;
  send.querySelector(".send__copy strong").textContent = fileSending
    ? "正在发送…"
    : count
      ? `发送 ${count} 个文件`
      : "发送文件";
  send.querySelector(".send__copy small").textContent = fileSending
    ? "已交给局域网，请稍候"
    : count
      ? `共 ${formatBytes(total)}`
      : "选择文件后发送";
  $("sendMeter").hidden = !fileSending;

  pendingFiles.forEach((file, index) => {
    const li = rowShell();
    const chip = document.createElement("span");
    chip.className = "row__kind";
    chip.textContent = fileBadge(file.name);
    li.append(chip);
    li.append(rowMain(file.name, formatBytes(file.size), "", false));
    const ops = document.createElement("span");
    ops.className = "row__ops";
    const drop = opButton("✕", "不发送这个文件", "danger");
    drop.dataset.drop = String(index);
    ops.append(drop);
    li.append(ops);
    box.append(li);
  });
  if (!count) appendEmpty(box, "还没有选择文件，点上面的区域或把文件拖进来。");
}

async function sendFiles() {
  if (!pendingFiles.length || mediaBusy) return;
  const count = pendingFiles.length;
  mediaBusy = true;
  fileSending = true;
  setFileNote(`正在发送 ${count} 个文件…`);
  paintFiles();
  try {
    if (!(await ensureBase(false))) throw new Error(statusLine());
    const reply = await horizonMedia.upload(mediaBase, "/upload", pendingFiles);
    setFileNote(reply || `已送达 ${count} 个文件`, "ok");
    pendingFiles = [];
    $("files").value = "";
  } catch (error) {
    setFileNote(`发送失败：${describeMediaError(error)}`, "error");
  } finally {
    mediaBusy = false;
    fileSending = false;
    paintFiles();
  }
}

function formatBytes(bytes) {
  if (!(bytes > 0)) return "—";
  const units = ["B", "KB", "MB", "GB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value >= 10 || unit === 0 ? Math.round(value) : value.toFixed(1)} ${units[unit]}`;
}
