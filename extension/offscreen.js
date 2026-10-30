// Capture and encode the page here, not in the popup: a popup dies the moment
// the user picks a screen or switches tabs, and a dead renderer cannot hold a
// MediaStream. Offscreen documents live until the extension does something
// about them, so the stream, encoder and socket all belong here.

const HEADER = 24;
const KIND_VIDEO = 1;
const KIND_AUDIO = 2;
// A fresh IDR only matters after a reconnect or a decoder stall; the receiver
// asks for one explicitly in both cases. A longer periodic interval keeps the
// expensive keyframe out of the steady-state pipeline.
const KEYFRAME_MS = 5000;
// Above 1080p the extra pixels cost encode time, bandwidth and decode time
// without looking better on a projector or a 1080p panel.
const MAX_DIM = 1920;
// One frame may wait for the encoder; two would stack a slow keyframe behind a
// delta and add a whole frame interval of latency for nothing.
const VIDEO_QUEUE_LIMIT = 1;
// ~0.4 s of video at the top bitrate. Anything more is latency, not safety.
const VIDEO_BUFFER_LIMIT = 350_000;
// ~0.4 s of 48 kHz stereo PCM; dropping older audio only adds a click.
const AUDIO_BUFFER_LIMIT = 80_000;
const CONNECT_TIMEOUT_MS = 6000;
const MAX_RECONNECTS = 6;

let current = null;
let lastState = idleState();

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

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (!message || message.to !== "offscreen") return false;
  switch (message.type) {
    case "start":
      start(message).then(
        () => sendResponse({ ok: true }),
        (error) => sendResponse({ ok: false, error: describe(error) }),
      );
      return true;
    case "stop":
      stop().then(
        () => sendResponse({ ok: true }),
        () => sendResponse({ ok: true }),
      );
      return true;
    case "status":
      sendResponse(current ? current.snapshot() : lastState);
      return false;
    default:
      return false;
  }
});

async function start(options) {
  await stop();
  const mode = options.mode === "screen" ? "screen" : "tab";
  const session = new MirrorSession({ mode, url: options.url, streamId: options.streamId });
  current = session;
  try {
    await session.begin();
  } catch (error) {
    await session.discard();
    current = null;
    lastState = {
      ...idleState(),
      phase: "error",
      mode,
      url: options.url,
      detail: describe(error),
    };
    throw error;
  }
}

async function stop() {
  const session = current;
  current = null;
  if (session) await session.discard();
  lastState = idleState();
  publish(lastState);
}

function publish(state) {
  chrome.runtime.sendMessage({ to: "background", type: "state", state }).catch(() => {});
}

class MirrorSession {
  constructor({ mode, url, streamId }) {
    this.mode = mode;
    this.url = url;
    this.streamId = streamId;
    this.media = null;
    this.videoTrack = null;
    this.audioTrack = null;
    this.ws = null;
    this.encoder = null;
    this.reader = null;
    this.audio = null;
    this.source = null;
    this.node = null;
    this.parameterSets = null;
    // Encoded chunks carry the track's media timestamp, not wall time. The
    // wall-clock moment each frame was handed to the encoder is kept here so
    // the receiver can measure real end-to-end latency.
    this.born = new Map();
    this.width = 0;
    this.height = 0;
    this.sourceWidth = 0;
    this.sourceHeight = 0;
    this.fps = 30;
    this.forceKey = true;
    this.lastKey = 0;
    this.live = false;
    this.stopping = false;
    this.attempt = 0;
    this.detail = "正在连接接收端…";
    this.ticker = null;
    this.clock = Date.now();
    this.stats = { frames: 0, keyframes: 0, dropped: 0, videoBytes: 0, audioBytes: 0 };
    this.window = { at: Date.now(), frames: 0, fps: 0 };
  }

  async begin() {
    await this.connect();
    await this.capture();
    this.ticker = setInterval(() => this.tick(), 1000);
    await this.startVideo();
    await this.startAudio();
    this.detail = this.audioTrack ? "画面与声音已接通" : "画面已接通（没有声音）";
    this.publish();
  }

  // ------------------------------------------------------------- transport

