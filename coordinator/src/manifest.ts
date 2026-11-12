// Signed builds (PROTOCOL 2): checks a build's manifest, files and signature in R2 before it is
// registered. The coordinator never holds the secret key; with RELEASE_PUBKEYS set it also
// verifies the minisign signature, which agents verify again on every PC.

import { BUILD_RE, bad, cmpSemver, hex, isSemver, sha256Hex } from "./util";

export interface ManifestFile {
  path: string;
  sha256: string;
  size: number;
}

export interface Manifest {
  manifest: 1;
  kind: "agent" | "worker" | "runtime";
  version: string;
  created?: number;
  game?: string;
  engine_commit?: string;
  core_commit?: string;
  profile?: string;
  aw?: number;
  zero_onnx?: number[];
  min_agent?: string;
  requires_runtime?: string;
  entry?: string;
  files: ManifestFile[];
}

export interface VerifiedBuild {
  build_id: string;
  manifest: Manifest;
  text: string;
  size: number;
  sig_verified: boolean;
}

const MAX_MANIFEST = 1 << 20;
const PATH_RE = /^[A-Za-z0-9._\-]+(\/[A-Za-z0-9._\-]+)*$/;

export function parseManifest(text: string, problems: string[]): Manifest | null {
  let m: any;
  try {
    m = JSON.parse(text);
  } catch {
    problems.push("manifest.json is not JSON");
    return null;
  }
  if (!m || typeof m !== "object") {
    problems.push("manifest.json is not an object");
    return null;
  }
  if (m.manifest !== 1) problems.push("manifest must be 1");
  if (!["agent", "worker", "runtime"].includes(m.kind)) problems.push("kind must be agent, worker or runtime");
  if (typeof m.version !== "string" || !m.version || m.version.length > 64) problems.push("version must be a short string");
  if (m.kind === "agent" && typeof m.version === "string" && !isSemver(m.version)) problems.push("an agent's version must be semver");
  if (m.min_agent !== undefined && (typeof m.min_agent !== "string" || !isSemver(m.min_agent))) problems.push("min_agent must be semver");
  if (m.kind === "worker") {
    if (typeof m.game !== "string" || !m.game) problems.push("a worker needs game");
    if (!Number.isInteger(m.aw)) problems.push("a worker needs aw (the agent-worker protocol it speaks)");
  }
  if ((m.kind === "worker" || m.kind === "agent") && typeof m.entry !== "string") problems.push(`${m.kind} needs entry`);
  if (!Array.isArray(m.files) || m.files.length === 0 || m.files.length > 2000) {
    problems.push("files must be a non-empty list");
    return problems.length ? null : m;
  }
  const seen = new Set<string>();
  for (const f of m.files) {
    if (!f || typeof f.path !== "string" || !PATH_RE.test(f.path) || f.path.split("/").some((s: string) => s === "." || s === "..")) {
      problems.push(`bad file path ${JSON.stringify(f?.path)}`);
      continue;
    }
    if (f.path === "manifest.json" || f.path === "manifest.json.minisig") problems.push(`file ${f.path} would overwrite the manifest`);
    if (seen.has(f.path)) problems.push(`file ${f.path} listed twice`);
    seen.add(f.path);
    if (typeof f.sha256 !== "string" || !/^[0-9a-f]{64}$/.test(f.sha256)) problems.push(`file ${f.path}: bad sha256`);
    if (!Number.isSafeInteger(f.size) || f.size < 0) problems.push(`file ${f.path}: bad size`);
  }
  if (typeof m.entry === "string" && !seen.has(m.entry)) problems.push(`entry ${m.entry} is not among the files`);
  return problems.length ? null : (m as Manifest);
}

async function r2Sha256(obj: R2Object, bucket: R2Bucket, key: string): Promise<string | null> {
  if (obj.checksums.sha256) return hex(obj.checksums.sha256);
  const body = await bucket.get(key);
  if (!body) return null;
  const ds = new crypto.DigestStream("SHA-256");
  await body.body.pipeTo(ds);
  return hex(await ds.digest);
}

