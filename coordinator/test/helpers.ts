// Test helpers: an HTTP client for the Worker with a movable clock, and fixtures (builds, runs,
// checkpoints, campaigns, enrolled machines).

import { createExecutionContext, waitOnExecutionContext } from "cloudflare:test";
import { env } from "cloudflare:workers";
import { expect } from "vitest";
import { blake2b512, verifyMinisign } from "../src/manifest";
import worker from "../src/index";

export const T0 = 1_800_000_000;
export const clock = { now: T0 };
export const advance = (s: number) => (clock.now += s);

export const ADMIN = () => env.ADMIN_TOKEN;
export const AGENT = "0.3.1";

export interface Res {
  status: number;
  body: any;
  headers: Headers;
}

export interface CallOpts {
  token?: string | null;
  body?: unknown; // JSON unless `raw`
  raw?: BodyInit;
  headers?: Record<string, string>;
  now?: number;
  agent?: string | null;
  env?: Record<string, string>; // vars for this request only (the Worker's; LeaseBook keeps its own)
}

export async function call(method: string, path: string, o: CallOpts = {}): Promise<Res> {
  const headers: Record<string, string> = { "x-test-now": String(o.now ?? clock.now), "x-farm-aw": "1" };
  if (o.agent !== null) headers["x-farm-agent"] = o.agent ?? AGENT;
  if (o.token) headers.authorization = `Bearer ${o.token}`;
  let body: BodyInit | undefined;
  if (method === "GET" || method === "HEAD") body = undefined;
  else if (o.raw !== undefined) body = o.raw;
  else if (o.body !== undefined) {
    body = JSON.stringify(o.body);
    headers["content-type"] = "application/json";
  }
  // The Worker's fetch handler with the real (local) bindings. Not SELF.fetch: in
  // vitest-pool-workers 0.23 every SELF request wraps the entrypoint's prototype in one more Proxy,
  // so thousands of requests (the fleet simulation) get quadratically slow and overflow the stack.
  const ctx = createExecutionContext();
  const req = new Request(`https://farm.test${path}`, { method, headers: { ...headers, ...o.headers }, body });
  const e = o.env ? ({ ...env, ...o.env } as typeof env) : env;
  const res = await worker.fetch(req as Request<unknown, IncomingRequestCfProperties>, e, ctx);
  await waitOnExecutionContext(ctx);
  const text = await res.text();
  let parsed: any = text;
  if ((res.headers.get("content-type") ?? "").includes("application/json")) parsed = JSON.parse(text);
  return { status: res.status, body: parsed, headers: res.headers };
}

export const admin = (method: string, path: string, body?: unknown, o: CallOpts = {}) =>
  call(method, path, { token: ADMIN(), body, ...o });

// Asserts the status (with the body in the failure message) and returns the body.
export function ok(r: Res, status = 200): any {
  expect(r.status, JSON.stringify(r.body)).toBe(status);
  return r.body;
}

export function err(r: Res, status: number, code: string): any {
  expect(r.status, JSON.stringify(r.body)).toBe(status);
  expect(r.body.error, JSON.stringify(r.body)).toBe(code);
  expect(typeof r.body.message).toBe("string");
  return r.body;
}

export async function sha256(data: string | Uint8Array): Promise<string> {
  const bytes = typeof data === "string" ? new TextEncoder().encode(data) : data;
  const d = new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
  return [...d].map((b) => b.toString(16).padStart(2, "0")).join("");
}

export async function putArtifact(key: string, data: string | Uint8Array): Promise<void> {
  const bytes = typeof data === "string" ? new TextEncoder().encode(data) : data;
  ok(await admin("PUT", `/v1/admin/artifact/${key}`, undefined, { raw: bytes, headers: { "x-sha256": await sha256(bytes) } }));
}

export interface BuildOpts {
  kind?: "worker" | "runtime" | "agent";
  version?: string;
  min_agent?: string;
  aw?: number;
  register?: boolean;
  tag?: string; // makes the manifest (and so the build id) unique
  requires_runtime?: string; // worker: the runtime pack it loads
  gpu?: { vendor: string; min_driver?: string; providers: string[] }; // runtime, rev 11 (PROTOCOL 9.2)
  size?: number; // the file's declared size (the manifest's; default: the real one)
}