  openSocket() {
    return new Promise((resolve, reject) => {
      let settled = false;
      let ws;
      try {
        ws = new WebSocket(this.url);
      } catch (error) {
        reject(new Error(`地址无效：${describe(error)}`));
        return;
      }
      ws.binaryType = "arraybuffer";
      const timer = setTimeout(() => {
        if (settled) return;
        settled = true;
        try {
          ws.close();
        } catch {}
        reject(new Error("连接接收端超时，请确认地址和防火墙"));
      }, CONNECT_TIMEOUT_MS);

      ws.onopen = () => {
        clearTimeout(timer);
        this.ws = ws;
        this.attempt = 0;
        this.forceKey = true;
        this.detail = "已连接，正在取画面…";
        try {
          ws.send(JSON.stringify({ type: "hello", protocol: 1, mode: this.mode }));
        } catch {}
        if (!settled) {
          settled = true;
          resolve();
        }
      };
      ws.onmessage = (event) => {
        if (typeof event.data !== "string") return;
        try {
          const message = JSON.parse(event.data);
          if (message.type === "welcome") {
            this.live = true;
            this.detail = message.receiver ? `已连接 ${message.receiver}` : "已连接";
            this.publish();
          } else if (message.type === "keyframe") {
            // The viewer fell behind and threw its decoder away; the next
            // captured frame should be a fresh IDR.
            this.forceKey = true;
          }
        } catch {}
      };
      ws.onerror = () => {};
      ws.onclose = (event) => {
        clearTimeout(timer);
        if (this.ws === ws) this.ws = null;
        this.live = false;
        if (this.stopping) {
          if (!settled) {
            settled = true;
            reject(new Error("已停止"));
          }
          return;
        }
        if (!settled) {
          settled = true;
          reject(
            new Error(
              event.code === 1006
                ? "接收端拒绝连接：请核对大屏上的投屏码与投屏开关"
                : `连接被关闭（${event.code}）`,
            ),
          );
        }
        this.scheduleReconnect();
      };
    });
  }

  async connect() {
    this.publish();
    try {
      await this.openSocket();
    } catch (error) {
      if (!this.stopping && this.attempt === 0) {
        // The tab stays muted for as long as tabCapture owns it, so a receiver
        // that never answers must not leave capture running.
        throw error;
      }
      throw error;
    }
  }

  scheduleReconnect() {
    if (this.stopping) return;
    this.attempt += 1;
    if (this.attempt > MAX_RECONNECTS) {
      this.fail("与接收端断开，已停止重连");
      return;
    }
    const wait = Math.min(8000, 1000 * 2 ** (this.attempt - 1));
    this.detail = `连接中断，${Math.round(wait / 1000)} 秒后重试（第 ${this.attempt} 次）`;
    this.publish();
    setTimeout(() => {
      if (this.stopping) return;
      this.openSocket().catch(() => {});
    }, wait);
  }

  send(packet) {
    const ws = this.ws;
    if (!ws || ws.readyState !== WebSocket.OPEN) return false;
    try {
      ws.send(packet);
      return true;
    } catch {
      return false;
    }
  }

  isOpen() {
    return Boolean(this.ws) && this.ws.readyState === WebSocket.OPEN;
  }

  // --------------------------------------------------------------- capture

  async capture() {
    if (this.mode === "tab") {
      const mandatory = { chromeMediaSource: "tab", chromeMediaSourceId: this.streamId };
      if (!this.streamId) throw new Error("没有拿到标签页采集授权，请重新点击");
      this.media = await navigator.mediaDevices.getUserMedia({
        audio: { mandatory },
        video: { mandatory },
      });
    } else {
      this.media = await navigator.mediaDevices.getDisplayMedia({
        video: { frameRate: { ideal: 30, max: 30 } },
        audio: {
          echoCancellation: false,
          noiseSuppression: false,
          autoGainControl: false,
        },
        selfBrowserSurface: "exclude",
        surfaceSwitching: "include",
        systemAudio: "include",
      });
    }

    this.videoTrack = this.media.getVideoTracks()[0] ?? null;
    this.audioTrack = this.media.getAudioTracks()[0] ?? null;
    if (!this.videoTrack) throw new Error("没有可用的画面");
    try {
      // Screen and tab content is mostly text and edges; tell the encoder so
      // it favours sharpness over smooth-motion savings.
      this.videoTrack.contentHint = "detail";
    } catch {}
    this.videoTrack.addEventListener("ended", () => {
      if (this.stopping) return;
      this.detail = "投屏已结束";
      this.fail("投屏已结束");
    });
    if (this.audioTrack) {
      this.audioTrack.addEventListener("ended", () => {
        this.audioTrack = null;
        this.publish();
      });
    }
  }