// Checks presence, sizes and hashes of everything under builds/<id>/, and the signature.
export async function verifyBuild(bucket: R2Bucket, buildId: string, pubkeys: string): Promise<VerifiedBuild> {
  if (!BUILD_RE.test(buildId)) throw bad("build_id must be 24 lowercase hex chars");
  const problems: string[] = [];
  const mobj = await bucket.get(`builds/${buildId}/manifest.json`);
  if (!mobj) throw bad(`builds/${buildId}/manifest.json is not uploaded`);
  if (mobj.size > MAX_MANIFEST) throw bad("manifest.json is larger than 1 MB");
  const bytes = new Uint8Array(await mobj.arrayBuffer());
  const digest = await sha256Hex(bytes);
  if (digest.slice(0, 24) !== buildId) {
    throw bad(`build_id must be the first 24 hex chars of sha256(manifest.json) = ${digest.slice(0, 24)}`);
  }
  const text = new TextDecoder().decode(bytes);
  const m = parseManifest(text, problems);
  if (!m) throw bad("manifest.json is not a valid manifest/1", { problems });

  let size = 0;
  for (const f of m.files) {
    const key = `builds/${buildId}/${f.path}`;
    const head = await bucket.head(key);
    if (!head) {
      problems.push(`${f.path}: not uploaded`);
      continue;
    }
    size += f.size;
    if (head.size !== f.size) {
      problems.push(`${f.path}: ${head.size} bytes, manifest says ${f.size}`);
      continue;
    }
    const sha = await r2Sha256(head, bucket, key);
    if (sha !== f.sha256) problems.push(`${f.path}: sha256 ${sha}, manifest says ${f.sha256}`);
  }

  const sigObj = await bucket.get(`builds/${buildId}/manifest.json.minisig`);
  let sigVerified = false;
  if (!sigObj) problems.push("manifest.json.minisig is not uploaded");
  else {
    const keys = pubkeys
      .split(",")
      .map((k) => k.trim())
      .filter(Boolean);
    if (keys.length) {
      const sig = await sigObj.text();
      const why = await verifyMinisign(bytes, sig, keys, "RELEASE_PUBKEYS");
      if (why) problems.push(`signature: ${why}`);
      else {
        const trusted = trustedComment(sig) ?? "";
        if (!trusted.split(/\s+/).includes(`build_id=${buildId}`)) problems.push(`signature: trusted comment lacks build_id=${buildId}`);
        if (!trusted.split(/\s+/).includes(`kind=${m.kind}`)) problems.push(`signature: trusted comment lacks kind=${m.kind}`);
        sigVerified = problems.length === 0;
      }
    }
  }
  if (problems.length) throw bad(`build ${buildId} failed its checks`, { problems });
  return { build_id: buildId, manifest: m, text, size, sig_verified: sigVerified };
}

export function minAgentOk(agent: string, minAgent: string | null | undefined): boolean {
  return !minAgent || cmpSemver(agent, minAgent) >= 0;
}

// ---------- minisign ----------

