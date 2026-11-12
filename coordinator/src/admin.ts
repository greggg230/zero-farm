// Desktop API (PROTOCOL 4): bearer = the admin token. Used by the trainer's pool remote, the
// release tool, scripts/admin.ts and the /admin dashboard.

import { accessEnabled } from "./access";
import * as book from "./book";
import { blockLifted } from "./blocks";
import { config } from "./config";
import { authAdmin, type Ctx } from "./ctx";
import {
  DEF_KEY_RE,
  DEF_KINDS,
  type Def,
  type DefinitionRow,
  MAX_DEFINITION,
  MAX_SIGNATURE,
  checkpointProblem,
  currentDef,
  currentDefs,
  defKey,
  evalProblem,
  leaseProblem,
  parseDefinition,
  seatGens,
} from "./definitions";
import { MAX_EXTERNAL_BODY, type Contributor, type ExternalRow, ownContributor, parseReport, storeReport, totals } from "./external";
import { CHANNELS, serveObject } from "./friend";
import { bookFor } from "./leasebook";
import { minisignKeyId, verifyBuild, verifyMinisign } from "./manifest";
import { GPU_MODES, type Routing, routingOf } from "./routing";
import {
  BUILD_RE,
  HEX64_RE,
  MACHINE_RE,
  SLUG_RE,
  bad,
  bool,
  checkKey,
  cmpSemver,
  conflict,
  hex,
  int,
  type Json,
  isSemver,
  json,
  newInviteCode,
  notFound,
  num,
  obj,
  oneOf,
  opaque,
  parseJson,
  readBounded,
  readJson,
  sha256Hex,
  str,
  unwrap,
  uriDecode,
} from "./util";

const STATES = ["active", "paused"] as const;
const MAX_SIMPLE_PUT = 100 * 1024 * 1024;
const NAME_RE = /^[a-z0-9][a-z0-9_-]{0,63}$/;

function slug(v: string, what: string): string {
  if (!SLUG_RE.test(v)) throw bad(`${what} must match [a-z0-9][a-z0-9-]{0,62}`);
  return v;
}

// ---------- invites ----------

export async function invites(c: Ctx): Promise<Response> {
  await authAdmin(c);
  const b = await readJson(c.req);
  const count = int(b.count, "count", { optional: true, min: 1, max: 100 }) ?? 1;
  const note = str(b.note, "note", { optional: true, max: 200 }) ?? null;
  const channel = oneOf(b.channel, "channel", CHANNELS, { optional: true }) ?? "stable";
  const codes: string[] = [];
  while (codes.length < count) {
    const code = newInviteCode();
    const r = await c.env.DB.prepare(`INSERT OR IGNORE INTO invites (code, note, channel, created_at) VALUES (?, ?, ?, ?)`)
      .bind(code, note, channel, c.now)
      .run();
    if (r.meta.changes) codes.push(code);
  }
  return json({ codes });
}

// ---------- artifacts and builds ----------

// Upload keys: builds/, ckpt/, and (rev 6) a definition and its signature at
// defs/<kind>/<id>/<version>/definition.json(.minisig). `registered`: that definition version is
// registered, so its files can't change (agents check their sha256, and the version is spent).
async function uploadKey(c: Ctx, raw: string): Promise<{ key: string; registered: boolean }> {
  const key = checkKey(uriDecode(raw));
  if (key.startsWith("defs/")) {
    const m = DEF_KEY_RE.exec(key);
    if (!m) throw bad("definitions go at defs/<run|campaign>/<id>/<version>/definition.json (and .minisig)");
    const taken = await c.env.DB.prepare(`SELECT 1 FROM definitions WHERE kind = ? AND id = ? AND version = ?`).bind(m[1], m[2], Number(m[3])).first();
    return { key, registered: !!taken };
  }
  if (!["builds/", "ckpt/"].some((p) => key.startsWith(p))) throw bad("artifacts go under builds/, ckpt/ or defs/");
  return { key, registered: false };
}

const spent = (key: string) => conflict(`${key} belongs to a registered definition: its files can't change (sign a new version)`);

// An agent release's installer on GitHub Releases (rev 4), where GET /download redirects. Exactly
// https://github.com/<GITHUB_REPO>/releases/download/agent-v<version>/ZeroFarmSetup.exe, the
// release's own version (security review H4/L4: no other repository, tag or file).
export function githubUrlFor(repo: string, version: string): string {
  return `https://github.com/${repo}/releases/download/agent-v${version}/ZeroFarmSetup.exe`;
}

// PUT /v1/admin/artifact/{key}: a whole object, or (with ?upload_id=&part=) one part of a
// multipart upload for objects larger than a request may be.
export async function putArtifact(c: Ctx, raw: string): Promise<Response> {
  await authAdmin(c);
  const { key, registered } = await uploadKey(c, raw);
  if (!c.req.body) throw bad("empty body");
  if (registered) {
    // Uploading the very same bytes again (a retried publish) is a no-op; anything else is refused.
    if (c.url.searchParams.has("upload_id")) throw spent(key);
    const body = await readBounded(c.req, MAX_DEFINITION);
    const old = await c.env.BUCKET.get(key);
    if (!old || (await sha256Hex(new Uint8Array(await old.arrayBuffer()))) !== (await sha256Hex(body))) throw spent(key);
    return json({ ok: true, key, size: body.byteLength, unchanged: true });
  }
  const uploadId = c.url.searchParams.get("upload_id");
  if (uploadId !== null) {
    const part = int(Number(c.url.searchParams.get("part")), "part", { min: 1, max: 10000 });
    const mpu = c.env.BUCKET.resumeMultipartUpload(key, uploadId);
    const up = await mpu.uploadPart(part, c.req.body);
    return json({ part: up.partNumber, etag: up.etag });
  }
  const sha = c.req.headers.get("x-sha256")?.toLowerCase();
  if (sha !== undefined && !HEX64_RE.test(sha)) throw bad("x-sha256 must be lowercase hex sha256");
  // R2 streams a body whose length is known; a chunked one is read into memory first (bounded:
  // bigger artifacts go up as multipart).
  const body = c.req.headers.get("content-length") !== null ? c.req.body : await readBounded(c.req, MAX_SIMPLE_PUT);
  try {
    const put = await c.env.BUCKET.put(key, body, sha ? { sha256: sha } : undefined);
    return json({ ok: true, key, size: put?.size ?? null });
  } catch (e) {
    if (sha && /sha-?256|checksum|digest/i.test(String(e))) throw bad("body does not match x-sha256");
    throw e;
  }
}

// POST /v1/admin/artifact/{key}?uploads starts a multipart upload; ?upload_id= completes it
// with {"parts": [{"part": 1, "etag": "..."}, ...]}.
export async function postArtifact(c: Ctx, raw: string): Promise<Response> {
  await authAdmin(c);
  const { key, registered } = await uploadKey(c, raw);
  if (registered) throw spent(key);
  if (c.url.searchParams.has("uploads")) {
    const mpu = await c.env.BUCKET.createMultipartUpload(key);
    return json({ key, upload_id: mpu.uploadId });
  }
  const uploadId = c.url.searchParams.get("upload_id");
  if (uploadId === null) throw bad("POST needs ?uploads or ?upload_id=");
  const b = await readJson(c.req);
  if (!Array.isArray(b.parts) || !b.parts.length || b.parts.length > 10000) throw bad("parts must be a non-empty list");
  const parts = b.parts.map((p: unknown, i: number) => {
    const o = obj(p, `parts[${i}]`);
    return { partNumber: int(o.part, `parts[${i}].part`, { min: 1, max: 10000 }), etag: str(o.etag, `parts[${i}].etag`, { max: 200 }) };
  });
  const done = await c.env.BUCKET.resumeMultipartUpload(key, uploadId).complete(parts);
  return json({ ok: true, key, size: done.size });
}

