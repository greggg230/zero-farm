// Test helpers: an HTTP client for the Worker with a movable clock, and fixtures (builds, runs,
// checkpoints, campaigns, enrolled machines).

import { createExecutionContext, waitOnExecutionContext } from "cloudflare:test";
import { env } from "cloudflare:workers";
import { expect } from "vitest";
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
  const res = await worker.fetch(req as Request<unknown, IncomingRequestCfProperties>, env, ctx);
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
  const text = JSON.stringify(manifest, null, 2);
  const id = (await sha256(text)).slice(0, 24);
  await putArtifact(`builds/${id}/manifest.json`, text);
  await putArtifact(`builds/${id}/manifest.json.minisig`, `untrusted comment: test\nAAAA\ntrusted comment: build_id=${id} kind=${kind}\nAAAA\n`);
  await putArtifact(`builds/${id}/${entry}`, file);
  if (o.register !== false) ok(await admin("POST", "/v1/admin/build", { build_id: id }));
  return id;
}

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
}

export function runBody(build: string, o: RunOpts = {}) {
  return {
    game: "mtg",
    build_id: build,
    runtime: null,
    priority: o.priority ?? 1,
    state: o.state ?? "active",
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

export async function makeRun(run: string, o: RunOpts = {}): Promise<string> {
  const build = o.build ?? (await makeBuild({ tag: run }));
  ok(await admin("PUT", `/v1/admin/run/${run}`, runBody(build, o)));
  if (o.gen !== null) await publishCkpt(run, o.gen ?? 1);
  return build;
}

export async function makeCampaign(id: string, o: { build?: string; seed_base?: number; chunk?: number | "auto"; priority?: number; state?: string } = {}) {
  const build = o.build ?? (await makeBuild({ tag: `fuzz-${id}` }));
  return ok(
    await admin("PUT", `/v1/admin/campaign/${id}`, {
      build_id: build,
      mode: "delver",
      env: { FUZZ_POOL: "delver" },
      seed_base: o.seed_base ?? 10_000_000,
      chunk: o.chunk ?? 1000,
      priority: o.priority ?? 0.2,
      state: o.state ?? "active",
    }),
  );
}

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
  caps?: Partial<{ threads: number; ram_mb: number; gpu: boolean; disk_mb: number }>;
  cached?: { builds?: string[]; ckpts?: string[] };
  agent?: string;
  digest?: string;
  profile?: object | null;
  now?: number;
}

export function ask(m: Machine, o: AskOpts = {}): Promise<Res> {
  return call("POST", "/v1/lease", {
    token: m.token,
    agent: o.agent,
    now: o.now,
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
  o: { given_back?: boolean; unfinished?: unknown; stats?: unknown; now?: number } = {},
) {
  return call("POST", `/v1/lease/${leaseId}/done`, {
    token: m.token,
    now: o.now,
    body: { results, stats: o.stats ?? { games: results.length }, given_back: o.given_back ?? false, unfinished: o.unfinished ?? null },
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
