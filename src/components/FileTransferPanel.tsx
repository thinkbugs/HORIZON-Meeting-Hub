import { useCallback, useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { listen } from "@tauri-apps/api/event";
import { CloseIcon, QrCodeIcon, FolderIcon, EditIcon, CheckIcon } from "./Icons";
import {
  isTauri,
  startSnapdropServer,
  setSnapdropDeviceName,
  setSnapdropStorageDir,
  getSnapdropConfig,
  generateSnapdropQr,
  pickSnapdropStorageFolder,
  snapdropFirewallOk,
  allowSnapdropFirewall,
  type SnapdropConfig,
} from "../lib/bridge";

interface FileTransferPanelProps {
  onClose: () => void;
  onNotify: (text: string, tone?: "success" | "error" | "info") => void;
}

const SNAPDROP_PORT = 7878;

interface Incoming {
  name: string;
  progress: number;
}

export function FileTransferPanel({ onClose, onNotify }: FileTransferPanelProps) {
  const [loading, setLoading] = useState(true);
  const [serverUrl, setServerUrl] = useState<string | null>(null);
  const [config, setConfig] = useState<SnapdropConfig | null>(null);
  const [showQr, setShowQr] = useState(false);
  const [qrCode, setQrCode] = useState<string | null>(null);
  const [editingName, setEditingName] = useState(false);
  const [nameInput, setNameInput] = useState("");
  const [incoming, setIncoming] = useState<Incoming | null>(null);
  const [firewallOk, setFirewallOk] = useState<boolean | null>(null);
  const [allowing, setAllowing] = useState(false);
  const [dragPos, setDragPos] = useState({ x: 0, y: 0 });
  const [isDragging, setIsDragging] = useState(false);
  const dragStart = useRef({ x: 0, y: 0 });
  const panelRef = useRef<HTMLDivElement>(null);

  // Start SnapDrop server on mount
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const url = await startSnapdropServer(SNAPDROP_PORT);
        const cfg = await getSnapdropConfig();
        if (!cancelled) {
          setServerUrl(url);
          setConfig(cfg);
          setNameInput(cfg.deviceName);
          setLoading(false);
        }
        snapdropFirewallOk()
          .then((ok) => !cancelled && setFirewallOk(ok))
          .catch(() => !cancelled && setFirewallOk(true));
      } catch (error) {
        if (!cancelled) {
          const reason = error instanceof Error ? error.message : String(error);
          onNotify(`无法启动文件传送服务：${reason}`, "error");
          setLoading(false);
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [onNotify]);

  // Generate QR code when needed
  useEffect(() => {
    if (showQr && serverUrl) {
      generateSnapdropQr()
        .then((base64) => setQrCode(base64))
        .catch(() => onNotify("生成二维码失败", "error"));
    }
  }, [showQr, serverUrl, onNotify]);

  // Progress while the panel is open; the arrival toast lives in App so it also
  // fires when this panel is closed.
  useEffect(() => {
    if (!isTauri) return;
    const unlistenFile = listen<{ name: string }>("snapdrop-file", () => {
      setIncoming(null);
    });
    const unlistenProgress = listen<{ name: string; progress: number }>(
      "snapdrop-progress",
      (event) => setIncoming(event.payload),
    );
    return () => {
      unlistenFile.then((fn) => fn());
      unlistenProgress.then((fn) => fn());
    };
  }, []);

  // Handle device name save
  const handleSaveName = useCallback(async () => {
    const next = nameInput.trim();
    if (next && next !== config?.deviceName) {
      try {
        await setSnapdropDeviceName(next);
        setConfig((prev) => (prev ? { ...prev, deviceName: next } : prev));
        onNotify("设备名已更新", "success");
      } catch {
        onNotify("更新设备名失败", "error");
      }
    }
    setEditingName(false);
  }, [nameInput, config, onNotify]);

  // Handle storage directory change
  const handlePickStorageDir = useCallback(async () => {
    try {
      const selected = await pickSnapdropStorageFolder();
      if (selected && selected !== config?.storageDir) {
        await setSnapdropStorageDir(selected);
        setConfig((prev) => (prev ? { ...prev, storageDir: selected } : prev));
        onNotify("存储目录已更新", "success");
      }
    } catch {
      onNotify("更新存储目录失败", "error");
    }
  }, [config, onNotify]);

  // The public firewall profile blocks inbound by default and the per-user
  // installer never runs elevated, so the rule is requested here on demand.
  const handleAllowFirewall = useCallback(async () => {
    setAllowing(true);
    try {
      const ok = await allowSnapdropFirewall();
      setFirewallOk(ok);
      onNotify(ok ? "已放行防火墙，局域网设备可以访问了" : "规则未生效，请重试", ok ? "success" : "error");
    } catch (error) {
      onNotify(error instanceof Error ? error.message : String(error), "error");
    } finally {
      setAllowing(false);
    }
  }, [onNotify]);

  // Drag handling
  const handleMouseDown = useCallback((e: React.MouseEvent) => {
    if ((e.target as HTMLElement).closest(".file-transfer-header__close, button, input")) return;
    setIsDragging(true);
    dragStart.current = { x: e.clientX - dragPos.x, y: e.clientY - dragPos.y };
  }, [dragPos]);

  useEffect(() => {
    if (!isDragging) return;
    const handleMouseMove = (e: MouseEvent) => {
      setDragPos({
        x: e.clientX - dragStart.current.x,
        y: e.clientY - dragStart.current.y,
      });
    };
    const handleMouseUp = () => setIsDragging(false);
    window.addEventListener("mousemove", handleMouseMove);
    window.addEventListener("mouseup", handleMouseUp);
    return () => {
      window.removeEventListener("mousemove", handleMouseMove);
      window.removeEventListener("mouseup", handleMouseUp);
    };
  }, [isDragging]);

  // Escape key to close
  useEffect(() => {
    const handleKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.stopPropagation();
        onClose();
      }
    };
    window.addEventListener("keydown", handleKey, true);
    return () => window.removeEventListener("keydown", handleKey, true);
  }, [onClose]);

  if (loading) {
    return createPortal(
      <div className="file-transfer-overlay" role="dialog" aria-modal="true">
        <div className="file-transfer-loading">
          <div className="file-transfer-spinner" />
          <p>正在启动 SnapDrop 服务…</p>
        </div>
      </div>,
      document.body,
    );
  }

  return createPortal(
    <div
      className="file-transfer-overlay"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
      role="dialog"
      aria-modal="true"
      aria-labelledby="file-transfer-title"
    >
      <div
        ref={panelRef}
        className="file-transfer-panel"
        style={{
          transform: `translate(${dragPos.x}px, ${dragPos.y}px)`,
          cursor: isDragging ? "grabbing" : "default",
        }}
      >
        {/* ── Header ─────────────────────────────────────────────── */}
        <header
          className="file-transfer-header"
          onMouseDown={handleMouseDown}
          style={{ cursor: isDragging ? "grabbing" : "grab" }}
        >
          <div className="file-transfer-header__copy">
            <span className="file-transfer-header__eyebrow">
              <span className="file-transfer-header__dot" aria-hidden="true" />
              HORIZON
            </span>
            <h2 id="file-transfer-title" className="file-transfer-header__title">
              文件传送
            </h2>
            <p className="file-transfer-header__subtitle">SnapDrop · 局域网 P2P 文件传输</p>
          </div>
          <button
            className="file-transfer-header__close icon-button modal-close"
            onClick={onClose}
            aria-label="关闭文件传送"
            type="button"
          >
            <CloseIcon size={20} />
          </button>
        </header>

        {/* ── Body ───────────────────────────────────────────────── */}
        <div className="file-transfer-body">
          {incoming && (
            <p className="file-transfer-qr-hint" role="status">
              正在接收「{incoming.name}」 {Math.round(incoming.progress * 100)}%
            </p>
          )}
          {firewallOk === false && (
            <div className="file-transfer-firewall" role="alert">
              <div>
                <strong>Windows 防火墙正在拦截局域网访问</strong>
                <p>其他电脑和手机现在打不开上面的地址。放行需要确认一次管理员授权。</p>
              </div>
              <button type="button" onClick={handleAllowFirewall} disabled={allowing}>
                {allowing ? "正在放行…" : "放行"}
              </button>
            </div>
          )}
          {/* Device name section */}
          <div className="file-transfer-section">
            <label className="file-transfer-label">设备名称</label>
            <div className="file-transfer-device-name">
              {editingName ? (
                <div className="file-transfer-name-editor">
                  <input
                    type="text"
                    value={nameInput}
                    onChange={(e) => setNameInput(e.target.value)}
                    onKeyDown={(e) => e.key === "Enter" && handleSaveName()}
                    className="file-transfer-name-input"
                    autoFocus
                    maxLength={30}
                  />
                  <button
                    className="file-transfer-name-save"
                    onClick={handleSaveName}
                    type="button"
                  >
                    <CheckIcon size={16} />
                  </button>
                </div>
              ) : (
                <div className="file-transfer-name-display">
                  <span>{config?.deviceName || "未命名设备"}</span>
                  <button
                    className="file-transfer-name-edit"
                    onClick={() => setEditingName(true)}
                    type="button"
                    aria-label="编辑设备名"
                  >
                    <EditIcon size={14} />
                  </button>
                </div>
              )}
            </div>
          </div>

          {/* QR Code / Address section */}
          <div className="file-transfer-section">
            <div className="file-transfer-section-header">
              <label className="file-transfer-label">连接到其他设备</label>
              <button
                className={`file-transfer-qr-toggle${showQr ? " is-active" : ""}`}
                onClick={() => setShowQr(!showQr)}
                type="button"
              >
                <QrCodeIcon size={16} />
                <span>{showQr ? "输入地址" : "扫码连接"}</span>
              </button>
            </div>

            {showQr ? (
              <div className="file-transfer-qr-section">
                <p className="file-transfer-qr-hint">手机扫码，即可发送文件到本机</p>
                {qrCode ? (
                  <div className="file-transfer-qr-code">
                    <img src={`data:image/png;base64,${qrCode}`} alt="SnapDrop QR Code" />
                  </div>
                ) : (
                  <div className="file-transfer-qr-loading">生成二维码中…</div>
                )}
                <p className="file-transfer-qr-url">{serverUrl}</p>
              </div>
            ) : (
              <div className="file-transfer-address-section">
                <p className="file-transfer-address-hint">
                  在其他电脑浏览器打开以下地址，即可传输文件
                </p>
                <div className="file-transfer-address-box">
                  <code>{serverUrl}</code>
                  <button
                    className="file-transfer-address-copy"
                    onClick={() => {
                      navigator.clipboard.writeText(serverUrl || "");
                      onNotify("地址已复制到剪贴板", "success");
                    }}
                    type="button"
                  >
                    复制
                  </button>
                </div>
                {serverUrl?.includes("127.0.0.1") && (
                  <p className="file-transfer-address-hint">
                    本机没有找到局域网地址，请确认已连接 Wi-Fi 或有线网络
                  </p>
                )}
              </div>
            )}
          </div>

          {/* Storage directory section */}
          <div className="file-transfer-section">
            <label className="file-transfer-label">存储目录</label>
            <div className="file-transfer-storage">
              <div className="file-transfer-storage-path">
                <FolderIcon size={16} />
                <span>{config?.storageDir || "未设置"}</span>
              </div>
              <button
                className="file-transfer-storage-change"
                onClick={handlePickStorageDir}
                type="button"
              >
                更改
              </button>
            </div>
          </div>

          {/* Instructions */}
          <div className="file-transfer-instructions">
            <h4>使用说明</h4>
            <ul>
              <li>手机扫码或电脑输入地址，即可连接到本机</li>
              <li>连接后，选择文件即可发送到本机</li>
              <li>文件将自动保存到上方设置的存储目录</li>
              <li>同一局域网内的设备会自动发现</li>
            </ul>
          </div>
        </div>
      </div>
    </div>,
    document.body,
  );
}