// Uploads a manifest, its signature file and one file, and registers the build. Returns its id.
export async function makeBuild(o: BuildOpts = {}): Promise<string> {
  const kind = o.kind ?? "worker";
  const file = `${kind} binary ${o.tag ?? ""} ${o.version ?? ""}`;
  const entry = kind === "agent" ? "farm-agent.exe" : kind === "worker" ? "mtgz-worker.exe" : "onnxruntime.dll";
  const manifest: Record<string, unknown> = {
    manifest: 1,
    kind,
    version: o.version ?? (kind === "agent" ? "0.3.1" : `test-${o.tag ?? "x"}`),
    created: T0,
    files: [{ path: entry, sha256: await sha256(file), size: new TextEncoder().encode(file).length }],
  };
  if (kind !== "runtime") manifest.entry = entry;
  if (kind === "worker") {
    manifest.game = "mtg";
    manifest.aw = o.aw ?? 1;
    manifest.profile = "release";
    manifest.zero_onnx = [1];
  }
  if (o.min_agent) manifest.min_agent = o.min_agent;
  if (o.requires_runtime) manifest.requires_runtime = o.requires_runtime;
  if (o.gpu) manifest.gpu = o.gpu;
  const text = JSON.stringify(manifest, null, 2);
  const id = (await sha256(text)).slice(0, 24);
  await putArtifact(`builds/${id}/manifest.json`, text);
  await putArtifact(`builds/${id}/manifest.json.minisig`, `untrusted comment: test\nAAAA\ntrusted comment: build_id=${id} kind=${kind}\nAAAA\n`);
  await putArtifact(`builds/${id}/${entry}`, file);
  if (o.register !== false) ok(await admin("POST", "/v1/admin/build", { build_id: id }));
  return id;
}

// ---------- signed definitions (PROTOCOL 7) ----------

// The tests' data key (vitest.config.ts DATA_PUBKEYS holds its public half). Throwaway: nothing
// but these tests trusts it.
export const TEST_DATA_KEY = {
  d: "q0sXRrNbdZv0cqU59Vimx2BRSmnYa-Keh2rtBYCeDGo",
  x: "JMGqKGY1wUYWnXiv8nMvStgeJu94IkHcZbfDqhphpCw",
  keyId: "f786c8054e153423",
};

const b64 = (b: Uint8Array) => btoa(String.fromCharCode(...b));
const hexBytes = (h: string) => new Uint8Array(h.match(/../g)!.map((x) => parseInt(x, 16)));

export interface SignKey {
  d: string;
  x: string;
  keyId: string;
}

// A minisign signature file (prehashed, "ED") of `msg`.
export async function minisign(msg: Uint8Array, key: SignKey = TEST_DATA_KEY, trusted = "file=definition.json"): Promise<string> {
  const k = await crypto.subtle.importKey("jwk", { kty: "OKP", crv: "Ed25519", d: key.d, x: key.x }, { name: "Ed25519" }, false, ["sign"]);
  const sig = new Uint8Array(await crypto.subtle.sign("Ed25519", k, blake2b512(msg)));
  const head = new Uint8Array([0x45, 0x44, ...hexBytes(key.keyId), ...sig]);
  const tc = new TextEncoder().encode(trusted);
  const global = new Uint8Array(await crypto.subtle.sign("Ed25519", k, new Uint8Array([...sig, ...tc])));
  return ["untrusted comment: zero-farm test signature", b64(head), `trusted comment: ${trusted}`, b64(global), ""].join("\n");
}