// GET /v1/admin/artifact/{key}: any object (results, diag uploads, builds, checkpoints).
export async function getArtifact(c: Ctx, raw: string): Promise<Response> {
  await authAdmin(c);
  return serveObject(c, checkKey(uriDecode(raw)));
}

export async function registerBuild(c: Ctx): Promise<Response> {
  await authAdmin(c);
  const b = await readJson(c.req);
  const id = str(b.build_id, "build_id", { re: BUILD_RE });
  const v = await verifyBuild(c.env.BUCKET, id, String(c.env.RELEASE_PUBKEYS ?? ""));
  const m = v.manifest;
  await c.env.DB.prepare(
    `INSERT OR REPLACE INTO builds (build_id, kind, version, game, profile, min_agent, aw, size, manifest, sig_verified, created_at)
     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
  )
    .bind(id, m.kind, m.version, m.game ?? null, m.profile ?? null, m.min_agent ?? null, m.aw ?? null, v.size, v.text, v.sig_verified ? 1 : 0, c.now)
    .run();
  return json({ ok: true, build_id: id, kind: m.kind, version: m.version, size: v.size, sig_verified: v.sig_verified });
}

async function buildOfKind(db: D1Database, id: string, kind: string, what: string) {
  const b = await db.prepare(`SELECT build_id, kind, version FROM builds WHERE build_id = ?`).bind(id).first<{ kind: string; version: string }>();
  if (!b) throw bad(`${what} ${id} is not a registered build (POST /v1/admin/build first)`);
  if (b.kind !== kind) throw bad(`${what} ${id} is a ${b.kind} build, not ${kind}`);
  return b;
}

export async function agentRelease(c: Ctx): Promise<Response> {
  await authAdmin(c);
  const b = await readJson(c.req);
  const version = str(b.version, "version", { max: 64 });
  if (!isSemver(version)) throw bad("version must be semver");
  const id = str(b.build_id, "build_id", { re: BUILD_RE });
  const channel = oneOf(b.channel, "channel", CHANNELS);
  const minAgent = str(b.min_agent, "min_agent", { max: 64 });
  if (!isSemver(minAgent)) throw bad("min_agent must be semver");
  if (cmpSemver(minAgent, version) > 0) throw bad("min_agent is newer than the release itself");
  const build = await buildOfKind(c.env.DB, id, "agent", "build_id");
  if (build.version !== version) throw bad(`build ${id} is agent ${build.version}, not ${version}`);
  // Rev 4: installers live on GitHub Releases, no longer in R2 (`setup`). Refused loudly rather
  // than ignored, so an old release tool can't think it attached one.
  if (b.setup !== undefined && b.setup !== null) throw bad("setup is no longer accepted: pass github_url (PROTOCOL rev 4)");
  let githubUrl: string | null = null;
  if (b.github_url !== undefined && b.github_url !== null) {
    githubUrl = str(b.github_url, "github_url", { max: 512 });
    const want = githubUrlFor(config(c.env).githubRepo, version);
    if (githubUrl !== want) throw bad(`github_url must be ${want}`);
  }
  await c.env.DB.prepare(`INSERT INTO agent_releases (channel, version, build_id, min_agent, t, github_url) VALUES (?, ?, ?, ?, ?, ?)`)
    .bind(channel, version, id, minAgent, c.now, githubUrl)
    .run();
  return json({ ok: true, channel, version, build_id: id, min_agent: minAgent, github_url: githubUrl });
}

// ---------- runs ----------

function needsOf(v: unknown, name: string): book.Needs {
  const o = obj(v, name, { optional: true }) ?? {};
  return {
    threads: int(o.threads, `${name}.threads`, { optional: true, min: 0, max: 4096 }) ?? 0,
    ram_mb: int(o.ram_mb, `${name}.ram_mb`, { optional: true, min: 0 }) ?? 0,
  };
}

// Rev 7 capability routing (PROTOCOL 8): `gpu` (default any) and an optional `providers` allowlist.
function routingIn(o: Record<string, any>, prefix: string): Routing {
  return routingOf(oneOf(o.gpu, `${prefix}gpu`, GPU_MODES, { optional: true }) ?? "any", o.providers, prefix);
}

function perLease(v: unknown, name: string, max: number): "auto" | number {
  if (v === undefined || v === null || v === "auto") return "auto";
  return int(v, name, { min: 1, max });
}

function jobOf(v: unknown, name: string): Record<string, Json> {
  return opaque(obj(v, name, { optional: true }) ?? {}, name) as Record<string, Json>;
}

function selfplayOf(v: unknown): book.SelfplayCfg | null {
  if (v === undefined || v === null) return null;
  const o = obj(v, "selfplay");
  return {
    quota: int(o.quota, "selfplay.quota", { min: 0, max: 10_000_000 }),
    slack: num(o.slack, "selfplay.slack", { optional: true, min: 0, max: 10 }) ?? 0.1,
    max_stale: int(o.max_stale, "selfplay.max_stale", { optional: true, min: 0, max: 1000 }) ?? 1,
    games_per_lease: perLease(o.games_per_lease, "selfplay.games_per_lease", 100000),
    ...routingIn(o, "selfplay."),
    needs: needsOf(o.needs, "selfplay.needs"),
    job: jobOf(o.job, "selfplay.job"),
  };
}

export const MAX_EVAL_NETS = 16;

// An eval batch (PROTOCOL 5.2, rev 5). Checks what needs no database here; LeaseBook checks that
// the checkpoints are registered and that a batch never changes.
export function evalOf(v: unknown): book.EvalCfg | null {
  if (v === undefined || v === null) return null;
  const o = obj(v, "eval");
  if (o.gen !== undefined) throw bad("eval.gen was replaced by eval.checkpoints (PROTOCOL rev 5)");
  const games = int(o.games, "eval.games", { min: 1, max: 1_000_000 });
  if (!Array.isArray(o.checkpoints)) throw bad("eval.checkpoints must be a list of checkpoint gens (PROTOCOL 5.2)");
  if (o.checkpoints.length > MAX_EVAL_NETS) throw bad(`eval.checkpoints has more than ${MAX_EVAL_NETS} gens`);
  const checkpoints = o.checkpoints.map((g: unknown, i: number) => int(g, `eval.checkpoints[${i}]`, { min: 0, max: 999_999_999 }));
  if (new Set(checkpoints).size !== checkpoints.length) throw bad("eval.checkpoints has a gen twice");
  const job = jobOf(o.job, "eval.job");
  const paired = bool(job.paired, "eval.job.paired", { optional: true }) ?? false;
  if (paired && games % 2 !== 0) throw bad("eval.games must be even for a paired batch (whole pairs)");
  const seatProblems: string[] = [];
  const nets = new Set<number>(seatGens(job.seats, "eval.job.seats", seatProblems));
  if (seatProblems.length) throw bad(seatProblems[0], { problems: seatProblems });
  const named = [...nets].sort((a, b) => a - b).join(",");
  if (named !== [...checkpoints].sort((a, b) => a - b).join(",")) {
    throw bad(`eval.checkpoints [${checkpoints.join(", ")}] must be exactly the gens the seats name [${named}]`);
  }
  return {
    batch: str(o.batch, "eval.batch", { re: SLUG_RE }),
    games,
    checkpoints,
    games_per_lease: perLease(o.games_per_lease, "eval.games_per_lease", 100000),
    ...routingIn(o, "eval."),
    needs: needsOf(o.needs, "eval.needs"),
    job,
  };
}

export async function putRun(c: Ctx, run: string): Promise<Response> {
  await authAdmin(c);
  slug(run, "run");
  const b = await readJson(c.req);
  const cfg: book.RunCfg = {
    run,
    game: str(b.game, "game", { re: NAME_RE }),
    build_id: str(b.build_id, "build_id", { re: BUILD_RE }),
    runtime: str(b.runtime, "runtime", { optional: true, re: BUILD_RE }) ?? null,
    priority: num(b.priority, "priority", { optional: true, min: 0, max: 1000 }) ?? 1,
    state: oneOf(b.state, "state", STATES, { optional: true }) ?? "active",
    selfplay: selfplayOf(b.selfplay),
    eval: evalOf(b.eval),
  };
  await buildOfKind(c.env.DB, cfg.build_id, "worker", "build_id");
  if (cfg.runtime) await buildOfKind(c.env.DB, cfg.runtime, "runtime", "runtime");
  return json(unwrap(await bookFor(c.env, book.runScope(run)).putRun(cfg, c.now)));
}

export async function getRunRoute(c: Ctx, run: string): Promise<Response> {
  await authAdmin(c);
  const r = await book.getRun(c.env.DB, slug(run, "run"));
  if (!r) throw notFound(`no run ${run}`);
  const scope = book.runScope(run);
  const sp = unwrap(await bookFor(c.env, scope).selfplayState(run));
  const ckpt = r.current_gen === null ? null : await c.env.DB.prepare(`SELECT * FROM checkpoints WHERE run = ? AND gen = ?`).bind(run, r.current_gen).first();
  const gens = (await c.env.DB.prepare(`SELECT gen FROM checkpoints WHERE run = ? ORDER BY gen`).bind(run).all<{ gen: number }>()).results;
  const batches = await evalBatches(c.env.DB, run, 50);
  return json({
    run: r.run,
    game: r.game,
    build_id: r.build_id,
    runtime: r.runtime,
    priority: r.priority,
    state: r.state,
    selfplay: parseJson(r.selfplay, null),
    eval: parseJson(r.eval, null),
    eval_batch: r.eval_batch,
    next_gid: r.next_gid,
    current_gen: r.current_gen,
    checkpoint: ckpt,
    checkpoints: gens.map((g) => g.gen),
    selfplay_state: sp,
    eval_state: batches.find((b) => b.batch === r.eval_batch && b.replaced_at === null) ?? null,
    eval_batches: batches,
    pool: await book.poolSize(c.env.DB, scope),
    definition: await definitionStatus(c, "run", r.run, r, null),
  });
}

// A run's eval batches with their progress, newest first.
async function evalBatches(db: D1Database, run: string, limit: number): Promise<book.EvalProgress[]> {
  const { results } = await db
    .prepare(`SELECT * FROM eval_batches WHERE run = ? ORDER BY created_at DESC, lo DESC LIMIT ?`)
    .bind(run, limit)
    .all<book.EvalBatchRow>();
  const out: book.EvalProgress[] = [];
  for (const b of results) out.push(await book.evalProgress(db, b));
  return out;
}

export async function putCheckpoint(c: Ctx, run: string, genStr: string): Promise<Response> {
  await authAdmin(c);
  slug(run, "run");
  if (!/^\d{1,9}$/.test(genStr)) throw bad("gen must be an integer");
  const b = await readJson(c.req);
  const model = checkKey(str(b.model, "model", { max: 512 }));
  const sha256 = str(b.sha256, "sha256", { re: HEX64_RE });
  const reference = b.reference === undefined || b.reference === null ? null : checkKey(str(b.reference, "reference", { max: 512 }));
  const referenceSha = reference ? str(b.reference_sha256, "reference_sha256", { re: HEX64_RE }) : null;
  for (const k of [model, reference]) {
    if (k && !k.startsWith(`ckpt/${run}/`)) throw bad(`${k} is not under ckpt/${run}/`);
  }
  const head = await c.env.BUCKET.head(model);
  if (!head) throw bad(`${model} is not uploaded`);
  if (head.checksums.sha256 && hex(head.checksums.sha256) !== sha256) throw bad(`${model} does not match sha256`);
  let referenceSize: number | null = null;
  if (reference) {
    const rh = await c.env.BUCKET.head(reference);
    if (!rh) throw bad(`${reference} is not uploaded`);
    if (rh.checksums.sha256 && hex(rh.checksums.sha256) !== referenceSha) throw bad(`${reference} does not match reference_sha256`);
    referenceSize = rh.size;
  }
  // Rev 6: a gen the current definition already signs must be exactly those files.
  const def = await currentDef(c.env.DB, "run", run);
  const facts = { gen: Number(genStr), sha256, size: head.size, reference_sha256: referenceSha, reference_size: referenceSize };
  if (def && def.checkpoints[String(facts.gen)] && checkpointProblem(def, facts)) throw bad(checkpointProblem(def, facts)!);
  const ans = unwrap(
    await bookFor(c.env, book.runScope(run)).publishCheckpoint({
      run,
      gen: Number(genStr),
      model,
      sha256,
      reference,
      reference_sha256: referenceSha,
      size: head.size,
      reference_size: referenceSize,
      now: c.now,
    }),
  );
  return json(ans);
}

// ---------- results (runs and campaigns) ----------

async function listResults(c: Ctx, scope: string): Promise<Response> {
  const after = int(Number(c.url.searchParams.get("after") ?? "0"), "after", { min: 0 });
  const limit = int(Number(c.url.searchParams.get("limit") ?? "500"), "limit", { min: 1, max: 1000 });
  const { results } = await c.env.DB.prepare(`SELECT * FROM results WHERE scope = ? AND id > ? ORDER BY id LIMIT ?`)
    .bind(scope, after, limit)
    .all<any>();
  return json({
    results: results.map((r) => ({
      id: r.id,
      key: r.key,
      lease_id: r.lease_id,
      seq: r.seq,
      machine_id: r.machine_id,
      kind: r.kind,
      gen: r.gen,
      games: r.games,
      ids: JSON.parse(r.ids),
      dup_ids: JSON.parse(r.dup_ids),
      sha256: r.sha256,
      size: r.size,
      t: r.t,
      // Eval chunks (PROTOCOL 5.2): `held` ids wait for their pair-mate; `also` = held ids of
      // earlier chunks of this lease that this chunk completed (their rows are in those chunks).
      ...(r.kind === "eval" ? { batch: r.batch, held: parseJson(r.held, []), also: parseJson(r.also, []) } : {}),
    })),
    cursor: results.length ? results[results.length - 1].id : after,
  });
}

async function requireRun(c: Ctx, run: string) {
  if (!(await book.getRun(c.env.DB, slug(run, "run")))) throw notFound(`no run ${run}`);
}
async function requireCampaign(c: Ctx, id: string) {
  if (!(await book.getCampaign(c.env.DB, slug(id, "campaign")))) throw notFound(`no campaign ${id}`);
}

export async function runResults(c: Ctx, run: string): Promise<Response> {
  await authAdmin(c);
  await requireRun(c, run);
  return listResults(c, book.runScope(run));
}

export async function campaignResults(c: Ctx, id: string): Promise<Response> {
  await authAdmin(c);
  await requireCampaign(c, id);
  return listResults(c, book.campaignScope(id));
}

export async function acceptRoute(c: Ctx, run: string): Promise<Response> {
  await authAdmin(c);
  await requireRun(c, run);
  const b = await readJson(c.req);
  const gen = int(b.gen, "gen", { min: 0 });
  const games = int(b.games, "games", { min: 0 });
  const state = unwrap(await bookFor(c.env, book.runScope(run)).accept(run, gen, games, c.now));
  return json({ ok: true, selfplay_state: state });
}

async function ackScope(c: Ctx, scope: string): Promise<Response> {
  const b = await readJson(c.req);
  const cursor = int(b.cursor, "cursor", { min: 0 });
  return json(unwrap(await bookFor(c.env, scope).ack(scope, cursor, c.now)));
}

export async function runAck(c: Ctx, run: string): Promise<Response> {
  await authAdmin(c);
  await requireRun(c, run);
  return ackScope(c, book.runScope(run));
}

export async function campaignAck(c: Ctx, id: string): Promise<Response> {
  await authAdmin(c);
  await requireCampaign(c, id);
  return ackScope(c, book.campaignScope(id));
}

// ---------- campaigns ----------

export async function putCampaign(c: Ctx, id: string): Promise<Response> {
  await authAdmin(c);
  slug(id, "campaign");
  const b = await readJson(c.req);
  const envIn = obj(b.env, "env", { optional: true }) ?? {};
  const env: Record<string, string> = {};
  if (Object.keys(envIn).length > 64) throw bad("env has more than 64 variables");
  for (const [k, v] of Object.entries(envIn)) {
    if (!/^[A-Za-z_][A-Za-z0-9_]{0,63}$/.test(k)) throw bad(`env: bad variable name ${JSON.stringify(k)}`);
    env[k] = str(v, `env.${k}`, { max: 1024 });
  }
  const chunk = b.chunk === undefined || b.chunk === null || b.chunk === "auto" ? "auto" : int(b.chunk, "chunk", { min: 1, max: book.MAX_LEASE_IDS });
  const cfg: book.CampaignCfg = {
    campaign: id,
    build_id: str(b.build_id, "build_id", { re: BUILD_RE }),
    mode: str(b.mode, "mode", { re: NAME_RE }),
    env,
    seed_base: int(b.seed_base, "seed_base", { min: 0, max: 2 ** 52 }),
    chunk,
    priority: num(b.priority, "priority", { optional: true, min: 0, max: 1000 }) ?? 0.2,
    state: oneOf(b.state, "state", STATES, { optional: true }) ?? "active",
    needs: b.needs === undefined || b.needs === null ? null : needsOf(b.needs, "needs"),
    ...routingIn(b, ""),
  };
  await buildOfKind(c.env.DB, cfg.build_id, "worker", "build_id");
  return json(unwrap(await bookFor(c.env, book.campaignScope(id)).putCampaign(cfg, c.now)));
}

export async function getCampaignRoute(c: Ctx, id: string): Promise<Response> {
  await authAdmin(c);
  const x = await book.getCampaign(c.env.DB, slug(id, "campaign"));
  if (!x) throw notFound(`no campaign ${id}`);
  const scope = book.campaignScope(id);
  const [claimed, leased, fails] = await c.env.DB.batch<any>([
    c.env.DB.prepare(`SELECT COALESCE(SUM(hi - lo), 0) AS n FROM claims WHERE scope = ?`).bind(scope),
    c.env.DB.prepare(`SELECT COUNT(*) AS n, COALESCE(SUM(hi - lo - claimed), 0) AS ids FROM leases WHERE scope = ? AND state = 'active'`).bind(scope),
    c.env.DB.prepare(`SELECT COUNT(*) AS n, COUNT(DISTINCT site) AS sites FROM failures WHERE campaign = ?`).bind(id),
  ]);
  return json({
    ...x,
    env: parseJson(x.env, {}),
    needs: parseJson(x.needs, null),
    providers: parseJson(x.providers, null),
    seeds_done: claimed.results[0].n,
    seeds_leased: leased.results[0].ids,
    active_leases: leased.results[0].n,
    pool: await book.poolSize(c.env.DB, scope),
    failures: fails.results[0].n,
    sites: fails.results[0].sites,
    definition: await definitionStatus(c, "campaign", x.campaign, null, x),
  });
}

export async function failures(c: Ctx, id: string): Promise<Response> {
  await authAdmin(c);
  await requireCampaign(c, id);
  const after = int(Number(c.url.searchParams.get("after") ?? "0"), "after", { min: 0 });
  const limit = int(Number(c.url.searchParams.get("limit") ?? "500"), "limit", { min: 1, max: 1000 });
  const { results } = await c.env.DB.prepare(`SELECT * FROM failures WHERE campaign = ? AND id > ? ORDER BY id LIMIT ?`)
    .bind(id, after, limit)
    .all<any>();
  return json({
    failures: results.map((f) => ({ ...f, first_of_site: !!f.first_of_site })),
    cursor: results.length ? results[results.length - 1].id : after,
  });
}

// ---------- machines ----------

export async function machines(c: Ctx): Promise<Response> {
  await authAdmin(c);
  const { results } = await c.env.DB.prepare(
    `SELECT m.*, i.note AS invite_note,
       (SELECT COUNT(*) FROM leases l WHERE l.machine_id = m.machine_id AND l.state = 'active') AS active_leases
     FROM machines m LEFT JOIN invites i ON i.code = m.invite ORDER BY m.created_at`,
  ).all<any>();
  return json({
    machines: results.map(({ token_hash: _omit, ...m }) => ({
      ...m,
      profile: parseJson(m.profile, null),
      caps: parseJson(m.caps, null),
      paused: !!m.paused,
    })),
  });
}

export async function revoke(c: Ctx, id: string): Promise<Response> {
  await authAdmin(c);
  if (!MACHINE_RE.test(id)) throw notFound(`no machine ${id}`);
  const m = await c.env.DB.prepare(`SELECT machine_id FROM machines WHERE machine_id = ?`).bind(id).first();
  if (!m) throw notFound(`no machine ${id}`);
  await c.env.DB.batch([
    c.env.DB.prepare(`UPDATE machines SET revoked_at = ? WHERE machine_id = ? AND revoked_at IS NULL`).bind(c.now, id),
    c.env.DB.prepare(`UPDATE diag_requests SET done_at = ? WHERE machine_id = ? AND done_at IS NULL`).bind(c.now, id),
  ]);
  // It can no longer beat, so its leases end now and their ids go back to the pool.
  const { results } = await c.env.DB.prepare(`SELECT lease_id, scope FROM leases WHERE machine_id = ? AND state = 'active'`)
    .bind(id)
    .all<{ lease_id: string; scope: string }>();
  let cancelled = 0;
  for (const l of results) {
    if (unwrap(await bookFor(c.env, l.scope).cancelLease(l.lease_id, "revoked", c.now))) cancelled++;
  }
  return json({ ok: true, cancelled });
}

export async function updateMachine(c: Ctx, id: string): Promise<Response> {
  await authAdmin(c);
  if (!MACHINE_RE.test(id)) throw notFound(`no machine ${id}`);
  const b = await readJson(c.req);
  const channel = oneOf(b.channel, "channel", CHANNELS, { optional: true });
  const paused = bool(b.paused, "paused", { optional: true });
  const name = str(b.name, "name", { optional: true, min: 1, max: 64 });
  const unblock = bool(b.unblock, "unblock", { optional: true }) ?? false;
  const r = await c.env.DB.prepare(
    `UPDATE machines SET channel = COALESCE(?, channel), paused = COALESCE(?, paused), name = COALESCE(?, name) WHERE machine_id = ?`,
  )
    .bind(channel ?? null, paused === undefined ? null : paused ? 1 : 0, name ?? null, id)
    .run();
  if (!r.meta.changes) throw notFound(`no machine ${id}`);
  // Rev 5: lifts the builds this machine reported as blocked by Windows (/v1/crash kind blocked).
  const unblocked = unblock ? ((await c.env.DB.prepare(`DELETE FROM blocks WHERE machine_id = ?`).bind(id).run()).meta.changes ?? 0) : 0;
  return json(unblock ? { ok: true, unblocked } : { ok: true });
}

export async function pause(c: Ctx): Promise<Response> {
  await authAdmin(c);
  const b = await readJson(c.req);
  const paused = bool(b.paused, "paused");
  await c.env.DB.prepare(`INSERT OR REPLACE INTO settings (key, value) VALUES ('paused', ?)`).bind(paused ? "1" : "0").run();
  return json({ ok: true, paused });
}

// ---------- diagnostics ----------

export async function diag(c: Ctx): Promise<Response> {
  await authAdmin(c);
  const b = await readJson(c.req);
  const kind = oneOf(b.kind, "kind", ["sweep", "profile", "stability", "logs"] as const);
  const params = b.params === undefined || b.params === null ? null : JSON.stringify(opaque(obj(b.params, "params"), "params", 16384));
  let targets: string[];
  if (b.machine_id !== undefined && b.class !== undefined) throw bad("give machine_id or class, not both");
  if (b.machine_id !== undefined) {
    const id = str(b.machine_id, "machine_id", { re: MACHINE_RE });
    const m = await c.env.DB.prepare(`SELECT machine_id FROM machines WHERE machine_id = ? AND revoked_at IS NULL`).bind(id).first();
    if (!m) throw notFound(`no machine ${id}`);
    targets = [id];
  } else if (b.class !== undefined) {
    const cls = str(b.class, "class", { max: 200 });
    const { results } = await c.env.DB.prepare(`SELECT machine_id FROM machines WHERE class = ? AND revoked_at IS NULL`).bind(cls).all<{ machine_id: string }>();
    if (!results.length) throw notFound(`no machine of class ${cls}`);
    targets = results.map((r) => r.machine_id);
  } else throw bad("machine_id or class is required");
  const out: { id: number; machine_id: string }[] = [];
  for (const id of targets) {
    const r = await c.env.DB.prepare(`INSERT INTO diag_requests (machine_id, kind, params, created_at) VALUES (?, ?, ?, ?) RETURNING id`)
      .bind(id, kind, params, c.now)
      .first<{ id: number }>();
    out.push({ id: r!.id, machine_id: id });
  }
  return json({ requests: out });
}

export async function diagList(c: Ctx): Promise<Response> {
  await authAdmin(c);
  const id = c.url.searchParams.get("machine_id");
  if (id !== null && !MACHINE_RE.test(id)) throw bad("bad machine_id");
  const where = id ? `WHERE machine_id = ?` : "";
  const bind = id ? [id] : [];
  const [reqs, ups, res] = await c.env.DB.batch<any>([
    c.env.DB.prepare(
      `SELECT d.*, l.state AS lease_state FROM diag_requests d LEFT JOIN leases l ON l.lease_id = d.lease_id
       ${id ? "WHERE d.machine_id = ?" : ""} ORDER BY d.id DESC LIMIT 200`,
    ).bind(...bind),
    c.env.DB.prepare(`SELECT * FROM diag_uploads ${where} ORDER BY id DESC LIMIT 200`).bind(...bind),
    c.env.DB.prepare(
      `SELECT id, key, lease_id, seq, machine_id, size, t FROM results WHERE scope = 'diag' ${id ? "AND machine_id = ?" : ""} ORDER BY id DESC LIMIT 200`,
    ).bind(...bind),
  ]);
  return json({
    requests: reqs.results.map((r: any) => ({ ...r, params: parseJson(r.params, null) })),
    uploads: ups.results,
    results: res.results,
  });
}

// ---------- dashboard aggregates ----------

export async function stats(c: Ctx): Promise<Response> {
  await authAdmin(c);
  const db = c.env.DB;
  const day = c.now - 86400;
  const hour = c.now - 3600;
  const cfg = config(c.env);
  const [machinesR, leasesR, crashesR, thrR, runsR, campsR, recentCrashR, fuzzR, classR, pausedR, failCountR, releasesR, diagR, blocksR, batchesR, perMachineR, perScopeR, externalR] = await db.batch<any>([
    db.prepare(
      `SELECT machine_id, name, class, channel, agent_version, created_at, last_seen, revoked_at, paused, caps,
         json_extract(profile, '$.idle_h_day') AS idle_h_day, json_extract(profile, '$.on_h_day') AS on_h_day,
         json_extract(profile, '$.providers') AS providers, json_extract(profile, '$.gpus') AS gpus
       FROM machines ORDER BY created_at`,
    ),
    db
      .prepare(
        `SELECT machine_id,
           COUNT(*) AS leases,
           SUM(CASE WHEN state = 'active' THEN 1 ELSE 0 END) AS active,
           SUM(CASE WHEN kind IN ('selfplay', 'eval') THEN units_done ELSE 0 END) AS games,
           SUM(CASE WHEN kind = 'fuzz' THEN units_done ELSE 0 END) AS seeds,
           SUM(MAX(0, COALESCE(closed_at, last_beat, issued_at) - issued_at - paused_s)) AS busy_s,
           SUM(paused_s) AS paused_s,
           SUM(CASE WHEN state = 'expired' THEN 1 ELSE 0 END) AS expired,
           SUM(CASE WHEN state = 'given_back' THEN 1 ELSE 0 END) AS given_back,
           SUM(CASE WHEN state = 'cancelled' THEN 1 ELSE 0 END) AS cancelled,
           SUM(CASE WHEN close_reason LIKE 'refused:%' THEN 1 ELSE 0 END) AS refused,
           SUM(COALESCE(json_extract(stats, '$.yields'), 0)) AS yields,
           SUM(COALESCE(json_extract(stats, '$.starved_ms'), 0)) AS starved_ms,
           MAX(COALESCE(json_extract(stats, '$.yield_ms_max'), 0)) AS yield_ms_max,
           GROUP_CONCAT(CASE WHEN state = 'active' THEN kind || ':' || COALESCE(run, campaign, 'diag') END) AS working_on
         FROM leases WHERE issued_at >= ? OR state = 'active' GROUP BY machine_id`,
      )
      .bind(day),
    db.prepare(`SELECT machine_id, COUNT(*) AS n FROM crashes WHERE t >= ? GROUP BY machine_id`).bind(day),
    db
      .prepare(
        `SELECT kind, SUM(CASE WHEN t >= ? THEN games ELSE 0 END) AS h1, SUM(games) AS h24, COUNT(*) AS chunks
         FROM results WHERE t >= ? GROUP BY kind`,
      )
      .bind(hour, day),
    db.prepare(`SELECT * FROM runs ORDER BY priority DESC, run`),
    db.prepare(`SELECT * FROM campaigns ORDER BY priority DESC, campaign`),
    db
      .prepare(`SELECT c.*, m.name FROM crashes c LEFT JOIN machines m ON m.machine_id = c.machine_id ORDER BY c.id DESC LIMIT 20`),
    db
      .prepare(
        `SELECT campaign, seed, site, panic, machine_id, t FROM failures WHERE first_of_site = 1 ORDER BY id DESC LIMIT 20`,
      ),
    db
      .prepare(
        `SELECT m.class, r.kind, r.scope, AVG(r.rate) AS rate, COUNT(*) AS machines FROM rates r
         JOIN machines m ON m.machine_id = r.machine_id GROUP BY m.class, r.kind, r.scope ORDER BY m.class, r.kind, r.scope`,
      ),
    db.prepare(`SELECT value FROM settings WHERE key = 'paused'`),
    db
      .prepare(
        `SELECT (SELECT COUNT(*) FROM crashes WHERE t >= ?) AS crashes, (SELECT COUNT(*) FROM failures WHERE t >= ?) AS fuzz_failures,
           (SELECT COUNT(*) FROM leases WHERE state = 'expired' AND closed_at >= ?) AS expired,
           (SELECT COUNT(*) FROM leases WHERE state = 'given_back' AND closed_at >= ?) AS given_back`,
      )
      .bind(day, day, day, day),
    db.prepare(
      `SELECT r.channel, r.version, r.build_id, r.min_agent, r.t, r.github_url FROM agent_releases r
       WHERE r.id = (SELECT MAX(id) FROM agent_releases x WHERE x.channel = r.channel) ORDER BY r.channel`,
    ),
    db.prepare(
      `SELECT d.id, d.machine_id, m.name, d.name AS what, d.key, d.size, d.user, d.t FROM diag_uploads d
       LEFT JOIN machines m ON m.machine_id = d.machine_id ORDER BY d.id DESC LIMIT 20`,
    ),
    db.prepare(`SELECT * FROM blocks WHERE until > ? ORDER BY t DESC`).bind(c.now),
    db.prepare(`SELECT * FROM eval_batches ORDER BY created_at DESC, run, lo DESC LIMIT 20`),
    // Credited ids per machine and per run/campaign, by kind, over the last hour and day.
    db
      .prepare(
        `SELECT machine_id, kind, SUM(CASE WHEN t >= ? THEN games ELSE 0 END) AS h1, SUM(games) AS h24
         FROM results WHERE t >= ? AND kind IN ('selfplay', 'eval', 'fuzz') GROUP BY machine_id, kind`,
      )
      .bind(hour, day),
    db
      .prepare(
        `SELECT scope, kind, SUM(CASE WHEN t >= ? THEN games ELSE 0 END) AS h1, SUM(games) AS h24
         FROM results WHERE t >= ? AND kind IN ('selfplay', 'eval', 'fuzz') GROUP BY scope, kind`,
      )
      .bind(hour, day),
    db.prepare(`SELECT * FROM external_sources ORDER BY reporter, id`),
  ]);

  const per = new Map<string, any>(leasesR.results.map((r: any) => [r.machine_id, r]));
  const crashes = new Map<string, number>(crashesR.results.map((r: any) => [r.machine_id, r.n]));
  const machinesOut = machinesR.results.map((m: any) => {
    const l = per.get(m.machine_id) ?? {};
    // Builds Windows refused to start on this PC (rev 5), unless a newer agent has asked since.
    const blocked = blocksR.results
      .filter((x: any) => x.machine_id === m.machine_id && !blockLifted(x, m.agent_version))
      .map((x: any) => ({ build_id: x.build_id, until: x.until, os_error: x.os_error, message: x.message, agent_version: x.agent_version, t: x.t }));
    return {
      machine_id: m.machine_id,
      name: m.name,
      class: m.class,
      channel: m.channel,
      agent_version: m.agent_version,
      last_seen: m.last_seen,
      revoked: m.revoked_at !== null,
      paused: !!m.paused,
      caps: parseJson(m.caps, null),
      // Rev 7: the execution providers that passed this PC's probe, and its adapters' LUIDs.
      providers: parseJson<unknown>(m.providers, null),
      luids: parseJson<unknown[]>(m.gpus, [])
        .map((g: any) => g?.luid)
        .filter((x: unknown) => typeof x === "string"),
      working_on: l.working_on ?? null,
      active_leases: l.active ?? 0,
      leases_24h: l.leases ?? 0,
      games_24h: l.games ?? 0,
      seeds_24h: l.seeds ?? 0,
      busy_s_24h: l.busy_s ?? 0,
      paused_s_24h: l.paused_s ?? 0,
      yields_24h: l.yields ?? 0,
      starved_s_24h: Math.round((l.starved_ms ?? 0) / 1000),
      idle_h_day: m.idle_h_day ?? null,
      on_h_day: m.on_h_day ?? null,
      yield_ms_max: l.yield_ms_max ?? 0,
      given_back_24h: l.given_back ?? 0,
      expired_24h: l.expired ?? 0,
      cancelled_24h: l.cancelled ?? 0,
      crashes_24h: crashes.get(m.machine_id) ?? 0,
      refused_24h: l.refused ?? 0,
      blocked,
    };
  });

  const defs = await currentDefs(db);
  const runs = [];
  for (const r of runsR.results as book.RunRow[]) {
    const scope = book.runScope(r.run);
    const sp = r.selfplay ? unwrap(await bookFor(c.env, scope).selfplayState(r.run)) : null;
    const extra = await db
      .prepare(
        `SELECT (SELECT COUNT(*) FROM leases WHERE scope = ? AND state = 'active') AS active,
           (SELECT COUNT(*) FROM results WHERE scope = ? AND acked_at IS NULL) AS unacked,
           (SELECT COALESCE(SUM(hi - lo), 0) FROM pool WHERE scope = ?) AS pool`,
      )
      .bind(scope, scope, scope)
      .first<any>();
    const cur = r.eval_batch ? await book.getBatch(db, r.run, r.eval_batch) : null;
    runs.push({
      run: r.run,
      game: r.game,
      state: r.state,
      priority: r.priority,
      build_id: r.build_id,
      current_gen: r.current_gen,
      next_gid: r.next_gid,
      selfplay: sp,
      eval: cur ? await book.evalProgress(db, cur) : null,
      active_leases: extra.active,
      unacked_results: extra.unacked,
      pool: extra.pool,
      definition: await definitionStatus(c, "run", r.run, r, null, defs.get(`run:${r.run}`) ?? null),
    });
  }
  const campaigns = [];
  for (const x of campsR.results as book.CampaignRow[]) {
    const scope = book.campaignScope(x.campaign);
    const e = await db
      .prepare(
        `SELECT (SELECT COUNT(*) FROM leases WHERE scope = ? AND state = 'active') AS active,
           (SELECT COALESCE(SUM(hi - lo), 0) FROM claims WHERE scope = ?) AS done,
           (SELECT COALESCE(SUM(hi - lo), 0) FROM pool WHERE scope = ?) AS pool,
           (SELECT COUNT(*) FROM failures WHERE campaign = ?) AS failures,
           (SELECT COUNT(DISTINCT site) FROM failures WHERE campaign = ?) AS sites`,
      )
      .bind(scope, scope, scope, x.campaign, x.campaign)
      .first<any>();
    campaigns.push({
      campaign: x.campaign,
      mode: x.mode,
      state: x.state,
      priority: x.priority,
      build_id: x.build_id,
      seed_base: x.seed_base,
      cursor: x.cursor,
      seeds_done: e.done,
      pool: e.pool,
      active_leases: e.active,
      failures: e.failures,
      sites: e.sites,
      definition: await definitionStatus(c, "campaign", x.campaign, null, x, defs.get(`campaign:${x.campaign}`) ?? null),
    });
  }

  const evalBatchesOut = [];
  for (const b of batchesR.results as book.EvalBatchRow[]) evalBatchesOut.push({ run: b.run, ...(await book.evalProgress(db, b)) });

  // Contributors: the farm's PCs and the owner's own machines (POST /v1/admin/external) together.
  const win = (rows: any[], key: string, field: string) => {
    const out = new Map<string, Record<string, { h1: number; h24: number }>>();
    for (const r of rows) {
      const o = out.get(r[key]) ?? {};
      o[r[field]] = { h1: r.h1 ?? 0, h24: r.h24 ?? 0 };
      out.set(r[key], o);
    }
    return out;
  };
  const perMachine = win(perMachineR.results, "machine_id", "kind");
  const none = { h1: 0, h24: 0 };
  const contributors: Contributor[] = [
    ...machinesOut.map((m: any): Contributor => {
      const w = perMachine.get(m.machine_id) ?? {};
      const status = m.revoked ? "revoked" : m.active_leases > 0 ? "working" : m.paused ? "paused" : m.last_seen && c.now - m.last_seen < 600 ? "online" : "offline";
      return {
        origin: "farm",
        kind: "farm",
        id: m.machine_id,
        name: m.name,
        hardware: m.class,
        status,
        last_seen: m.last_seen,
        working_on: m.working_on ? String(m.working_on).split(",") : [],
        selfplay: w.selfplay ?? none,
        eval: w.eval ?? none,
        fuzz: w.fuzz ?? none,
        busy_h_24h: Math.round(((m.busy_s_24h ?? 0) / 3600) * 10) / 10,
      };
    }),
    ...(externalR.results as ExternalRow[]).map((r) => ownContributor(r, c.now)),
  ];
  const perScope = win(perScopeR.results, "scope", "kind");
  const scopes = new Set<string>([...perScope.keys()]);
  for (const x of contributors) if (x.origin === "own" && x.status !== "offline") for (const s of x.working_on) scopes.add(s);
  const byScope = [...scopes].sort().map((scope) => ({
    scope,
    farm: { selfplay: perScope.get(scope)?.selfplay ?? none, eval: perScope.get(scope)?.eval ?? none, fuzz: perScope.get(scope)?.fuzz ?? none },
    own: contributors.filter((x) => x.origin === "own" && x.status !== "offline" && x.working_on.includes(scope)).map((x) => x.name),
  }));
  const blocks = blocksR.results
    .map((x: any) => {
      const m = machinesOut.find((y: any) => y.machine_id === x.machine_id);
      return m && !blockLifted(x, m.agent_version) ? { ...x, name: m.name } : null;
    })
    .filter(Boolean);

  return json({
    t: c.now,
    auth: c.admin ?? null,
    paused: pausedR.results[0]?.value === "1",
    limits: {
      definition_agent: cfg.definitionAgent,
      data_keys: cfg.dataPubkeys.length,
      max_active_leases: cfg.maxActiveLeases,
      max_lease_chunks: cfg.maxLeaseChunks,
      max_lease_bytes: cfg.maxLeaseBytes,
      late_results_s: cfg.lateResultsS,
      max_crashes_per_day: cfg.maxCrashesPerDay,
      crash_retention_days: cfg.crashRetentionDays,
      diag_retention_days: cfg.diagRetentionDays,
      github_repo: cfg.githubRepo,
      access: accessEnabled(c.env),
    },
    contributors,
    totals: totals(contributors, c.now),
    by_scope: byScope,
    blocks,
    throughput: Object.fromEntries(thrR.results.map((r: any) => [r.kind, { last_hour: r.h1, last_day: r.h24, chunks_day: r.chunks }])),
    machines: machinesOut,
    runs,
    campaigns,
    eval_batches: evalBatchesOut,
    classes: classR.results,
    releases: releasesR.results,
    diag_uploads: diagR.results.map((d: any) => ({ ...d, user: !!d.user })),
    failures: {
      last_day: failCountR.results[0],
      crashes: recentCrashR.results.map((x: any) => ({ ...x, backtrace: x.backtrace ? String(x.backtrace).slice(0, 2000) : null })),
      fuzz_sites: fuzzR.results,
    },
  });
}

// ---------- definitions (rev 6, PROTOCOL 7) ----------

// PUT /v1/admin/definition {"kind", "id", "version", "key", "sha256"}: registers a definition whose
// definition.json and .minisig were uploaded to `key` (defs/<kind>/<id>/<version>/...), after
// checking its bytes, its signature (DATA_PUBKEYS only), its contents and its builds, and makes it
// the current definition of that run or campaign.
export async function putDefinition(c: Ctx): Promise<Response> {
  await authAdmin(c);
  const b = await readJson(c.req);
  const kind = oneOf(b.kind, "kind", DEF_KINDS);
  const id = slug(str(b.id, "id", { max: 63 }), "id");
  const version = int(b.version, "version", { min: 1 });
  const key = str(b.key, "key", { max: 200 });
  if (key !== defKey(kind, id, version)) throw bad(`key must be ${defKey(kind, id, version)}`);
  const sha = str(b.sha256, "sha256", { re: HEX64_RE });
  const keys = config(c.env).dataPubkeys;
  if (!keys.length) throw bad("DATA_PUBKEYS is not set on the coordinator: it can't verify definitions");

  const obj = await c.env.BUCKET.get(key);
  if (!obj) throw bad(`${key} is not uploaded`);
  if (obj.size > MAX_DEFINITION) throw bad(`definition.json is larger than ${MAX_DEFINITION} bytes`);
  const bytes = new Uint8Array(await obj.arrayBuffer());
  const digest = await sha256Hex(bytes);
  if (digest !== sha) throw bad(`${key} has sha256 ${digest}, not ${sha}`);
  const sigObj = await c.env.BUCKET.get(`${key}.minisig`);
  if (!sigObj) throw bad(`${key}.minisig is not uploaded`);
  if (sigObj.size > MAX_SIGNATURE) throw bad(`${key}.minisig is larger than ${MAX_SIGNATURE} bytes`);
  const sig = await sigObj.text();
  const why = await verifyMinisign(bytes, sig, keys, "DATA_PUBKEYS");
  if (why) throw bad(`signature: ${why}`);

  const d = parseDefinition(new TextDecoder().decode(bytes));
  const problems: string[] = [];
  if (d.kind !== kind || d.id !== id || d.version !== version) {
    problems.push(`the signed definition is ${d.kind} ${d.id} version ${d.version}, not ${kind} ${id} version ${version}`);
  }
  if (d.expires !== null && d.expires <= c.now) problems.push(`it expired at ${d.expires}`);
  const ids = [...d.builds, ...(d.runtime ? [d.runtime] : [])];
  const { results } = await c.env.DB.prepare(`SELECT build_id, kind, game FROM builds WHERE build_id IN (${ids.map(() => "?").join(",")})`)
    .bind(...ids)
    .all<{ build_id: string; kind: string; game: string | null }>();
  const reg = new Map(results.map((r) => [r.build_id, r]));
  for (const w of d.builds) {
    const r = reg.get(w);
    if (!r) problems.push(`build ${w} is not registered (POST /v1/admin/build first)`);
    else if (r.kind !== "worker") problems.push(`build ${w} is a ${r.kind} build, not a worker`);
    else if (r.game !== d.game) problems.push(`build ${w} is for game ${r.game}, not ${d.game}`);
  }
  if (d.runtime) {
    const r = reg.get(d.runtime);
    if (!r) problems.push(`runtime ${d.runtime} is not registered`);
    else if (r.kind !== "runtime") problems.push(`runtime ${d.runtime} is a ${r.kind} build`);
  }
  // Gens already registered on the run must be exactly the signed files (they can't change).
  const gens = Object.keys(d.checkpoints).map(Number);
  if (kind === "run" && gens.length) {
    const { results: ck } = await c.env.DB.prepare(`SELECT * FROM checkpoints WHERE run = ?`).bind(id).all<book.CheckpointRow>();
    for (const row of ck) {
      if (!d.checkpoints[String(row.gen)]) continue;
      const p = checkpointProblem({ ...d, key, sha256: sha, size: bytes.length, key_id: "", registered_at: 0 }, book.ckptFacts(row));
      if (p) problems.push(p);
    }
  }
  if (problems.length) throw bad(`definition ${kind} ${id} version ${version} can't be registered`, { problems });

  const row: DefinitionRow = {
    kind,
    id,
    version,
    key,
    sha256: sha,
    size: bytes.length,
    created: d.created,
    expires: d.expires,
    game: d.game,
    builds: JSON.stringify(d.builds),
    runtime: d.runtime,
    checkpoints: JSON.stringify(d.checkpoints),
    jobs: JSON.stringify(d.jobs),
    key_id: minisignKeyId(sig) ?? "",
    registered_at: c.now,
  };
  const scope = kind === "run" ? book.runScope(id) : book.campaignScope(id);
  return json(unwrap(await bookFor(c.env, scope).putDefinition(row)));
}

// What the current definition of a run or campaign allows now, for the admin page and GET run /
// campaign: why some job can't be leased (problems), or that there is none (legacy only).
export interface DefStatus {
  version: number | null;
  key: string | null;
  sha256: string | null;
  created: number | null;
  expires: number | null;
  registered_at: number | null;
  key_id: string | null;
  builds: string[];
  runtime: string | null;
  gens: number[];
  jobs: string[];
  problems: string[];
}

async function definitionStatus(
  c: Ctx,
  kind: "run" | "campaign",
  id: string,
  run: book.RunRow | null,
  camp: book.CampaignRow | null,
  known?: Def | null,
): Promise<DefStatus> {
  const d = known === undefined ? await currentDef(c.env.DB, kind, id) : known;
  const agent = config(c.env).definitionAgent;
  if (!d) {
    return {
      version: null,
      key: null,
      sha256: null,
      created: null,
      expires: null,
      registered_at: null,
      key_id: null,
      builds: [],
      runtime: null,
      gens: [],
      jobs: [],
      problems: [`no definition: only agents older than ${agent} get its work (legacy leases)`],
    };
  }
  const problems: string[] = [];
  const say = (p: string | null) => {
    if (p && !problems.includes(p)) problems.push(p);
  };
  if (run) {
    if (parseJson<book.SelfplayCfg | null>(run.selfplay, null)) {
      say(leaseProblem(d, "selfplay", run.build_id, run.runtime, c.now));
      if (run.current_gen !== null) {
        const ck = await c.env.DB.prepare(`SELECT * FROM checkpoints WHERE run = ? AND gen = ?`).bind(run.run, run.current_gen).first<book.CheckpointRow>();
        if (ck) say(checkpointProblem(d, book.ckptFacts(ck)));
      }
    }
    const ev = parseJson<book.EvalCfg | null>(run.eval, null);
    if (ev && Array.isArray(ev.checkpoints)) {
      say(leaseProblem(d, "eval", run.build_id, run.runtime, c.now));
      if (d.jobs.eval) say(evalProblem(d, { checkpoints: ev.checkpoints, paired: book.isPaired(ev) }));
      if (ev.checkpoints.length) {
        const { results } = await c.env.DB.prepare(`SELECT * FROM checkpoints WHERE run = ? AND gen IN (${ev.checkpoints.map(() => "?").join(",")})`)
          .bind(run.run, ...ev.checkpoints)
          .all<book.CheckpointRow>();
        for (const ck of results) say(checkpointProblem(d, book.ckptFacts(ck)));
      }
    }
  }
  if (camp) say(leaseProblem(d, "fuzz", camp.build_id, null, c.now));
  if (d.expires !== null && d.expires <= c.now) say(`definition v${d.version} expired`);
  return {
    version: d.version,
    key: d.key,
    sha256: d.sha256,
    created: d.created,
    expires: d.expires,
    registered_at: d.registered_at,
    key_id: d.key_id,
    builds: d.builds,
    runtime: d.runtime,
    gens: Object.keys(d.checkpoints)
      .map(Number)
      .sort((a, b) => a - b),
    jobs: (["selfplay", "eval", "fuzz"] as const).filter((k) => d.jobs[k]),
    problems,
  };
}

// ---------- the owner's own machines (POST /v1/admin/external) ----------

// A desktop reporter's numbers for machines that don't run the agent (external.ts). Each POST
// replaces all sources of its reporter. Admin page only.
export async function external(c: Ctx): Promise<Response> {
  await authAdmin(c);
  const report = parseReport(await readJson(c.req, MAX_EXTERNAL_BODY));
  const n = await storeReport(c.env.DB, report, c.now);
  return json({ ok: true, reporter: report.reporter, sources: n });
}
