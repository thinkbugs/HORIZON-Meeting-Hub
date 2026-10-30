// One-off generator: lifts the constant tables out of the Airplayer reference
// sources so the Rust port does not hand-transcribe 1,700 hex bytes.
const fs = require("fs");

// Usage: node tools/gen_fairplay_tables.cjs <path-to-OmgHaxHex.cs>
const source = process.argv[2];
if (!source) {
  console.error("用法：node tools/gen_fairplay_tables.cjs <OmgHaxHex.cs 路径>");
  process.exit(1);
}
const src = fs.readFileSync(source, "utf8");

function literal(name) {
  const re = new RegExp(name + "\\s*=\\s*new byte(\\[\\d*\\])?\\s*\\{([\\s\\S]*?)\\};");
  const m = src.match(re);
  if (!m) throw new Error("missing " + name);
  return m[2].match(/0x[0-9a-fA-F]+/g).map((s) => parseInt(s, 16));
}

function rows(name, per) {
  const re = new RegExp(name + "\\s*=\\s*new byte\\[4\\]\\[\\]\\s*\\{([\\s\\S]*?)\\n\\s*\\};");
  const body = src.match(re)[1];
  const out = [];
  let cur = [];
  for (const line of body.split("\n")) {
    cur.push(...(line.match(/0x[0-9a-fA-F]+/g) || []).map((s) => parseInt(s, 16)));
    while (cur.length >= per) {
      out.push(cur.slice(0, per));
      cur = cur.slice(per);
    }
  }
  if (cur.length) throw new Error(name + " remainder " + cur.length);
  return out;
}

const parts = [];
const hex = (n) => "0x" + n.toString(16).padStart(2, "0");
function emit(name, arr) {
  parts.push(`pub const ${name}: [u8; ${arr.length}] = [${arr.map(hex).join(", ")}];`);
}

emit("STATIC_SOURCE1", literal("StaticSource1"));
emit("STATIC_SOURCE2", literal("StaticSource2"));
emit("INITIAL_SESSION_KEY", literal("InitialSessionKey"));
emit("DEFAULT_SAP", literal("DefaultSap"));
emit("INDEX_M_ANGLE", literal("IndexMAngle"));
emit("T_KEY", literal("TKey"));
emit("X_KEY", literal("XKey"));
emit("Z_KEY", literal("ZKey"));

const mk = rows("MessageKey", 144);
const mi = rows("MessageIv", 16);
parts.push("pub const MESSAGE_KEY: [[u8; 144]; 4] = [");
for (const r of mk) parts.push(`    [${r.map(hex).join(", ")}],`);
parts.push("];");
parts.push("pub const MESSAGE_IV: [[u8; 16]; 4] = [");
for (const r of mi) parts.push(`    [${r.map(hex).join(", ")}],`);
parts.push("];");

fs.writeFileSync("crates/horizon-cast/src/airplay/fairplay_tables.rs", parts.join("\n") + "\n");
console.log("static1", literal("StaticSource1").length);
console.log("static2", literal("StaticSource2").length);
console.log("sap", literal("DefaultSap").length);
console.log("mk", mk.length, mk[0].length, "mi", mi.length, mi[0].length);
