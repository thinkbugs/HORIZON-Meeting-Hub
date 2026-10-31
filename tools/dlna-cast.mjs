// DLNA controller used to drive the desktop DMR during verification.
//   node tools/dlna-cast.mjs <dd.xml url> <media url>
//   node tools/dlna-cast.mjs <dd.xml url> --action Play|Pause|Stop
//   node tools/dlna-cast.mjs <dd.xml url> --status
const [location, ...rest] = process.argv.slice(2);
if (!location) {
  console.error("usage: dlna-cast <dd.xml> [<media-url> | --action NAME | --status]");
  process.exit(2);
}

const xml = await (await fetch(location)).text();
const service = (type) => {
  const re = /<service>([\s\S]*?)<\/service>/g;
  for (const m of xml.matchAll(re)) {
    if (m[1].includes(type)) {
      const url = /<controlURL>([^<]*)<\/controlURL>/.exec(m[1])?.[1] ?? "";
      const base = new URL(location);
      return new URL(url.startsWith("/") ? url : `/${url}`, base).href;
    }
  }
  throw new Error(`service ${type} not found`);
};

const av = service("urn:schemas-upnp-org:service:AVTransport:1");

async function action(name, args = {}) {
  const body = Object.entries(args)
    .map(([k, v]) => `<${k}>${v}</${k}>`)
    .join("");
  const envelope = `<?xml version="1.0" encoding="utf-8"?` +
    `<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">` +
    `<s:Body><u:${name} xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">${body}</u:${name}></s:Body></s:Envelope>`;
  const res = await fetch(av, {
    method: "POST",
    headers: {
      "Content-Type": 'text/xml; charset="utf-8"',
      SOAPAction: `"urn:schemas-upnp-org:service:AVTransport:1#${name}"`,
    },
    body: envelope,
  });
  const text = await res.text();
  const fault = /<faultstring>([\s\S]*?)<\/faultstring>/.exec(text)?.[1];
  console.log(`${name} -> HTTP ${res.status}${fault ? ` fault=${fault}` : ""}`);
  if (name === "GetTransportInfo" || name === "GetPositionInfo") {
    for (const tag of ["CurrentTransportState", "CurrentSpeed", "RelTime", "TrackDuration"]) {
      const v = new RegExp(`<${tag}>([^<]*)</${tag}>`).exec(text)?.[1];
      if (v !== undefined) console.log(`  ${tag}=${v}`);
    }
  }
  return text;
}

if (rest[0] === "--status") {
  await action("GetTransportInfo", { InstanceID: 0 });
  await action("GetPositionInfo", { InstanceID: 0 });
} else if (rest[0] === "--action") {
  await action(rest[1], { InstanceID: 0 });
} else {
  const uri = rest[0];
  const meta = `<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"><item id="0" parentID="-1" restricted="1"><dc:title>verification</dc:title><upnp:class>object.item.videoItem</upnp:class><res protocolInfo="http-get:*:*:*" size="0">${uri}</res></item></DIDL-Lite>`;
  await action("SetAVTransportURI", { InstanceID: 0, CurrentURI: uri, CurrentURIMetaData: meta });
  await action("Play", { InstanceID: 0, Speed: "1" });
  await action("GetTransportInfo", { InstanceID: 0 });
}