// A fresh Ed25519 key (not trusted anywhere) and its minisign public key line.
export async function newSignKey(): Promise<{ key: SignKey; pub: string }> {
  const kp = (await crypto.subtle.generateKey({ name: "Ed25519" }, true, ["sign", "verify"])) as CryptoKeyPair;
  const jwk = (await crypto.subtle.exportKey("jwk", kp.privateKey)) as JsonWebKey;
  const id = crypto.getRandomValues(new Uint8Array(8));
  const x = Uint8Array.from(atob(jwk.x!.replace(/-/g, "+").replace(/_/g, "/")), (ch) => ch.charCodeAt(0));
  const keyId = [...id].map((b) => b.toString(16).padStart(2, "0")).join("");
  return { key: { d: jwk.d!, x: jwk.x!, keyId }, pub: b64(new Uint8Array([0x45, 0x64, ...id, ...x])) };
}

export interface DefOpts {
  key?: SignKey; // default: the test data key
  text?: string; // the exact bytes to upload (default: the doc's JSON)
  register?: boolean; // default true
  env?: Record<string, string>;
}

// Uploads a definition and its signature, and registers it. Returns the PUT answer.
export async function putDefinition(doc: any, o: DefOpts = {}): Promise<Res> {
  const text = o.text ?? JSON.stringify(doc, null, 2);
  const bytes = new TextEncoder().encode(text);
  const key = `defs/${doc.kind}/${doc.id}/${doc.version}/definition.json`;
  await putArtifact(key, bytes);
  // The trusted comment farm-release define signs with (informational for agents).
  await putArtifact(`${key}.minisig`, await minisign(bytes, o.key, `definition=1 kind=${doc.kind} id=${doc.id} version=${doc.version}`));
  if (o.register === false) return { status: 0, body: null, headers: new Headers() };
  return admin("PUT", "/v1/admin/definition", { kind: doc.kind, id: doc.id, version: doc.version, key, sha256: await sha256(bytes) }, { env: o.env });
}

// A definition naming exactly what the run or campaign uses now: its build and runtime, every
// registered checkpoint, and its job templates (the coordinator's job objects). Version = next.
export async function definitionFor(kind: "run" | "campaign", id: string): Promise<any | null> {
  const prev = await env.DB.prepare(`SELECT MAX(version) AS v FROM definitions WHERE kind = ? AND id = ?`).bind(kind, id).first<{ v: number | null }>();
  const base = { definition: 1, kind, id, version: (prev?.v ?? 0) + 1, created: clock.now, expires: null };
  if (kind === "campaign") {
    const c = await env.DB.prepare(`SELECT * FROM campaigns WHERE campaign = ?`).bind(id).first<any>();
    return { ...base, game: "mtg", builds: [c.build_id], runtime: null, checkpoints: {}, jobs: { selfplay: null, eval: null, fuzz: { mode: c.mode, env: JSON.parse(c.env) } } };
  }
  const r = await env.DB.prepare(`SELECT * FROM runs WHERE run = ?`).bind(id).first<any>();
  const ck = (await env.DB.prepare(`SELECT * FROM checkpoints WHERE run = ?`).bind(id).all<any>()).results;
  const sp = JSON.parse(r.selfplay ?? "null");
  const ev = JSON.parse(r.eval ?? "null");
  if (!sp && !ev) return null;
  return {
    ...base,
    game: r.game,
    builds: [r.build_id],
    runtime: r.runtime,
    checkpoints: Object.fromEntries(
      ck.map((c: any) => [String(c.gen), { model_sha256: c.sha256, model_size: c.size, reference_sha256: c.reference_sha256, reference_size: c.reference_size }]),
    ),
    // Templates as the agent takes them: only their kind's keys (no per-lease `seed`).
    jobs: {
      selfplay: sp ? { settings: sp.job.settings ?? {} } : null,
      eval: ev ? { settings: ev.job.settings ?? {}, seats: ev.job.seats, paired: ev.job.paired === true } : null,
      fuzz: null,
    },
  };
}

// Signs and registers the next definition of a run or campaign (definitionFor), as the desktop
// does whenever builds, checkpoints or job settings change.
export async function define(kind: "run" | "campaign", id: string, mutate?: (d: any) => void): Promise<any> {
  const doc = await definitionFor(kind, id);
  if (!doc) return null;
  mutate?.(doc);
  return ok(await putDefinition(doc));
}

