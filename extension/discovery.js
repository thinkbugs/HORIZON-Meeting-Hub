// LAN discovery, shared by the service worker (which owns the sweep) and the
// popup (which only reads the cached result). It lives in its own file because
// a popup dies the moment the operator alt-tabs to read the 投屏码 off the big
// screen, and a 254-address sweep must not die with it.

const HORIZON_DISCOVERY_PORT = 9407;
const PROBE_BUDGET_MS = 700;
const SWEEP_CONCURRENCY = 48;
const PROGRESS_INTERVAL_MS = 250;

async function localPrefixes(extraHost) {
  const prefixes = new Set();
  const seen = new Set();
  const interfaces = await new Promise((resolve) => {
    if (!chrome.system?.network?.getNetworkInterfaces) return resolve([]);
    try {
      chrome.system.network.getNetworkInterfaces((list) => {
        resolve(chrome.runtime.lastError ? [] : (list ?? []));
      });
    } catch {
      resolve([]);
    }
  });
  for (const iface of interfaces) {
    if (iface.family !== "IPv4" || !iface.address || iface.address.startsWith("127.")) continue;
    const octets = iface.address.split(".");
    if (octets.length !== 4) continue;
    const prefix = octets.slice(0, 3).join(".");
    if (seen.has(prefix)) continue;
    seen.add(prefix);
    prefixes.add(prefix);
  }
  // A hand-typed address deserves a probe even when its subnet is not one of the
  // machine's own interfaces — that is how a receiver on another VLAN still gets
  // found after the operator wrote it in once.
  if (extraHost) {
    const octets = extraHost.split(".");
    if (octets.length === 4) prefixes.add(octets.slice(0, 3).join("."));
  }
  return [...prefixes];
}

async function candidateHosts(extraHost) {
  const hosts = new Set(["127.0.0.1"]);
  for (const prefix of await localPrefixes(extraHost)) {
    for (let index = 1; index < 255; index += 1) hosts.add(`${prefix}.${index}`);
  }
  if (extraHost && !extraHost.startsWith("127.")) hosts.add(extraHost);
  return [...hosts];
}

async function probe(host) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), PROBE_BUDGET_MS);
  try {
    const response = await fetch(`http://${host}:${HORIZON_DISCOVERY_PORT}/info`, {
      signal: controller.signal,
      cache: "no-store",
    });
    if (response.ok) {
      const info = await response.json().catch(() => null);
      if (info?.service === "horizon-browser") {
        return {
          host: `${host}:${info.port ?? HORIZON_DISCOVERY_PORT}`,
          name: info.name || "HORIZON 接收端",
          paired: Boolean(info.paired),
        };
      }
    }
    // Receivers older than the discovery endpoint answer 404 on /info but still
    // take the stream, and that 404 carries no CORS headers — it is readable only
    // because this extension holds host_permissions. Any other status, or a body
    // that is not ours, stays unlisted: a sweep of 509 addresses on a real office
    // LAN produced exactly the HORIZON hosts and nothing else, so 404 is the one
    // answer worth trusting. The 投屏码 still has to be right to connect.
    if (response.status === 404) {
      return {
        host: `${host}:${HORIZON_DISCOVERY_PORT}`,
        name: "HORIZON 接收端",
        paired: true,
        legacy: true,
      };
    }
    return null;
  } catch {
    return null;
  } finally {
    clearTimeout(timer);
  }
}

function hostRank(receiver, preferred) {
  // The receiver used last outranks everything, a real LAN address outranks
  // loopback, and loopback must never win just by answering first.
  if (receiver.host === preferred) return 0;
  if (receiver.host.startsWith("127.")) return 2;
  return 1;
}

function tier(receiver) {
  // A receiver that identified itself always lists ahead of one recognised only
  // by answering on the mirror port.
  return receiver.legacy ? 1 : 0;
}

function rank(receivers, preferred) {
  return [...receivers].sort(
    (a, b) =>
      tier(a) - tier(b) ||
      hostRank(a, preferred) - hostRank(b, preferred) ||
      a.name.localeCompare(b.name, "zh-Hans-CN"),
  );
}

/**
 * Sweep the machine's own subnets and report every HORIZON receiver found.
 * `onProgress` fires as answers arrive (throttled) and once more when done, so
 * the caller can paint results without waiting out the silent addresses.
 */
async function sweep({ preferred = null, extraHost = "", onProgress } = {}) {
  const queue = [...(await candidateHosts(extraHost))];
  const found = [];
  let selected = preferred && queue.length ? preferred : null;
  let lastPaint = 0;

  const paint = (done) => {
    const now = Date.now();
    if (!done && now - lastPaint < PROGRESS_INTERVAL_MS) return;
    lastPaint = now;
    onProgress?.({ receivers: rank(found, preferred), selected, done });
  };

  const workers = Array.from({ length: Math.min(SWEEP_CONCURRENCY, queue.length) }, async () => {
    while (queue.length) {
      const host = queue.shift();
      const receiver = await probe(host);
      if (!receiver) continue;
      found.push(receiver);
      // The first answer becomes the default, but only while nothing was
      // remembered; the ranked list settles the final choice at the end.
      if (!selected) selected = receiver.host;
      paint(false);
    }
  });
  await Promise.all(workers);

  const receivers = rank(found, preferred);
  selected =
    preferred && receivers.some((item) => item.host === preferred)
      ? preferred
      : (receivers[0]?.host ?? null);
  paint(true);
  return { receivers, selected };
}

globalThis.horizonDiscovery = { sweep, probe, rank, candidateHosts };