function b64(s: string): Uint8Array {
  const bin = atob(s.trim());
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

function trustedComment(sig: string): string | null {
  const line = sig.split(/\r?\n/).find((l) => l.startsWith("trusted comment: "));
  return line ? line.slice("trusted comment: ".length) : null;
}

async function ed25519Verify(pub: Uint8Array, sig: Uint8Array, msg: Uint8Array): Promise<boolean> {
  const key = await crypto.subtle.importKey("raw", pub, { name: "Ed25519" }, false, ["verify"]);
  return crypto.subtle.verify({ name: "Ed25519" }, key, sig, msg);
}

// The key id (16 hex chars, as minisign prints it: big-endian) of a .minisig file, or null.
export function minisignKeyId(sig: string): string | null {
  try {
    const s = b64(sig.split(/\r?\n/)[1] ?? "");
    if (s.length !== 74) return null;
    return [...s.slice(2, 10)].reverse().map((b) => b.toString(16).padStart(2, "0")).join("").toUpperCase();
  } catch {
    return null;
  }
}

// null if `sig` (a .minisig file) is a valid signature of `msg` by one of `pubkeys` (the base64
// line of minisign.pub), else why not. Supports legacy ("Ed") and prehashed ("ED") signatures.
// `listName` names the key list in the answer: release keys sign builds (RELEASE_PUBKEYS), data
// keys sign definitions (DATA_PUBKEYS), and each check is given only its own list.
export async function verifyMinisign(msg: Uint8Array, sig: string, pubkeys: string[], listName = "RELEASE_PUBKEYS"): Promise<string | null> {
  const lines = sig.split(/\r?\n/);
  if (lines.length < 4 || !lines[0].startsWith("untrusted comment:") || !lines[2].startsWith("trusted comment: ")) {
    return "not a minisign signature file";
  }
  let s: Uint8Array;
  let global: Uint8Array;
  try {
    s = b64(lines[1]);
    global = b64(lines[3]);
  } catch {
    return "bad base64";
  }
  if (s.length !== 74 || global.length !== 64) return "bad signature length";
  const alg = String.fromCharCode(s[0], s[1]);
  if (alg !== "Ed" && alg !== "ED") return `unknown signature algorithm ${alg}`;
  const keyId = s.slice(2, 10);
  const signature = s.slice(10);
  for (const k of pubkeys) {
    let pk: Uint8Array;
    try {
      pk = b64(k);
    } catch {
      continue;
    }
    if (pk.length !== 42 || pk[0] !== 0x45 || pk[1] !== 0x64) continue;
    if (!pk.slice(2, 10).every((b, i) => b === keyId[i])) continue;
    const pub = pk.slice(10);
    const signed = alg === "ED" ? blake2b512(msg) : msg;
    if (!(await ed25519Verify(pub, signature, signed))) return "signature does not verify";
    const tc = new TextEncoder().encode(lines[2].slice("trusted comment: ".length));
    const both = new Uint8Array(64 + tc.length);
    both.set(signature);
    both.set(tc, 64);
    if (!(await ed25519Verify(pub, global, both))) return "trusted comment signature does not verify";
    return null;
  }
  return `signed by a key not in ${listName}`;
}

// ---------- BLAKE2b-512 (RFC 7693), for minisign's prehashed signatures ----------

const MASK = (1n << 64n) - 1n;
const IV = [
  0x6a09e667f3bcc908n, 0xbb67ae8584caa73bn, 0x3c6ef372fe94f82bn, 0xa54ff53a5f1d36f1n,
  0x510e527fade682d1n, 0x9b05688c2b3e6c1fn, 0x1f83d9abfb41bd6bn, 0x5be0cd19137e2179n,
];
const SIGMA = [
  [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
  [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
  [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
  [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
  [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
  [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
  [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
  [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
  [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
  [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
];

const rotr = (x: bigint, n: bigint) => ((x >> n) | (x << (64n - n))) & MASK;

function compress(h: bigint[], block: Uint8Array, t: bigint, last: boolean) {
  const dv = new DataView(block.buffer, block.byteOffset, 128);
  const m: bigint[] = [];
  for (let i = 0; i < 16; i++) m.push(dv.getBigUint64(i * 8, true));
  const v = [...h, ...IV];
  v[12] ^= t & MASK;
  v[13] ^= t >> 64n;
  if (last) v[14] ^= MASK;
  const g = (a: number, b: number, c: number, d: number, x: bigint, y: bigint) => {
    v[a] = (v[a] + v[b] + x) & MASK;
    v[d] = rotr(v[d] ^ v[a], 32n);
    v[c] = (v[c] + v[d]) & MASK;
    v[b] = rotr(v[b] ^ v[c], 24n);
    v[a] = (v[a] + v[b] + y) & MASK;
    v[d] = rotr(v[d] ^ v[a], 16n);
    v[c] = (v[c] + v[d]) & MASK;
    v[b] = rotr(v[b] ^ v[c], 63n);
  };
  for (let r = 0; r < 12; r++) {
    const s = SIGMA[r % 10];
    g(0, 4, 8, 12, m[s[0]], m[s[1]]);
    g(1, 5, 9, 13, m[s[2]], m[s[3]]);
    g(2, 6, 10, 14, m[s[4]], m[s[5]]);
    g(3, 7, 11, 15, m[s[6]], m[s[7]]);
    g(0, 5, 10, 15, m[s[8]], m[s[9]]);
    g(1, 6, 11, 12, m[s[10]], m[s[11]]);
    g(2, 7, 8, 13, m[s[12]], m[s[13]]);
    g(3, 4, 9, 14, m[s[14]], m[s[15]]);
  }
  for (let i = 0; i < 8; i++) h[i] ^= v[i] ^ v[i + 8];
}

export function blake2b512(data: Uint8Array): Uint8Array {
  const h = IV.slice();
  h[0] ^= 0x01010000n ^ 64n; // digest length 64, no key, fanout 1, depth 1
  const blocks = Math.max(1, Math.ceil(data.length / 128));
  let t = 0n;
  for (let i = 0; i < blocks; i++) {
    const block = new Uint8Array(128);
    const chunk = data.subarray(i * 128, i * 128 + 128);
    block.set(chunk);
    t += BigInt(chunk.length);
    compress(h, block, t, i === blocks - 1);
  }
  const out = new Uint8Array(64);
  const dv = new DataView(out.buffer);
  for (let i = 0; i < 8; i++) dv.setBigUint64(i * 8, h[i], true);
  return out;
}