export const hasDefinition = async (kind: string, id: string) =>
  !!(await env.DB.prepare(`SELECT 1 FROM definitions WHERE kind = ? AND id = ?`).bind(kind, id).first());

export async function publishCkpt(run: string, gen: number): Promise<void> {
  const model = `onnx ${run} ${gen}`;
  const ref = `reference ${run} ${gen}`;
  await putArtifact(`ckpt/${run}/${gen}/model.onnx`, model);
  await putArtifact(`ckpt/${run}/${gen}/reference.npz`, ref);
  ok(
    await admin("PUT", `/v1/admin/run/${run}/checkpoint/${gen}`, {
      model: `ckpt/${run}/${gen}/model.onnx`,
      sha256: await sha256(model),
      reference: `ckpt/${run}/${gen}/reference.npz`,
      reference_sha256: await sha256(ref),
    }),
  );
  // A run with a definition gets a new version that signs the new gen (the trainer does this).
  if (await hasDefinition("run", run)) await define("run", run);
}

export interface RunOpts {
  build?: string;
  quota?: number;
  slack?: number;
  max_stale?: number;
  games_per_lease?: number | "auto";
  priority?: number;
  gpu?: "any" | "preferred" | "required";
  needs?: { threads?: number; ram_mb?: number };
  eval?: Record<string, unknown> | null;
  selfplay?: boolean;
  state?: "active" | "paused";
  gen?: number | null; // checkpoint to publish (default 1; null = none)
  define?: boolean; // sign a definition for it (default true; false = legacy agents only)
  min_agent?: string | null; // rev 9 (absent: not sent)
}

export function runBody(build: string, o: RunOpts = {}) {
  return {
    game: "mtg",
    build_id: build,
    runtime: null,
    priority: o.priority ?? 1,
    state: o.state ?? "active",
    ...(o.min_agent !== undefined ? { min_agent: o.min_agent } : {}),
    selfplay:
      o.selfplay === false
        ? null
        : {
            quota: o.quota ?? 100,
            slack: o.slack ?? 0.1,
            max_stale: o.max_stale ?? 1,
            games_per_lease: o.games_per_lease ?? 10,
            gpu: o.gpu ?? "any",
            needs: o.needs,
            job: { settings: { sims: 64 } },
          },
    eval: o.eval ?? null,
  };
}

// Creates a run, then publishes its checkpoint; an eval batch is put last, once its checkpoints
// are registered (PROTOCOL 5.2: they must be, and the run must exist to register them).
export async function makeRun(run: string, o: RunOpts = {}): Promise<string> {
  const build = o.build ?? (await makeBuild({ tag: run }));
  ok(await admin("PUT", `/v1/admin/run/${run}`, runBody(build, { ...o, eval: null })));
  if (o.gen !== null) await publishCkpt(run, o.gen ?? 1);
  if (o.eval) ok(await admin("PUT", `/v1/admin/run/${run}`, runBody(build, o)));
  if (o.define !== false) await define("run", run);
  return build;
}

export async function makeCampaign(
  id: string,
  o: { build?: string; seed_base?: number; chunk?: number | "auto"; priority?: number; state?: string; define?: boolean; min_agent?: string | null } = {},
) {
  const build = o.build ?? (await makeBuild({ tag: `fuzz-${id}` }));
  const ans = ok(
    await admin("PUT", `/v1/admin/campaign/${id}`, {
      build_id: build,
      mode: "delver",
      env: { FUZZ_POOL: "delver" },
      seed_base: o.seed_base ?? 10_000_000,
      chunk: o.chunk ?? 1000,
      priority: o.priority ?? 0.2,
      state: o.state ?? "active",
      ...(o.min_agent !== undefined ? { min_agent: o.min_agent } : {}),
    }),
  );
  if (o.define !== false) await define("campaign", id);
  return ans;
}

// Agent versions around DEFINITION_AGENT (0.2.5): AGENT verifies definitions, LEGACY doesn't.
export const LEGACY = "0.2.4";