  // --------------------------------------------------------------- encoder

  async startVideo() {
    const settings = this.videoTrack.getSettings();
    this.sourceWidth = settings.width || 1280;
    this.sourceHeight = settings.height || 720;
    const size = fitSize(this.sourceWidth, this.sourceHeight);
    this.width = size.width;
    this.height = size.height;
    this.fps = Math.min(30, Math.max(15, Math.round(settings.frameRate || 30)));
    const config = await chooseVideoConfig(this.width, this.height, this.fps);
    this.encoder = new VideoEncoder({
      output: (chunk, metadata) => this.onChunk(chunk, metadata),
      error: (error) => this.fail(`编码失败：${describe(error)}`),
    });
    this.encoder.configure(config);
    const processor = new MediaStreamTrackProcessor({ track: this.videoTrack });
    this.reader = processor.readable.getReader();
    void this.pump();
  }

  async pump() {
    while (!this.stopping) {
      let frame = null;
      try {
        const result = await this.reader.read();
        if (result.done) break;
        frame = result.value;
      } catch {
        break;
      }
      if (this.stopping) {
        frame.close();
        break;
      }
      if (
        frame.displayWidth !== this.sourceWidth ||
        frame.displayHeight !== this.sourceHeight
      ) {
        this.sourceWidth = frame.displayWidth;
        this.sourceHeight = frame.displayHeight;
        const size = fitSize(this.sourceWidth, this.sourceHeight);
        this.width = size.width;
        this.height = size.height;
        try {
          this.encoder.configure(await chooseVideoConfig(this.width, this.height, this.fps));
          this.forceKey = true;
        } catch {}
      }
      if (!this.isOpen()) {
        this.forceKey = true;
        frame.close();
        continue;
      }
      const backlogged =
        this.encoder.encodeQueueSize > VIDEO_QUEUE_LIMIT ||
        this.ws.bufferedAmount > VIDEO_BUFFER_LIMIT;
      if (backlogged && !this.forceKey) {
        this.stats.dropped += 1;
        frame.close();
        continue;
      }
      const now = performance.now();
      const key = this.forceKey || now - this.lastKey >= KEYFRAME_MS;
      const bornUs = Math.round(performance.timeOrigin * 1000 + now * 1000);
      this.born.set(frame.timestamp, bornUs);
      if (this.born.size > 240) {
        const oldest = this.born.keys().next().value;
        if (oldest !== undefined) this.born.delete(oldest);
      }
      try {
        this.encoder.encode(frame, { keyFrame: key });
        if (key) {
          this.forceKey = false;
          this.lastKey = now;
        }
      } catch {
        this.forceKey = true;
      }
      frame.close();
    }
  }

  onChunk(chunk, metadata) {
    if (this.stopping) return;
    const key = chunk.type === "key";
    const wire = new Uint8Array(chunk.byteLength);
    chunk.copyTo(wire);
    let payload = toAnnexB(wire);
    if (key) {
      const sets = metadata?.decoderConfig?.description;
      if (sets) this.parameterSets = parameterSets(sets) ?? this.parameterSets;
      // The preview configures its decoder from the keyframe it receives, so
      // every IDR must carry SPS/PPS even if the encoder keeps them out of band.
      if (this.parameterSets) payload = concat(this.parameterSets, payload);
    }
    this.stats.frames += 1;
    if (key) this.stats.keyframes += 1;
    this.stats.videoBytes += payload.length;
    this.window.frames += 1;
    // Wall-clock microseconds (flag bit 1) of the moment the frame entered the
    // encoder, so the receiver can measure end-to-end latency and staleness.
    const nowUs = Math.round(performance.timeOrigin * 1000 + performance.now() * 1000);
    const ptsUs = this.born.get(chunk.timestamp) ?? nowUs;
    this.born.delete(chunk.timestamp);
    this.send(
      mediaPacket(
        KIND_VIDEO,
        (key ? 1 : 0) | 2,
        ptsUs,
        this.width,
        this.height,
        payload,
      ),
    );
  }

  // ----------------------------------------------------------------- audio

