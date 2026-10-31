import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  castControl,
  castSend,
  castStatus,
  discoverPeers,
  miracastLaunch,
  miracastStatus,
  openBuiltin,
  openExtensionFolder,
  receiverStatus,
  startAirplay,
  startBrowser,
  startDlna,
  stopAirplay,
  stopBrowser,
  stopDlna,
} from "../lib/bridge";
import type {
  AirplayStatus,
  BrowserStatus,
  CastSenderPhase,
  CastSenderStatus,
  DlnaStatus,
  LanPeer,
  MiracastStatus,
  ReceiverStatus,
} from "../types";
import { AirPlayGlyph, CastGlyph, CloseIcon, FullscreenIcon, RadarIcon } from "./Icons";
import { MirrorSurface, useMirrorPreview } from "./MirrorPreview";
import type { MirrorView } from "./MirrorPreview";
import { CastStage } from "./CastStage";
import { Modal, Switch } from "./Overlay";

interface CastPanelProps {
  deviceName: string;
  presenting: "airplay" | "browser" | null;
  onPresent: (source: "airplay" | "browser") => void;
  onStageClose: () => void;
  onPresentDlna: () => void;
  onClose: () => void;
  onNotify: (text: string, tone?: "success" | "error" | "info") => void;
}

type PanelTab = "receive" | "send";

const EMPTY: ReceiverStatus = { airplay: null, dlna: null, browser: null };

const PHASE_LABELS: Record<DlnaStatus["phase"], string> = {
  idle: "等待控制器投放",
  loading: "正在加载",
  playing: "正在播放",
  paused: "已暂停",
  stopped: "已停止",
  ended: "播放结束",
  error: "播放出错",
};

const SENDER_PHASE_LABELS: Record<CastSenderPhase, string> = {
  idle: "空闲",
  connecting: "正在连接",
  launching: "正在启动播放器",
  loading: "正在加载",
  playing: "正在播放",
  paused: "已暂停",
  buffering: "缓冲中",
  stopped: "已结束",
  ended: "播放结束",
  error: "投放失败",
};

const TERMINAL_PHASES: CastSenderPhase[] = ["idle", "stopped", "ended", "error"];

const PROTOCOL_CLASS: Record<LanPeer["protocol"], string> = {
  HORIZON: "cast-chip--horizon",
  Cast: "cast-chip--cast",
  AirPlay: "cast-chip--airplay",
  DLNA: "cast-chip--dlna",
};