export const PROFILE = {
  os: "Windows 11 26100",
  cpu: "AMD Ryzen 7 7800X3D",
  cores: 8,
  threads: 16,
  perf_cores: 8,
  eff_cores: 0,
  ram_mb: 32768,
  disk_free_mb: 412000,
  gpus: [{ vendor: "nvidia", name: "RTX 4070", vram_mb: 12282, driver: "610.88" }],
  providers: ["cpu", "cuda"],
};
export const DIGEST = "ab".repeat(32);

export interface Machine {
  machine_id: string;
  token: string;
  name: string;
}

export async function enroll(name = "pc", o: { channel?: string; profile?: object; digest?: string | null } = {}): Promise<Machine> {
  const { codes } = ok(await admin("POST", "/v1/admin/invites", { count: 1, note: name, channel: o.channel }));
  const b = ok(
    await call("POST", "/v1/enroll", {
      body: { invite: codes[0], name, profile: o.profile ?? PROFILE, profile_digest: o.digest === null ? undefined : (o.digest ?? DIGEST) },
    }),
  );
  return { machine_id: b.machine_id, token: b.token, name };
}

export interface AskOpts {
  kinds?: string[];
  caps?: Partial<{ threads: number; ram_mb: number; gpu: boolean; disk_mb: number; gpu_allowed: boolean; gpu_games: Record<string, number> }>;
  cached?: { builds?: string[]; ckpts?: string[] };
  agent?: string;
  digest?: string;
  profile?: object | null;
  now?: number;
  env?: Record<string, string>;
}

export function ask(m: Machine, o: AskOpts = {}): Promise<Res> {
  return call("POST", "/v1/lease", {
    token: m.token,
    agent: o.agent,
    now: o.now,
    env: o.env,
    body: {
      profile_digest: o.digest ?? DIGEST,
      profile: o.profile ?? null,
      caps: { threads: 6, ram_mb: 12000, gpu: false, disk_mb: 2500, ...o.caps },
      cached: o.cached ?? { builds: [], ckpts: [] },
      kinds: o.kinds ?? ["fuzz", "eval", "selfplay", "diag"],
    },
  });
}

// A lease, asserting one was granted.
export async function lease(m: Machine, o: AskOpts = {}): Promise<any> {
  const b = ok(await ask(m, o));
  expect(b.lease, JSON.stringify(b)).toBeTruthy();
  return b.lease;
}

export function beat(m: Machine, leaseId: string, state: "running" | "paused" = "running", extra: Record<string, unknown> = {}, now?: number) {
  return call("POST", `/v1/lease/${leaseId}/beat`, { token: m.token, now, body: { state, progress: { games_done: 0, games_total: 1 }, ...extra } });
}

export async function upload(
  m: Machine,
  leaseId: string,
  seq: number,
  content: string,
  o: { ids?: [number, number][] | null; gen?: number; sha?: string; now?: number } = {},
): Promise<Res> {
  const headers: Record<string, string> = { "x-sha256": o.sha ?? (await sha256(content)) };
  if (o.ids) headers["x-ids"] = JSON.stringify(o.ids);
  if (o.gen !== undefined) headers["x-gen"] = String(o.gen);
  return call("PUT", `/v1/lease/${leaseId}/result/${seq}`, { token: m.token, raw: content, headers, now: o.now });
}

export function finish(
  m: Machine,
  leaseId: string,
  results: number[],
  o: { given_back?: boolean; unfinished?: unknown; stats?: unknown; now?: number; refused?: string } = {},
) {
  return call("POST", `/v1/lease/${leaseId}/done`, {
    token: m.token,
    now: o.now,
    body: {
      results,
      stats: o.stats ?? { games: results.length },
      given_back: o.given_back ?? false,
      unfinished: o.unfinished ?? null,
      ...(o.refused ? { refused: o.refused } : {}),
    },
  });
}

// Plays and uploads every id of a lease in one result, then finishes it.
export async function complete(m: Machine, l: any, now?: number): Promise<void> {
  const [lo, hi] = idsOf(l);
  ok(await upload(m, l.lease_id, 0, JSON.stringify({ lease: l.lease_id, lo, hi }), { ids: [[lo, hi]], now }));
  ok(await finish(m, l.lease_id, [0], { now }));
}