  async startAudio() {
    if (!this.audioTrack) return;
    const context = new AudioContext({ sampleRate: 48000, latencyHint: "interactive" });
    this.audio = context;
    await context.audioWorklet.addModule("pcm-worklet.js");
    const source = context.createMediaStreamSource(new MediaStream([this.audioTrack]));
    this.source = source;
    if (this.mode === "tab") {
      // tabCapture silences the tab it owns; without this the presenter, the
      // room and the local speakers all lose the sound they were hearing.
      source.connect(context.destination);
    }
    const node = new AudioWorkletNode(context, "pcm-capture", {
      numberOfInputs: 1,
      numberOfOutputs: 1,
      outputChannelCount: [2],
    });
    node.port.onmessage = (event) => this.onPcm(event.data);
    source.connect(node);
    const silent = context.createGain();
    silent.gain.value = 0;
    node.connect(silent).connect(context.destination);
    this.node = node;
  }

  onPcm(buffer) {
    const pcm = new Uint8Array(buffer);
    this.stats.audioBytes += pcm.length;
    if (this.ws && this.ws.bufferedAmount > AUDIO_BUFFER_LIMIT) return;
    this.send(mediaPacket(KIND_AUDIO, 0, 0, Math.round(this.audio.sampleRate / 100), 2, pcm));
  }

  // ---------------------------------------------------------------- status

  tick() {
    if (this.stopping) return;
    const now = Date.now();
    const elapsed = Math.max(1, (now - this.window.at) / 1000);
    this.window.fps = Math.round(this.window.frames / elapsed);
    this.window.frames = 0;
    this.window.at = now;
    this.publish();
  }

  snapshot() {
    const seconds = Math.max(0, Math.round((Date.now() - this.clock) / 1000));
    const bytes = this.stats.videoBytes + this.stats.audioBytes;
    return {
      phase: this.stopping ? "idle" : this.live ? "live" : "connecting",
      mode: this.mode,
      url: this.url,
      detail: this.detail,
      width: this.width,
      height: this.height,
      fps: this.window.fps,
      frames: this.stats.frames,
      keyframes: this.stats.keyframes,
      dropped: this.stats.dropped,
      kbps: Math.round((bytes * 8) / 1000 / Math.max(1, seconds)),
      seconds,
      hasAudio: Boolean(this.audioTrack),
    };
  }

  publish() {
    const state = this.snapshot();
    lastState = state;
    chrome.runtime
      .sendMessage({ to: "background", type: "state", state })
      .catch(() => {});
  }

  fail(reason) {
    if (this.stopping) return;
    this.detail = reason;
    this.lastKey = 0;
    this.discard().then(() => {
      if (current === this) {
        current = null;
        lastState = { ...idleState(), phase: "error", mode: this.mode, url: this.url, detail: reason };
        publish(lastState);
      }
    });
  }

  async discard() {
    this.stopping = true;
    clearInterval(this.ticker);
    this.ticker = null;
    try {
      await this.reader?.cancel();
    } catch {}
    this.reader = null;
    if (this.encoder && this.encoder.state !== "closed") {
      try {
        this.encoder.close();
      } catch {}
    }
    this.encoder = null;
    for (const node of [this.node, this.source]) {
      try {
        node?.disconnect();
      } catch {}
    }
    this.node = null;
    this.source = null;
    if (this.audio) {
      try {
        await this.audio.close();
      } catch {}
    }
    this.audio = null;
    for (const track of this.media?.getTracks() ?? []) {
      try {
        track.stop();
      } catch {}
    }
    this.media = null;
    this.videoTrack = null;
    this.audioTrack = null;
    this.born.clear();
    const ws = this.ws;
    this.ws = null;
    if (ws) {
      if (ws.readyState === WebSocket.OPEN) {
        try {
          ws.send(JSON.stringify({ type: "stop" }));
        } catch {}
        try {
          ws.close(1000, "stopped");
        } catch {}
      } else {
        try {
          ws.close();
        } catch {}
      }
    }
  }
}

// -------------------------------------------------------------- wire format

/** 24-byte header the receiver shares with the AirPlay preview. */
function mediaPacket(kind, flags, pts, first, second, payload) {
  const body = new Uint8Array(HEADER + payload.length);
  const view = new DataView(body.buffer);
  body[0] = kind;
  body[1] = flags;
  view.setBigUint64(4, BigInt(Math.max(0, Math.round(pts || 0))), true);
  view.setUint16(12, first & 0xffff, true);
  view.setUint16(14, second & 0xffff, true);
  view.setUint32(16, payload.length, true);
  body.set(payload, HEADER);
  return body;
}