export function CastPanel({
  deviceName,
  presenting,
  onPresent,
  onStageClose,
  onPresentDlna,
  onClose,
  onNotify,
}: CastPanelProps) {
  const [tab, setTab] = useState<PanelTab>("receive");
  const [status, setStatus] = useState<ReceiverStatus>(EMPTY);
  const [busy, setBusy] = useState<"airplay" | "dlna" | "browser" | "miracast" | null>(null);
  const [miracast, setMiracast] = useState<MiracastStatus | null>(null);
  const [peers, setPeers] = useState<LanPeer[] | null>(null);
  const [scanning, setScanning] = useState(false);
  const [scanError, setScanError] = useState<string | null>(null);
  const [sender, setSender] = useState<CastSenderStatus | null>(null);
  const [selectedDevice, setSelectedDevice] = useState<string | null>(null);
  const [manualHost, setManualHost] = useState("");
  const [mediaUrl, setMediaUrl] = useState("");
  const [sending, setSending] = useState(false);
  const [clock, setClock] = useState(() => Date.now());
  const mounted = useRef(true);

  const casting = sender !== null && !TERMINAL_PHASES.includes(sender.phase);

  // One decoder per source, owned here: the card and the full-screen stage
  // alternate as its surface, and the session stays attached while the panel
  // shows another tab. A hand-off that rebuilt the decoder would end the
  // picture for good, because an AirPlay session sends its only IDR at the
  // start.
  const [airplayHost, setAirplayHost] = useState<HTMLDivElement | null>(null);
  const [browserHost, setBrowserHost] = useState<HTMLDivElement | null>(null);
  const [stageHost, setStageHost] = useState<HTMLDivElement | null>(null);

  const airplayMirror = useMirrorPreview({
    active: Boolean(status.airplay),
    source: "airplay",
    host: presenting === "airplay" ? stageHost : airplayHost,
  });
  const browserMirror = useMirrorPreview({
    active: Boolean(status.browser),
    source: "browser",
    host: presenting === "browser" ? stageHost : browserHost,
  });

  const stage = useMemo(() => {
    if (!presenting) return null;
    if (presenting === "browser" && status.browser) {
      const parts: string[] = [];
      if (status.browser.width > 0 && status.browser.height > 0) {
        parts.push(`${status.browser.width} × ${status.browser.height}`);
      }
      parts.push(status.browser.peer ? `来自 ${status.browser.peer}` : "局域网直连");
      return {
        title: "浏览器投屏",
        detail: parts.join(" · "),
        live: status.browser.connected,
      };
    }
    if (presenting === "airplay" && status.airplay) {
      const parts: string[] = [];
      if (status.airplay.mirrorWidth > 0 && status.airplay.mirrorHeight > 0) {
        parts.push(`${status.airplay.mirrorWidth} × ${status.airplay.mirrorHeight}`);
      }
      parts.push(status.airplay.paired ? "已配对" : "AirPlay");
      return { title: "AirPlay 镜像", detail: parts.join(" · "), live: status.airplay.mirroring };
    }
    return null;
  }, [presenting, status]);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  useEffect(() => {
    let active = true;
    const refresh = () => {
      receiverStatus()
        .then((next) => {
          if (active) setStatus(next);
        })
        .catch(() => undefined);
      castStatus()
        .then((next) => {
          if (active) setSender(next);
        })
        .catch(() => undefined);
    };
    void refresh();
    const interval = window.setInterval(refresh, 1000);
    return () => {
      active = false;
      window.clearInterval(interval);
    };
  }, []);

  useEffect(() => {
    if (!casting) return;
    setClock(Date.now());
    const timer = window.setInterval(() => setClock(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [casting]);

  const closePanel = useCallback(() => {
    if (presenting) {
      onStageClose();
      return;
    }
    onClose();
  }, [onClose, onStageClose, presenting]);

  const scan = useCallback(async () => {
    setScanning(true);
    setScanError(null);
    try {
      const found = await discoverPeers(3);
      if (mounted.current) setPeers(found);
    } catch (reason) {
      if (mounted.current) setScanError(displayError(reason));
    } finally {
      if (mounted.current) setScanning(false);
    }
  }, []);

  useEffect(() => {
    void scan();
  }, [scan]);

  const castDevices = useMemo(
    () => (peers ?? []).filter((peer) => peer.protocol === "Cast" && peer.address),
    [peers],
  );

  useEffect(() => {
    if (manualHost.trim()) return;
    if (selectedDevice && castDevices.some((device) => device.id === selectedDevice)) return;
    setSelectedDevice(castDevices[0]?.id ?? null);
  }, [castDevices, manualHost, selectedDevice]);

  const toggleAirplay = useCallback(
    async (on: boolean) => {
      if (busy) return;
      setBusy("airplay");
      try {
        if (on) {
          const next = await startAirplay(deviceName);
          setStatus((previous) => ({ ...previous, airplay: next }));
          onNotify(`AirPlay 音频接收已开启`, "success");
        } else {
          await stopAirplay();
          setStatus((previous) => ({ ...previous, airplay: null }));
          onNotify("AirPlay 音频接收已关闭", "info");
        }
      } catch (reason) {
        onNotify(displayError(reason), "error");
      } finally {
        setBusy(null);
      }
    },
    [busy, deviceName, onNotify],
  );

  const toggleDlna = useCallback(
    async (on: boolean) => {
      if (busy) return;
      setBusy("dlna");
      try {
        if (on) {
          const next = await startDlna(deviceName);
          setStatus((previous) => ({ ...previous, dlna: next }));
          onNotify("DLNA 视频接收已开启", "success");
        } else {
          await stopDlna();
          setStatus((previous) => ({ ...previous, dlna: null }));
          onNotify("DLNA 视频接收已关闭", "info");
        }
      } catch (reason) {
        onNotify(displayError(reason), "error");
      } finally {
        setBusy(null);
      }
    },
    [busy, deviceName, onNotify],
  );

  // Windows owns the Miracast receiver, so this panel only asks the system for
  // it: probe once for the launcher and let the card word itself from that.
  useEffect(() => {
    miracastStatus()
      .then((next) => {
        if (mounted.current) setMiracast(next);
      })
      .catch(() => undefined);
  }, []);

  const startMiracast = useCallback(async () => {
    if (busy) return;
    setBusy("miracast");
    try {
      const next = await miracastLaunch();
      onNotify(
        next.mode === "launched"
          ? "已启动系统无线投屏，请在发送端选择本机"
          : "系统尚未安装「无线显示器」，已打开「投影到此电脑」设置页，点「可选功能」添加即可",
        next.mode === "launched" ? "success" : "info",
      );
    } catch (reason) {
      onNotify(displayError(reason), "error");
    } finally {
      setBusy(null);
    }
  }, [busy, onNotify]);

  const toggleBrowser = useCallback(
    async (on: boolean) => {
      if (busy) return;
      setBusy("browser");
      try {
        if (on) {
          const next = await startBrowser(deviceName);
          setStatus((previous) => ({ ...previous, browser: next }));
          onNotify("浏览器投屏接收已开启", "success");
        } else {
          await stopBrowser();
          setStatus((previous) => ({ ...previous, browser: null }));
          onNotify("浏览器投屏接收已关闭", "info");
        }
      } catch (reason) {
        onNotify(displayError(reason), "error");
      } finally {
        setBusy(null);
      }
    },
    [busy, deviceName, onNotify],
  );

  const startCast = useCallback(async () => {
    if (sending || casting) return;
    const device = castDevices.find((item) => item.id === selectedDevice);
    const host = manualHost.trim() || device?.address?.trim() || "";
    const url = mediaUrl.trim();
    if (!host) {
      onNotify("请选择电视，或输入设备地址", "error");
      return;
    }
    if (!url) {
      onNotify("请输入电视可以直接访问的媒体地址", "error");
      return;
    }
    setSending(true);
    try {
      const next = await castSend(device?.name ?? host, host, url);
      setSender(next);
      onNotify(`正在发送到 ${next.device}`, "success");
    } catch (reason) {
      onNotify(displayError(reason), "error");
    } finally {
      setSending(false);
    }
  }, [castDevices, casting, manualHost, mediaUrl, onNotify, selectedDevice, sending]);

  const controlCast = useCallback(
    async (action: "play" | "pause" | "stop") => {
      try {
        const next = await castControl(action);
        if (action === "stop") {
          setSender(null);
          onNotify("已停止投放", "info");
          return;
        }
        setSender((previous) =>
          previous
            ? { ...previous, ...next, phase: action === "pause" ? "paused" : "playing" }
            : next,
        );
        onNotify(action === "pause" ? "已暂停" : "继续播放", "info");
      } catch (reason) {
        onNotify(displayError(reason), "error");
      }
    },
    [onNotify],
  );

  let position = sender?.position ?? 0;
  if (sender?.phase === "playing") {
    position += Math.max(0, (clock - sender.observedAt) / 1000);
  }
  if (sender && sender.duration > 0) {
    position = Math.min(position, sender.duration);
  }

  return (
    <Modal
      onClose={closePanel}
      variant="drawer"
      className="settings-drawer cast-drawer"
      labelledBy="cast-title"
    >
      <header className="settings__header">
        <div>
          <p className="section-eyebrow">CAST HUB</p>
          <h2 id="cast-title">投屏</h2>
        </div>
        <div className="cast-drawer__tools">
          <div className="filter-tabs" role="tablist" aria-label="投屏方式">
            <button
              role="tab"
              aria-selected={tab === "receive"}
              className={tab === "receive" ? "is-active" : ""}
              onClick={() => setTab("receive")}
            >
              接收
            </button>
            <button
              role="tab"
              aria-selected={tab === "send"}
              className={tab === "send" ? "is-active" : ""}
              onClick={() => setTab("send")}
            >
              发送
            </button>
          </div>
          <button className="icon-button modal-close" onClick={closePanel} aria-label="关闭投屏">
            <CloseIcon size={20} />
          </button>
        </div>
      </header>

      <div className="settings__scroll">
        {tab === "receive" ? (
          <>
            <section className="settings-section cast-intro">
              <p>
                让手机、平板或电脑把声音与画面投到这块大屏。接收服务只在本机局域网内广播，
                不会连接外网，应用退出时自动关闭。
              </p>
            </section>

            <AirplayCard
              status={status.airplay}
              busy={busy === "airplay"}
              onToggle={(on) => void toggleAirplay(on)}
              mirror={airplayMirror}
              onMirrorHost={setAirplayHost}
              onPresent={() => onPresent("airplay")}
            />

            <BrowserCard
              status={status.browser}
              busy={busy === "browser"}
              onToggle={(on) => void toggleBrowser(on)}
              onNotify={onNotify}
              mirror={browserMirror}
              onMirrorHost={setBrowserHost}
              onPresent={() => onPresent("browser")}
            />

            <DlnaCard
              status={status.dlna}
              busy={busy === "dlna"}
              onToggle={(on) => void toggleDlna(on)}
              onPresent={onPresentDlna}
            />

            <MiracastCard
              status={miracast}
              busy={busy === "miracast"}
              onLaunch={() => void startMiracast()}
              onOpenSettings={() => void openBuiltin("projection")}
            />

            <section className="settings-section">
              <div className="settings-section__title">
                <div>
                  <RadarIcon size={17} />
                  <h3>局域网中的投屏设备</h3>
                </div>
                <button
                  className="button button--small button--ghost"
                  onClick={() => void scan()}
                  disabled={scanning}
                >
                  {scanning ? "扫描中…" : "重新扫描"}
                </button>
              </div>
              <p className="settings-section__description">
                通过 mDNS 与 SSDP 做一次三秒快照，用来确认网络连通情况。
              </p>

              {scanError ? (
                <div className="cast-empty is-error">{scanError}</div>
              ) : peers === null || scanning ? (
                <div className="cast-empty">{peers === null ? "正在扫描局域网…" : "刷新中…"}</div>
              ) : peers.length === 0 ? (
                <div className="cast-empty">没有发现其他投屏设备。请确认与本机处于同一网段。</div>
              ) : (
                <div className="cast-peers">
                  {peers.map((peer) => (
                    <div className="cast-peer" key={peer.id}>
                      <div className="cast-peer__copy">
                        <strong>{peer.name}</strong>
                        <span>{peer.address ?? peer.location ?? "地址待解析"}</span>
                      </div>
                      <span className={`cast-peer__chip ${PROTOCOL_CLASS[peer.protocol] ?? ""}`}>
                        {peer.protocol}
                      </span>
                    </div>
                  ))}
                </div>
              )}
            </section>
          </>
        ) : (
          <>
            <section className="settings-section cast-intro">
              <p>
                把电视可以直接访问的媒体地址交给 Chromecast 或内置 Cast 的电视播放。
                发送端只传递地址，不转发画面文件；电视需要能访问该地址才会开始播放。
              </p>
            </section>

            <section className="settings-section">
              <div className="settings-section__title">
                <div>
                  <RadarIcon size={17} />
                  <h3>可投送的电视</h3>
                </div>
                <button
                  className="button button--small button--ghost"
                  onClick={() => void scan()}
                  disabled={scanning}
                >
                  {scanning ? "扫描中…" : "重新扫描"}
                </button>
              </div>
              <p className="settings-section__description">
                通过 mDNS 查找支持 Google Cast 的设备。电视内置 Cast 时，通常需要保持唤醒。
              </p>

              {scanError ? (
                <div className="cast-empty is-error">{scanError}</div>
              ) : peers === null || (scanning && castDevices.length === 0) ? (
                <div className="cast-empty">正在扫描局域网…</div>
              ) : castDevices.length === 0 ? (
                <div className="cast-empty">
                  没有发现 Cast 设备。请确认电视与本机在同一网段，或在下方手动输入地址。
                </div>
              ) : (
                <div className="cast-peers">
                  {castDevices.map((device) => (
                    <button
                      type="button"
                      key={device.id}
                      className={`cast-peer cast-peer--select${
                        device.id === selectedDevice ? " is-selected" : ""
                      }`}
                      onClick={() => {
                        setSelectedDevice(device.id);
                        setManualHost("");
                      }}
                      disabled={casting}
                    >
                      <div className="cast-peer__copy">
                        <strong>{device.name}</strong>
                        <span>{[device.model, device.address].filter(Boolean).join(" · ")}</span>
                      </div>
                      <span className="cast-peer__chip cast-chip--cast">Cast</span>
                    </button>
                  ))}
                </div>
              )}

              <label className="field cast-field">
                <span className="field__label">手动地址</span>
                <input
                  value={manualHost}
                  onChange={(event) => {
                    setManualHost(event.target.value);
                    if (event.target.value.trim()) setSelectedDevice(null);
                  }}
                  placeholder="192.168.0.23:8009"
                  disabled={casting}
                  spellCheck={false}
                />
              </label>
              <p className="cast-note">发现不到设备时可手动输入，端口默认 8009。</p>
            </section>

            {sender && <SenderCard status={sender} position={position} onControl={controlCast} />}

            {!casting && (
              <section className="settings-section">
                <div className="settings-section__title">
                  <div>
                    <CastGlyph size={17} />
                    <h3>投送媒体</h3>
                  </div>
                </div>
                <label className="field cast-field">
                  <span className="field__label">媒体地址</span>
                  <input
                    value={mediaUrl}
                    onChange={(event) => setMediaUrl(event.target.value)}
                    placeholder="http://192.168.0.10:8000/demo.mp4"
                    disabled={sending}
                    spellCheck={false}
                  />
                </label>
                <p className="cast-note">
                  仅支持 http(s)。文件类型按扩展名识别，电视无法解码时会在投送状态里说明。
                </p>
                <button
                  className="button button--primary cast-submit"
                  onClick={() => void startCast()}
                  disabled={sending}
                >
                  {sending ? "正在发送…" : "发送到电视"}
                </button>
              </section>
            )}
          </>
        )}
      </div>

      <footer className="settings__footer">
        <span className="settings__save-state">
          <span />
          {tab === "receive"
            ? "接收状态每秒刷新"
            : casting
              ? "发送状态每秒刷新"
              : "Cast v2 · 默认媒体接收器"}
        </span>
        <button className="button button--primary" onClick={closePanel}>
          完成
        </button>
      </footer>
      {stage && (
        // The stage is a body-level portal, so it can live here in the panel
        // tree while the canvas the session decodes into travels with it.
        <CastStage
          title={stage.title}
          detail={stage.detail}
          live={stage.live}
          code={presenting === "browser" ? status.browser?.code : undefined}
          mirror={presenting === "browser" ? browserMirror : airplayMirror}
          onMirrorHost={setStageHost}
          onClose={onStageClose}
        />
      )}
    </Modal>
  );
}

function MiracastCard({
  status,
  busy,
  onLaunch,
  onOpenSettings,
}: {
  status: MiracastStatus | null;
  busy: boolean;
  onLaunch: () => void;
  onOpenSettings: () => void;
}) {
  const installed = Boolean(status?.launcher);
  return (
    <section className="settings-section cast-card">
      <div className="cast-card__head">
        <span className="cast-card__glyph is-cast">
          <CastGlyph size={21} />
        </span>
        <div className="cast-card__copy">
          <div className="cast-card__title">
            <h3>Miracast 投屏</h3>
            <span className={`badge ${installed ? "badge--live" : "badge--muted"}`}>
              {status === null ? "检测中" : installed ? "系统已就绪" : "需要安装组件"}
            </span>
          </div>
          <p>
            Windows 自带的无线显示接收能力，直接复用系统组件，本应用只做入口。
            启动后由系统全屏接管，任何支持 Miracast 的 Windows、Android 设备都能发现本机。
          </p>
        </div>
      </div>

      <div className="cast-card__body">
        <div className="cast-actions">
          <button
            type="button"
            className="button button--small button--primary"
            onClick={onLaunch}
            disabled={busy}
          >
            {busy ? "处理中…" : installed ? "启动系统无线投屏" : "打开「投影到此电脑」设置"}
          </button>
          {/* Without the receiver the primary button already opens this page, so
              printing a second route to the same place would be a lie. */}
          {installed && (
            <button
              type="button"
              className="button button--small button--ghost"
              onClick={onOpenSettings}
            >
              投影到此电脑
            </button>
          )}
        </div>
        <p className="cast-note">
          {installed
            ? `系统组件：${status?.launcher}`
            : "未检测到「无线显示器」组件。在「设置 > 系统 > 投影到此电脑」点「可选功能」，搜索“无线显示器”并安装，装好后这里会自动变为启动。"}
        </p>
      </div>
    </section>
  );
}

function SenderCard({
  status,
  position,
  onControl,
}: {
  status: CastSenderStatus;
  position: number;
  onControl: (action: "play" | "pause" | "stop") => void;
}) {
  const active = !TERMINAL_PHASES.includes(status.phase);
  return (
    <section className="settings-section cast-card">
      <div className="cast-card__head">
        <span className="cast-card__glyph is-cast">
          <CastGlyph size={21} />
        </span>
        <div className="cast-card__copy">
          <div className="cast-card__title">
            <h3>{status.device}</h3>
            <span className={`badge ${status.phase === "error" ? "badge--muted" : "badge--live"}`}>
              {SENDER_PHASE_LABELS[status.phase] ?? status.phase}
            </span>
          </div>
          <p>
            {status.host} · {status.contentType}
          </p>
        </div>
      </div>

      <div className="cast-card__body">
        <dl className="cast-facts">
          <div>
            <dt>播放进度</dt>
            <dd>
              {formatTime(position)}
              {status.duration > 0 ? ` / ${formatTime(status.duration)}` : ""}
            </dd>
          </div>
          <div>
            <dt>媒体地址</dt>
            <dd title={status.url}>{status.url}</dd>
          </div>
          {status.volume !== null && (
            <div>
              <dt>设备音量</dt>
              <dd>{Math.round(status.volume * 100)}%</dd>
            </div>
          )}
        </dl>
        {status.error && <p className="cast-note is-warning">{status.error}</p>}
        <div className="cast-actions">
          {(status.phase === "playing" || status.phase === "buffering") && (
            <button className="button button--small button--ghost" onClick={() => onControl("pause")}>
              暂停
            </button>
          )}
          {status.phase === "paused" && (
            <button className="button button--small button--ghost" onClick={() => onControl("play")}>
              继续播放
            </button>
          )}
          {active ? (
            <button
              className="button button--small button--danger-ghost"
              onClick={() => onControl("stop")}
            >
              停止投放
            </button>
          ) : (
            <button className="button button--small button--ghost" onClick={() => onControl("stop")}>
              清除记录
            </button>
          )}
        </div>
      </div>
    </section>
  );
}

function AirplayCard({
  status,
  busy,
  onToggle,
  mirror,
  onMirrorHost,
  onPresent,
}: {
  status: AirplayStatus | null;
  busy: boolean;
  onToggle: (on: boolean) => void;
  mirror: MirrorView;
  onMirrorHost: (element: HTMLDivElement | null) => void;
  onPresent: () => void;
}) {
  return (
    <section className="settings-section cast-card">
      <div className="cast-card__head">
        <span className="cast-card__glyph is-airplay">
          <AirPlayGlyph size={21} />
        </span>
        <div className="cast-card__copy">
          <div className="cast-card__title">
            <h3>AirPlay 接收</h3>
            <span className={`badge ${status ? "badge--live" : "badge--muted"}`}>
              {busy ? "处理中" : status ? "已开启" : "已关闭"}
            </span>
          </div>
          <p>
            iPhone、iPad 与 Mac 的音频与屏幕镜像。RAOP / L16 无损 PCM，镜像画面为 H.264。
          </p>
        </div>
        <Switch checked={Boolean(status)} onChange={onToggle} label="AirPlay 接收" />
      </div>

      {status && (
        <div className="cast-card__body">
          <dl className="cast-facts">
            <div>
              <dt>广播名称</dt>
              <dd title={status.service}>{status.service}</dd>
            </div>
            <div>
              <dt>监听地址</dt>
              <dd>
                {status.address}:{status.port}
              </dd>
            </div>
            <div>
              <dt>当前状态</dt>
              <dd>
                {status.mirroring
                  ? "正在镜像"
                  : status.connected
                    ? "已连接，正在接收"
                    : "等待发送方连接"}
              </dd>
            </div>
            {status.mirrorFrames > 0 && (
              <div>
                <dt>镜像画面</dt>
                <dd>
                  {status.mirrorWidth > 0 && status.mirrorHeight > 0
                    ? `${status.mirrorWidth}×${status.mirrorHeight}`
                    : "H.264"}
                  {status.paired ? " · 已配对" : ""}
                </dd>
              </div>
            )}
          </dl>
          <div className="cast-actions">
            <button type="button" className="button button--small" onClick={onPresent}>
              <FullscreenIcon size={13} />
              全屏观看
            </button>
          </div>
          <MirrorSurface view={mirror} onHost={onMirrorHost} onClick={onPresent} />
          <div className="cast-stats">
            <span>
              <strong>{status.seconds.toFixed(1)}s</strong>已接收音频
            </span>
            <span>
              <strong>{status.packets}</strong>音频包
            </span>
            <span>
              <strong>{status.missing}</strong>丢包
            </span>
            <span>
              <strong>{Math.round(status.volumeDb)} dB</strong>音量
            </span>
          </div>
          {status.mirrorFrames > 0 && (
            <p className="cast-note">
              镜像帧 {status.mirrorFrames}，关键帧 {status.mirrorKeyframes}；落后实时{" "}
              {status.mirrorDriftMs.toFixed(0)} ms（峰值{" "}
              {status.mirrorDriftMaxMs.toFixed(0)} ms）· 帧间隔{" "}
              {status.mirrorGapAvgMs.toFixed(1)} ms（最长{" "}
              {status.mirrorGapMaxMs.toFixed(0)} ms）· 本机每帧{" "}
              {status.mirrorPipelineMs.toFixed(2)} ms；镜像音频（AAC-ELD）尚未解码，当前只播放镜像画面。
            </p>
          )}
          {status.preview && status.preview.painted > 0 && (
            <p className="cast-note">
              预览上屏 {status.preview.fps.toFixed(0)} 帧/秒 · 到达到绘制{" "}
              {status.preview.inFlightAvgMs.toFixed(0)} ms（峰值{" "}
              {status.preview.inFlightMaxMs.toFixed(0)} ms）· 解码队列峰值{" "}
              {status.preview.queueMax} · 每秒追帧丢弃 {status.preview.skipped} 帧 · 转发丢帧{" "}
              {status.mirrorDrops}
            </p>
          )}
          {status.lastPeer && (
            <p className="cast-note">
              上次会话：{status.lastPeer}
              {status.lastCodec ? ` · ${status.lastCodec}` : ""}
            </p>
          )}
        </div>
      )}
    </section>
  );
}

function BrowserCard({
  status,
  busy,
  onToggle,
  onNotify,
  mirror,
  onMirrorHost,
  onPresent,
}: {
  status: BrowserStatus | null;
  busy: boolean;
  onToggle: (on: boolean) => void;
  onNotify: (text: string, tone?: "success" | "error" | "info") => void;
  mirror: MirrorView;
  onMirrorHost: (element: HTMLDivElement | null) => void;
  onPresent: () => void;
}) {
  const copy = async () => {
    if (!status) return;
    try {
      await copyText(status.url);
      onNotify("接收地址已复制", "success");
    } catch {
      onNotify("复制失败，请手动选中地址", "error");
    }
  };

  return (
    <section className="settings-section cast-card">
      <div className="cast-card__head">
        <span className="cast-card__glyph is-cast">
          <CastGlyph size={21} />
        </span>
        <div className="cast-card__copy">
          <div className="cast-card__title">
            <h3>浏览器投屏</h3>
            <span className={`badge ${status ? "badge--live" : "badge--muted"}`}>
              {busy ? "处理中" : status ? "已开启" : "已关闭"}
            </span>
          </div>
          <p>
            Chrome 或 Edge 扩展把标签页、窗口或整块屏幕编码后投到本机。画面 H.264，
            声音为原始 PCM，全程只在局域网内。
          </p>
        </div>
        <Switch checked={Boolean(status)} onChange={onToggle} label="浏览器投屏接收" />
      </div>

      {status && (
        <div className="cast-card__body">
          <dl className="cast-facts">
            <div>
              <dt>投屏码</dt>
              <dd className="cast-code-value" title={status.code}>
                {status.code}
              </dd>
            </div>
            <div>
              <dt>接收地址</dt>
              <dd title={status.url}>{status.url}</dd>
            </div>
            <div>
              <dt>当前状态</dt>
              <dd>
                {status.connected
                  ? `已连接${status.peer ? ` · ${status.peer}` : ""}`
                  : "等待浏览器扩展连接"}
              </dd>
            </div>
            {status.frames > 0 && (
              <div>
                <dt>画面</dt>
                <dd>
                  {status.width > 0 && status.height > 0
                    ? `${status.width}×${status.height} · `
                    : ""}
                  {status.frames} 帧 · {status.keyframes} 关键帧
                </dd>
              </div>
            )}
          </dl>

          <div className="cast-actions">
            <button type="button" className="button button--small" onClick={onPresent}>
              <FullscreenIcon size={13} />
              全屏观看
            </button>
            <button className="button button--small button--ghost" onClick={() => void copy()}>
              复制地址
            </button>
            <button
              className="button button--small button--ghost"
              onClick={() =>
                void openExtensionFolder().then(
                  () => onNotify("已打开扩展目录", "info"),
                  (reason) => onNotify(displayError(reason), "error"),
                )
              }
            >
              打开扩展目录
            </button>
          </div>

          <div className={`cast-preview-wrap${status.connected ? "" : " is-waiting"}`}>
            <MirrorSurface view={mirror} onHost={onMirrorHost} onClick={onPresent} />
            {!status.connected && (
              // The code only ever covers an empty preview: as soon as the
              // sender attaches it leaves the layout, so the picture and the
              // compositor never have to share the box with it.
              <div className="cast-code-card">
                <small>投屏码</small>
                <strong>{status.code.split("").join(" ")}</strong>
                <span>在浏览器扩展里输入这 6 位数字</span>
              </div>
            )}
          </div>

          <div className="cast-stats">
            <span>
              <strong>{status.frames}</strong>帧
            </span>
            <span>
              <strong>{formatBytes(status.videoBytes)}</strong>视频
            </span>
            <span>
              <strong>{formatBytes(status.audioBytes)}</strong>音频
            </span>
          </div>

          {status.frames === 0 && (
            <p className="cast-note">
              浏览器打开扩展管理页，启用开发者模式并“加载已解压的扩展”，选择上面的目录；
              扩展会自动找到本机，输入上面的投屏码即可投放当前标签页或屏幕。
            </p>
          )}
        </div>
      )}
    </section>
  );
}

function DlnaCard({
  status,
  busy,
  onToggle,
  onPresent,
}: {
  status: DlnaStatus | null;
  busy: boolean;
  onToggle: (on: boolean) => void;
  onPresent: () => void;
}) {
  return (
    <section className="settings-section cast-card">
      <div className="cast-card__head">
        <span className="cast-card__glyph is-dlna">
          <CastGlyph size={21} />
        </span>
        <div className="cast-card__copy">
          <div className="cast-card__title">
            <h3>DLNA 视频</h3>
            <span className={`badge ${status ? "badge--live" : "badge--muted"}`}>
              {busy ? "处理中" : status ? "已开启" : "已关闭"}
            </span>
          </div>
          <p>电视与视频应用通过 UPnP 投放的媒体，画面直接铺在大屏舞台上，播放、暂停、进度和音量在本机也能控制。</p>
        </div>
        <Switch checked={Boolean(status)} onChange={onToggle} label="DLNA 视频接收" />
      </div>

      {status && (
        <div className="cast-card__body">
          <dl className="cast-facts">
            <div>
              <dt>设备名称</dt>
              <dd title={status.name}>{status.name}</dd>
            </div>
            <div>
              <dt>描述地址</dt>
              <dd title={status.location}>{status.location}</dd>
            </div>
            <div>
              <dt>当前状态</dt>
              <dd>
                {PHASE_LABELS[status.phase] ?? status.phase}
                {status.duration > 0 &&
                  ` · ${formatTime(status.position)} / ${formatTime(status.duration)}`}
              </dd>
            </div>
          </dl>
          {status.error && <p className="cast-note is-warning">{status.error}</p>}
          <div className="cast-actions">
            <button type="button" className="button button--small" onClick={onPresent}>
              <FullscreenIcon size={13} />
              全屏观看
            </button>
          </div>
        </div>
      )}
    </section>
  );
}

function formatTime(seconds: number) {
  const total = Math.max(0, Math.round(seconds));
  const minutes = Math.floor(total / 60);
  return `${minutes}:${String(total % 60).padStart(2, "0")}`;
}

function formatBytes(bytes: number) {
  if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  if (bytes >= 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${bytes} B`;
}

async function copyText(text: string) {
  if (navigator.clipboard?.writeText) {
    await navigator.clipboard.writeText(text);
    return;
  }
  const area = document.createElement("textarea");
  area.value = text;
  area.style.position = "fixed";
  area.style.opacity = "0";
  document.body.appendChild(area);
  area.select();
  const copied = document.execCommand("copy");
  area.remove();
  if (!copied) throw new Error("复制失败");
}

function displayError(reason: unknown) {
  if (typeof reason === "string") return reason;
  if (reason instanceof Error) return reason.message;
  return "发生未知错误";
}