export function idsOf(l: any): [number, number] {
  const j = l.job;
  return j.kind === "fuzz" ? [j.first, j.first + j.count] : [j.gid_base, j.gid_base + j.games];
}

// Every id of a scope, from D1: claimed ranges, pooled ranges, and active leases' unclaimed ids.
export async function accounting(scope: string) {
  const claims = (await env.DB.prepare(`SELECT lo, hi, lease_id FROM claims WHERE scope = ? ORDER BY lo`).bind(scope).all<any>()).results;
  const pool = (await env.DB.prepare(`SELECT lo, hi FROM pool WHERE scope = ? ORDER BY lo`).bind(scope).all<any>()).results;
  const active = (await env.DB.prepare(`SELECT lo, hi FROM leases WHERE scope = ? AND state = 'active' AND pool IS NOT NULL`).bind(scope).all<any>())
    .results;
  return { claims, pool, active };
}

// Asserts claims and pool tile [lo, hi) exactly (no gap, no overlap) and no lease is active.
export async function expectTiled(scope: string, lo: number, hi: number) {
  const a = await accounting(scope);
  expect(a.active, "no active leases").toEqual([]);
  const all = [...a.claims.map((c: any) => [c.lo, c.hi, "claim"]), ...a.pool.map((p: any) => [p.lo, p.hi, "pool"])].sort((x, y) => x[0] - y[0]);
  let at = lo;
  for (const [a0, b0, what] of all) {
    expect(a0, `${what} [${a0}, ${b0}) should start at ${at} (gap or overlap)`).toBe(at);
    at = b0;
  }
  expect(at).toBe(hi);
  return a;
}

// What a definition-aware agent checks before it runs a lease (PROTOCOL 7, steps 1-4), as far as
// the coordinator's side goes: the definition downloads with its sha256, verifies against the test
// data key, names this run/campaign, lists the lease's build and runtime, signs every checkpoint,
// and the job carries only per-lease fields. Returns the definition.
const verified = new Map<string, any>();
export async function checkDefinitionLease(m: Machine, l: any): Promise<any> {
  expect(l.definition, "a definition lease").toBeTruthy();
  const id = l.definition.key + "@" + l.definition.sha256;
  let d = verified.get(id);
  if (!d) {
    const res = await call("GET", `/v1/artifact/${l.definition.key}`, { token: m.token });
    expect(res.status).toBe(200);
    const bytes = new TextEncoder().encode(res.body as string);
    expect(await sha256(bytes)).toBe(l.definition.sha256);
    const sig = await call("GET", `/v1/artifact/${l.definition.key}.minisig`, { token: m.token });
    expect(await verifyMinisign(bytes, sig.body as string, [String((env as any).DATA_PUBKEYS)], "DATA_PUBKEYS")).toBeNull();
    d = JSON.parse(res.body as string);
    verified.set(id, d);
  }
  expect([d.kind, d.id]).toEqual(l.run ? ["run", l.run] : ["campaign", l.campaign]);
  expect(d.builds).toContain(l.build.build_id);
  expect(d.runtime ?? null).toBe(l.runtime?.build_id ?? null);
  for (const c of [l.checkpoint, ...(l.checkpoints ?? [])].filter(Boolean)) {
    const e = d.checkpoints[String(c.gen)];
    expect(e, `gen ${c.gen} signed`).toBeTruthy();
    expect(e.model_sha256).toBe(c.sha256);
    expect(e.reference_sha256 ?? null).toBe(c.reference_sha256 ?? null);
  }
  const allowed = l.kind === "fuzz" ? ["count", "first", "kind"] : ["games", "gid_base", "kind", "seed"];
  expect(Object.keys(l.job).sort()).toEqual(allowed);
  expect(l.job.kind).toBe(l.kind);
  expect(d.jobs[l.kind], `the definition has a ${l.kind} template`).toBeTruthy();
  return d;
}