/** AVCC length-prefixed samples to Annex B, which the receiver's preview reads. */
function toAnnexB(data) {
  const out = new Uint8Array(data.length);
  let read = 0;
  let write = 0;
  while (read + 4 <= data.length) {
    const length =
      ((data[read] << 24) | (data[read + 1] << 16) | (data[read + 2] << 8) | data[read + 3]) >>> 0;
    read += 4;
    if (length === 0 || read + length > data.length) return data;
    out[write] = 0;
    out[write + 1] = 0;
    out[write + 2] = 0;
    out[write + 3] = 1;
    write += 4;
    out.set(data.subarray(read, read + length), write);
    write += length;
    read += length;
  }
  return out.subarray(0, write);
}

/** Pulls SPS/PPS out of the avcC record the encoder reports for keyframes. */
function parameterSets(description) {
  const bytes = new Uint8Array(description);
  if (bytes.length < 7 || bytes[0] !== 1) return null;
  const parts = [];
  let at = 5;
  const spsCount = bytes[at] & 0x1f;
  at += 1;
  for (let index = 0; index < spsCount; index += 1) {
    if (at + 2 > bytes.length) return null;
    const length = (bytes[at] << 8) | bytes[at + 1];
    at += 2;
    if (at + length > bytes.length) return null;
    parts.push(new Uint8Array([0, 0, 0, 1]), bytes.subarray(at, at + length));
    at += length;
  }
  if (at >= bytes.length) return concatAll(parts);
  const ppsCount = bytes[at];
  at += 1;
  for (let index = 0; index < ppsCount; index += 1) {
    if (at + 2 > bytes.length) return concatAll(parts);
    const length = (bytes[at] << 8) | bytes[at + 1];
    at += 2;
    if (at + length > bytes.length) return concatAll(parts);
    parts.push(new Uint8Array([0, 0, 0, 1]), bytes.subarray(at, at + length));
    at += length;
  }
  return concatAll(parts);
}

async function chooseVideoConfig(width, height, fps) {
  const level = width * height > 1920 * 1080 ? "33" : "2a";
  // Text-heavy screen content needs pixels per frame more than it needs
  // smoothness, and this is a LAN link, so lean generous.
  const bitrate = Math.min(12_000_000, Math.max(1_500_000, Math.round(width * height * fps * 0.12)));
  const profiles = ["6400" + level, "4d40" + level, "42e0" + level];
  for (const acceleration of ["prefer-hardware", "no-preference"]) {
    for (const profile of profiles) {
      const config = {
        codec: `avc1.${profile}`,
        width,
        height,
        bitrate,
        framerate: fps,
        latencyMode: "realtime",
        hardwareAcceleration: acceleration,
        avc: { format: "avc" },
      };
      try {
        const support = await VideoEncoder.isConfigSupported(config);
        if (support.supported) return config;
      } catch {}
    }
  }
  throw new Error("浏览器没有可用的 H.264 编码器");
}

function concat(head, tail) {
  const out = new Uint8Array(head.length + tail.length);
  out.set(head, 0);
  out.set(tail, head.length);
  return out;
}

function concatAll(parts) {
  let length = 0;
  for (const part of parts) length += part.length;
  const out = new Uint8Array(length);
  let at = 0;
  for (const part of parts) {
    out.set(part, at);
    at += part.length;
  }
  return out;
}

function even(value) {
  return Math.max(2, Math.round(value / 2) * 2);
}

/** Encoded size: keep the aspect ratio, cap the long edge, keep both even. */
function fitSize(width, height) {
  const scale = Math.min(1, MAX_DIM / Math.max(width || 1, height || 1));
  return { width: even((width || 1280) * scale), height: even((height || 720) * scale) };
}

function describe(error) {
  if (!error) return "未知错误";
  const text = error.message ?? String(error);
  if (text.includes("Permission denied") || text.includes("NotAllowedError")) {
    return "权限被拒绝或未选择共享内容";
  }
  return text;
}

if (typeof globalThis.addEventListener === "function") {
  globalThis.addEventListener("unhandledrejection", (event) => {
    if (current && !current.stopping) current.fail(describe(event.reason));
  });
}
