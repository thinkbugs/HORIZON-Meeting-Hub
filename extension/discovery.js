// LAN discovery, shared by the service worker (which owns the sweep) and the
// popup (which only reads the cached result). It lives in its own file because
// a popup dies the moment the operator alt-tabs to read the 投屏码 off the big
// screen, and a 254-address sweep must not die with it.

const HORIZON_DISCOVERY_PORT = 9407;
const PROBE_BUDGET_MS = 700;
// Measured on a three-interface machine (192.168.0.x, 192.168.137.x, 172.19.224.x
// — 762 hosts). A probe aborted at the 700 ms budget does not cancel its TCP
// connect: Chrome keeps the socket in the pool, so every dead host in the /24
// leaves one hanging connect behind, and at 48 workers the pool saturates before
// the sweep ends. Right after such a sweep the page's own reads answered in
// 3–6 s or not at all while `curl` from the same machine answered in 40 ms —
// including reads to 127.0.0.1, which is what proves it is the browser's pool and
// not the receiver. At 16 the page stayed healthy (all five addresses in ≤51 ms);
// at 24 one read still timed out. Smaller is slower, not worse: the worker keeps
// sweeping after the popup paints the receiver it already knows.
const SWEEP_CONCURRENCY = 16;
const PROGRESS_INTERVAL_MS = 250;
// 一个 /24 扫完之后，Chrome 里还挂着那些被 abort 的 connect，页面在几秒内打不开
// 任何新连接（node 侧同一时刻 24 ms）。实测：254 个地址、16 并发，扫完立刻读
// 9407 与 7878 三次全部 3 s 超时，约 10 s 后恢复。操作员扫完的下一个动作就是点
// 投放，所以"扫完"要以选中的那台此刻答得出为准，而不是以最后一个探针返回为准。
const SETTLE_ATTEMPTS = 12;
const SETTLE_DELAY_MS = 500;
// Probing stops here for the length of the pool's recovery window: a sweep that
// starts sooner aborts every probe and reports only what loopback answered.
const SWEEP_COOLDOWN_MS = 12000;
let lastSweepEndedAt = 0;

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
  // When the browser reports no interfaces (some Chrome builds / enterprise
  // policies), fall back to the three most common private /24 ranges so a
  // receiver on a typical home or office LAN is still reachable.
  if (prefixes.size === 0) {
    for (const fallback of ["192.168.0", "192.168.1", "10.0.0"]) {
      prefixes.add(fallback);
    }
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

async function candidateHosts(extraHost, preferred) {
  const hosts = new Set();
  // The receiver this room used last answers in milliseconds while a full sweep
  // of three /24s takes half a minute at a concurrency the browser's socket pool
  // can survive, so it is probed before anything else: the popup has a receiver to
  // paint while the worker is still grinding through the silent addresses.
  const remembered = String(preferred ?? "").split(":")[0];
  if (/^\d{1,3}(\.\d{1,3}){3}$/.test(remembered)) hosts.add(remembered);
  hosts.add("127.0.0.1");
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
          // The hub port is the receiver's to know: 7878 is today's default, and a
          // room that moved it would otherwise leave the 片库 pane pointed at a
          // port that answers something else. Null = this build has no library.
          hub: Number.isInteger(info.hub) && info.hub > 0 ? info.hub : null,
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

/** Answer once more whether the chosen receiver can be reached right now. */
async function reachable(host) {
  for (let attempt = 0; attempt < SETTLE_ATTEMPTS; attempt += 1) {
    const receiver = await probe(String(host).split(":")[0]);
    if (receiver) return true;
    await new Promise((resolve) => setTimeout(resolve, SETTLE_DELAY_MS));
  }
  return false;
}

/**
 * Sweep the machine's own subnets and report every HORIZON receiver found.
 * `onProgress` fires as answers arrive (throttled) and once more when done, so
 * the caller can paint results without waiting out the silent addresses.
 */
async function sweep({ preferred = null, extraHost = "", onProgress } = {}) {
  const cooling = SWEEP_COOLDOWN_MS - (Date.now() - lastSweepEndedAt);
  if (lastSweepEndedAt && cooling > 0) {
    await new Promise((resolve) => setTimeout(resolve, cooling));
  }
  const queue = [...(await candidateHosts(extraHost, preferred))];
  const found = [];
  const total = queue.length;
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
  // The recovery window starts when probing stops, not when the settle below ends.
  lastSweepEndedAt = Date.now();

  const receivers = rank(found, preferred);
  selected =
    preferred && receivers.some((item) => item.host === preferred)
      ? preferred
      : (receivers[0]?.host ?? null);
  // A sweep of more than a couple of workers is what leaves the hanging connects
  // behind; a short one (the remembered host plus loopback) cannot starve the
  // pool, so it must not pay the wait.
  if (selected && total > SWEEP_CONCURRENCY * 2) await reachable(selected);
  paint(true);
  return { receivers, selected };
}

globalThis.horizonDiscovery = { sweep, probe, rank, candidateHosts };
