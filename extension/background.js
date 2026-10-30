// Routing, offscreen lifecycle and LAN discovery. Capture lives in the offscreen
// document; the popup closes as soon as a picker opens or focus moves, so it
// cannot be the owner of anything long-lived — which is exactly why the sweep
// runs here instead.

importScripts("discovery.js");

const OFFSCREEN_PATH = "offscreen.html";
const REASONS = ["USER_MEDIA", "DISPLAY_MEDIA", "AUDIO_PLAYBACK"];

let lastState = idleState();
let creating = null;
let sweeping = null;

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (!message || message.to !== "background") return false;
  switch (message.type) {
    case "state":
      if (message.state) lastState = message.state;
      sendResponse({ ok: true });
      return false;
    case "scan":
      // The popup asks, then watches chrome.storage: the sweep outlives the popup.
      startSweep(message.preferred ?? null).then(
        (reply) => sendResponse(reply),
        (error) => sendResponse({ ok: false, error: describe(error) }),
      );
      return true;
    case "ensure":
      ensureOffscreen().then(
        () => sendResponse({ ok: true }),
        (error) => sendResponse({ ok: false, error: describe(error) }),
      );
      return true;
    case "start":
      ensureOffscreen()
        .then(() =>
          toOffscreen({
            type: "start",
            url: message.url,
            mode: message.mode,
            streamId: message.streamId,
          }),
        )
        .then(
          (reply) => sendResponse(reply ?? { ok: false, error: "投屏通道没有响应" }),
          (error) => sendResponse({ ok: false, error: describe(error) }),
        );
      return true;
    case "stop":
      toOffscreen({ type: "stop" }).then(
        () => {
          lastState = idleState();
          sendResponse({ ok: true });
        },
        () => {
          lastState = idleState();
          sendResponse({ ok: true });
        },
      );
      return true;
    case "status":
      queryStatus().then(sendResponse, () => sendResponse(lastState));
      return true;
    default:
      return false;
  }
});

// One sweep at a time; a second request joins the running one instead of
// starting a parallel scan of the same 254 addresses.
async function startSweep(preferred) {
  const stored = await chrome.storage.local.get(["discovery", "address"]);
  const cached = stored.discovery ?? null;
  if (!sweeping) {
    const extraHost = hostFromUrl(stored.address ?? "");
    sweeping = globalThis.horizonDiscovery
      .sweep({
        preferred: preferred || cached?.selected || null,
        extraHost,
        onProgress: ({ receivers, selected, done }) =>
          chrome.storage.local.set({
            discovery: { at: Date.now(), receivers, selected, done },
          }),
      })
      .catch(() => null)
      .finally(() => {
        sweeping = null;
      });
  }
  return { ok: true, scanning: true, cached };
}

function hostFromUrl(value) {
  const match = /^wss?:\/\/([^/?#]+)/i.exec((value ?? "").trim());
  if (!match) return "";
  return match[1].split(":")[0];
}

// Warm the cache before the popup is ever opened, so the first thing the operator
// sees is a receiver rather than a spinner.
chrome.runtime.onStartup.addListener(() => void startSweep(null));
chrome.runtime.onInstalled.addListener(() => void startSweep(null));

function idleState() {
  return {
    phase: "idle",
    mode: null,
    url: null,
    detail: null,
    width: 0,
    height: 0,
    fps: 0,
    frames: 0,
    keyframes: 0,
    dropped: 0,
    kbps: 0,
    seconds: 0,
    hasAudio: false,
  };
}

async function hasOffscreen() {
  if (!chrome.runtime.getContexts) return false;
  const contexts = await chrome.runtime.getContexts({
    contextTypes: ["OFFSCREEN_DOCUMENT"],
    documentUrls: [chrome.runtime.getURL(OFFSCREEN_PATH)],
  });
  return contexts.length > 0;
}

async function ensureOffscreen() {
  if (await hasOffscreen()) return;
  if (creating) return creating;
  creating = chrome.offscreen
    .createDocument({
      url: OFFSCREEN_PATH,
      reasons: REASONS,
      justification: "捕捉标签页或屏幕并在局域网内编码投放，同时把标签页声音回放到本机。",
    })
    .catch((error) => {
      if (!String(error?.message ?? error).includes("single offscreen")) throw error;
    })
    .finally(() => {
      creating = null;
    });
  return creating;
}

async function toOffscreen(message) {
  if (!(await hasOffscreen())) return null;
  try {
    return await chrome.runtime.sendMessage({ to: "offscreen", ...message });
  } catch {
    return null;
  }
}

async function queryStatus() {
  const reply = await toOffscreen({ type: "status" });
  if (reply) lastState = reply;
  return lastState;
}

function describe(error) {
  return error?.message ?? String(error ?? "未知错误");
}
