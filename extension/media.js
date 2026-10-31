// The hub door for the 文件 and 片库 panes.
//
// It lives in its own file for the same reason discovery.js does: this is the
// wire, and the popup is only the timing. Everything sent here is the action
// name and JSON body the big screen sends over IPC — one dispatcher in Rust
// serves both doors — so a control that exists in this popup is a control the
// receiver actually has.
//
// Wrapped, and published as one global, because popup.js and background.js load
// into the same realm as this file and a top-level `const api` would take the
// whole popup down with it.

(() => {
  const HORIZON_MIRROR_PORT = 9407;
  const INFO_BUDGET_MS = 1500;
  const API_BUDGET_MS = 6000;

  // "ws://192.168.0.52:9407/stream?token=123456" or "192.168.0.52:9407" or
  // "192.168.0.52" — the address the operator sees is not always the address with
  // a port on it, and the hub lives on a third port entirely.
  function authority(value) {
    return String(value ?? "")
      .trim()
      .replace(/^(wss?|https?):\/\//i, "")
      .split(/[/?#]/)[0];
  }

  function ipOf(value) {
    return authority(value).split(":")[0];
  }

  function portOf(value, fallback) {
    const parts = authority(value).split(":");
    const port = parts.length > 1 ? Number(parts[1]) : NaN;
    return Number.isInteger(port) && port > 0 && port < 65536 ? port : fallback;
  }

  async function readInfo(mirror) {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), INFO_BUDGET_MS);
    try {
      const response = await fetch(`http://${mirror}/info`, {
        signal: controller.signal,
        cache: "no-store",
      });
      if (!response.ok) return null;
      const info = await response.json().catch(() => null);
      return info?.service === "horizon-browser" ? info : null;
    } catch {
      return null;
    } finally {
      clearTimeout(timer);
    }
  }

  /**
   * Turn whatever the operator picked — a discovered receiver or a typed address —
   * into the hub's base URL.
   *
   * A discovered receiver already named its hub port in /info, so the common path
   * costs nothing. A hand-typed address only names the mirror port, so it has to
   * be asked once; the popup keeps the answer until the choice changes. An honest
   * failure beats a guess: reaching for 7878 anyway would send the 片库 pane's
   * traffic to whatever else listens there.
   */
  async function resolveBase({ receiver = null, manual = "" } = {}) {
    const typed = manual.trim();
    // The hand-typed address wins even when a discovered receiver is still
    // selected: that is exactly what the 投屏 page does with it, and if the hub
    // door disagreed the operator would manage one room's library while
    // mirroring to a different machine.
    const mirror = typed
      ? `${ipOf(typed)}:${portOf(typed, HORIZON_MIRROR_PORT)}`
      : (receiver?.host ?? "");
    if (!mirror) return { base: "", mirror: "", reason: "no-receiver" };
    if (!typed && receiver?.hub) {
      return {
        base: `http://${ipOf(receiver.host)}:${receiver.hub}`,
        mirror,
        name: receiver.name ?? "",
        reason: "",
      };
    }
    const info = await readInfo(mirror);
    if (!info) return { base: "", mirror, reason: "unreachable" };
    const hub = Number.isInteger(info.hub) && info.hub > 0 ? info.hub : null;
    if (!hub) return { base: "", mirror, name: info.name || "", reason: "no-hub" };
    return { base: `http://${ipOf(mirror)}:${hub}`, mirror, name: info.name || "", reason: "" };
  }

  /** `GET /api/media/<action>` with no args, `POST` with them. */
  async function api(base, action, args) {
    const url = `${base}/api/media/${encodeURIComponent(action)}`;
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), API_BUDGET_MS);
    const init =
      args === undefined
        ? { cache: "no-store", signal: controller.signal }
        : {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify(args ?? {}),
            cache: "no-store",
            signal: controller.signal,
          };
    try {
      const response = await fetch(url, init);
      const text = await response.text();
      let body = null;
      try {
        body = text ? JSON.parse(text) : null;
      } catch {
        body = null;
      }
      if (!response.ok) {
        throw new Error(body?.error ?? `接收端返回 ${response.status}`);
      }
      return body;
    } catch (error) {
      if (error?.name === "AbortError") throw new Error(`读取「${action}」超时`);
      throw error;
    } finally {
      clearTimeout(timer);
    }
  }

  /**
   * Stream files to the receiver. `endpoint` is `/upload` for the inbox and
   * `/api/media/upload` for the projection library; both take the same multipart
   * body, and neither is timed out here — a body limit is the server's business,
   * a two-hour recording is not a client error.
   */
  async function upload(base, endpoint, files) {
    const form = new FormData();
    for (const file of files) form.append("file", file, file.name);
    const response = await fetch(`${base}${endpoint}`, {
      method: "POST",
      body: form,
      cache: "no-store",
    });
    const text = await response.text();
    if (!response.ok) {
      let detail = text.trim();
      try {
        detail = JSON.parse(text).error ?? detail;
      } catch {}
      throw new Error(detail || `接收端返回 ${response.status}`);
    }
    if (endpoint.startsWith("/api/")) {
      try {
        return JSON.parse(text).items ?? [];
      } catch {
        return [];
      }
    }
    return text.trim();
  }

  globalThis.horizonMedia = { resolveBase, readInfo, api, upload, ipOf, portOf, HORIZON_MIRROR_PORT };

  // --------------------------------------------------------------- DLNA SOAP
  //
  // The hub's own DLNA renderer listens on port 8290. The extension uploads a
  // file to the library, then sends its URL via SOAP SetAVTransportURI+Play.
  // The renderer replaces the queue with that one item and plays it.

  const DLNA_PORT = 8290;
  const AVT_SERVICE = "urn:schemas-upnp-org:service:AVTransport:1";
  const RCS_SERVICE = "urn:schemas-upnp-org:service:RenderingControl:1";

  function dlnaBase(hubBase, port) {
    const ip = ipOf(hubBase.replace(/^http:\/\//, ""));
    return `http://${ip}:${port}`;
  }

  function escapeXml(value) {
    return String(value ?? "")
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&apos;");
  }

  function didlLite(title, uri, mime) {
    const klass = mime.startsWith("video/")
      ? "object.item.videoItem"
      : mime.startsWith("audio/")
        ? "object.item.audioItem.musicTrack"
        : "object.item.imageItem.photo";
    return (
      `<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" ` +
      `xmlns:dc="http://purl.org/dc/elements/1.1/" ` +
      `xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">` +
      `<item id="1" parentID="0" restricted="1">` +
      `<dc:title>${escapeXml(title)}</dc:title>` +
      `<upnp:class>${klass}</upnp:class>` +
      `<res protocolInfo="http-get:*:${mime}:*">${escapeXml(uri)}</res>` +
      `</item></DIDL-Lite>`
    );
  }

  function soapEnvelope(service, action, args) {
    return (
      `<?xml version="1.0" encoding="utf-8"?>` +
      `<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" ` +
      `s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">` +
      `<s:Body><u:${action} xmlns:u="${service}">${args}</u:${action}></s:Body></s:Envelope>`
    );
  }

  async function soapAction(base, service, action, args) {
    const url = `${base}/ctl/${service === AVT_SERVICE ? "AVTransport" : "RenderingControl"}`;
    const body = soapEnvelope(service, action, args);
    const response = await fetch(url, {
      method: "POST",
      headers: {
        "Content-Type": `text/xml; charset="utf-8"`,
        SOAPACTION: `"${service}#${action}"`,
      },
      body,
      cache: "no-store",
    });
    if (!response.ok) {
      const text = await response.text().catch(() => "");
      throw new Error(`${action} 失败（HTTP ${response.status}）${text ? `：${text.slice(0, 100)}` : ""}`);
    }
    return response.text();
  }

  function parseTag(xml, name) {
    const match = new RegExp(`<${name}[^>]*>(.*?)</${name}>`, "s").exec(xml);
    return match ? match[1].trim() : "";
  }

  async function setUri(hubBase, port, uri, kind, title) {
    const base = dlnaBase(hubBase, port);
    const mime = kind === "audio" ? "audio/mpeg" : kind === "image" ? "image/jpeg" : "video/mp4";
    const metadata = didlLite(title, uri, mime);
    const args =
      `<InstanceID>0</InstanceID>` +
      `<CurrentURI>${escapeXml(uri)}</CurrentURI>` +
      `<CurrentURIMetaData>${escapeXml(metadata)}</CurrentURIMetaData>`;
    await soapAction(base, AVT_SERVICE, "SetAVTransportURI", args);
  }

  async function play(hubBase, port) {
    const base = dlnaBase(hubBase, port);
    await soapAction(base, AVT_SERVICE, "Play", `<InstanceID>0</InstanceID><Speed>1</Speed>`);
  }

  async function pause(hubBase, port) {
    const base = dlnaBase(hubBase, port);
    await soapAction(base, AVT_SERVICE, "Pause", `<InstanceID>0</InstanceID>`);
  }

  async function stop(hubBase, port) {
    const base = dlnaBase(hubBase, port);
    await soapAction(base, AVT_SERVICE, "Stop", `<InstanceID>0</InstanceID>`);
  }

  async function getPosition(hubBase, port) {
    const base = dlnaBase(hubBase, port);
    try {
      const posXml = await soapAction(base, AVT_SERVICE, "GetPositionInfo", `<InstanceID>0</InstanceID>`);
      const transportXml = await soapAction(base, AVT_SERVICE, "GetTransportInfo", `<InstanceID>0</InstanceID>`);
      const state = parseTag(transportXml, "CurrentTransportState");
      const relTime = parseTag(posXml, "RelTime");
      const trackDuration = parseTag(posXml, "TrackDuration");
      return {
        state,
        seconds: parseHms(relTime),
        duration: parseHms(trackDuration),
      };
    } catch {
      return null;
    }
  }

  function parseHms(value) {
    const parts = (value || "00:00:00").split(":");
    if (parts.length !== 3) return 0;
    const h = Number(parts[0]) || 0;
    const m = Number(parts[1]) || 0;
    const s = Number(parts[2]) || 0;
    return h * 3600 + m * 60 + s;
  }

  globalThis.horizonDlna = { setUri, play, pause, stop, getPosition, DLNA_PORT };
})();
