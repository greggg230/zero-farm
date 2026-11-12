// What a LeaseBook does for its scope ("run:<run>", "campaign:<id>" or "diag"). Every function
// here runs inside that scope's Durable Object, one at a time (leasebook.ts), so a read followed
// by a write sees no other change to the scope in between. Each change is one D1 batch (atomic).
//
// Id accounting (game ids per run, seeds per campaign). Every id below the scope's counter
// (runs.next_gid, campaigns.cursor) is, at any moment, exactly one of:
//   - claimed: credited to the first result that reported it (table claims), or
//   - unclaimed inside an active lease's [lo, hi), or
//   - in the pool (handed back by an expired / given-back / cancelled / finished lease).
// A late result can claim ids that were already handed to another lease or back to the pool: the
// first copy wins, the pool loses them, and the other lease's copy is counted as a duplicate.

import { intersect, normalize, parseRanges, subtract, total, type Range } from "./ranges";
import {
  type Json,
  bad,
  conflict,
  gone,
  newLeaseId,
  notFound,
  parseJson,
} from "./util";

export type Kind = "fuzz" | "eval" | "selfplay" | "diag";

export const BEAT_S = 60;
export const SLACK_S = 600;
export const EXPIRE_BEATS = 3; // a lease silent for 3 beats expires
export const MAX_LEASE_IDS = 1_000_000; // no lease covers more ids than this

export const runScope = (run: string) => `run:${run}`;
export const campaignScope = (c: string) => `campaign:${c}`;
export const DIAG_SCOPE = "diag";

export interface SelfplayCfg {
  quota: number;
  slack: number;
  max_stale: number;
  games_per_lease: "auto" | number;
  gpu: "any" | "preferred" | "required";
  needs: Needs;
  job: Record<string, Json>;
}
export interface EvalCfg {
  batch: string;
  games: number;
  games_per_lease: "auto" | number;
  gen: number | null;
  gpu: "any" | "preferred" | "required";
  needs: Needs;
  job: Record<string, Json>;
}
export interface Needs {
  threads: number;
  ram_mb: number;
}

export interface RunRow {
  run: string;
  game: string;
  build_id: string;
  runtime: string | null;
  priority: number;
  state: string;
  selfplay: string | null;
  eval: string | null;
  eval_batch: string | null;
  next_gid: number;
  current_gen: number | null;
  created_at: number;
  updated_at: number;
}

export interface CampaignRow {
  campaign: string;
  build_id: string;
  mode: string;
  env: string;
  seed_base: number;
  cursor: number;
  chunk: string;
  priority: number;
  state: string;
  needs: string | null;
  created_at: number;
  updated_at: number;
}

export interface LeaseRow {
  lease_id: string;
  machine_id: string;
  scope: string;
  run: string | null;
  campaign: string | null;
  kind: Kind;
  pool: string | null;
  lo: number | null;
  hi: number | null;
  claimed: number;
  units_done: number;
  gen: number | null;
  build_id: string;
  diag_id: number | null;
  lease: string;
  state: "active" | "done" | "given_back" | "expired" | "cancelled";
  pending: "cancel" | "give_back" | null;
  beat_s: number;
  slack_s: number;
  issued_at: number;
  expires_at: number;
  last_beat: number | null;
  beat_state: string | null;
  paused_since: number | null;
  paused_s: number;
  progress: string | null;
  stats: string | null;
  closed_at: number | null;
  close_reason: string | null;
}

export interface CheckpointRow {
  run: string;
  gen: number;
  model: string;
  sha256: string;
  reference: string | null;
  reference_sha256: string | null;
}

export const checkpointOf = (c: CheckpointRow) => ({
  gen: c.gen,
  model: c.model,
  sha256: c.sha256,
  reference: c.reference,
  reference_sha256: c.reference_sha256,
});

type Stmt = D1PreparedStatement;

export async function getLease(db: D1Database, id: string): Promise<LeaseRow | null> {
  return db.prepare(`SELECT * FROM leases WHERE lease_id = ?`).bind(id).first<LeaseRow>();
}
export async function getRun(db: D1Database, run: string): Promise<RunRow | null> {
  return db.prepare(`SELECT * FROM runs WHERE run = ?`).bind(run).first<RunRow>();
}
export async function getCampaign(db: D1Database, c: string): Promise<CampaignRow | null> {
  return db.prepare(`SELECT * FROM campaigns WHERE campaign = ?`).bind(c).first<CampaignRow>();
}
async function getCheckpoint(db: D1Database, run: string, gen: number | null): Promise<CheckpointRow | null> {
  if (gen === null) return null;
  return db.prepare(`SELECT * FROM checkpoints WHERE run = ? AND gen = ?`).bind(run, gen).first<CheckpointRow>();
}

// ---------- pool and claims ----------

// Claimed ids inside `rs` (normalized). Claims never span more than one lease's ids, so only
// claims starting less than MAX_LEASE_IDS below the span can reach into it.
export async function claimedIn(db: D1Database, scope: string, rs: Range[]): Promise<Range[]> {
  if (!rs.length) return [];
  const lo = rs[0][0];
  const hi = rs[rs.length - 1][1];
  const { results } = await db
    .prepare(`SELECT lo, hi FROM claims WHERE scope = ? AND lo < ? AND lo > ? AND hi > ?`)
    .bind(scope, hi, lo - MAX_LEASE_IDS - 1, lo)
    .all<{ lo: number; hi: number }>();
  return intersect(normalize(results.map((r) => [r.lo, r.hi] as Range)), rs);
}

// Statements that add `ranges` to a pool, merged with touching neighbours. Adding an id that is
// already pooled would mean it was counted twice somewhere: that is a bug, so it throws.
async function poolAdd(db: D1Database, scope: string, pool: string, ranges: Range[]): Promise<Stmt[]> {
  if (!ranges.length) return [];
  const lo = ranges[0][0];
  const hi = ranges[ranges.length - 1][1];
  const { results } = await db
    .prepare(`SELECT lo, hi FROM pool WHERE scope = ? AND pool = ? AND lo <= ? AND hi >= ?`)
    .bind(scope, pool, hi, lo)
    .all<{ lo: number; hi: number }>();
  const old = results.map((r) => [r.lo, r.hi] as Range);
  const merged = normalize([...old, ...ranges.map((r) => [r[0], r[1]] as Range)]);
  if (total(merged) !== total(old) + total(ranges)) {
    throw new Error(`pool ${scope}/${pool}: ${JSON.stringify(ranges)} overlaps ids already pooled`);
  }
  return [
    db.prepare(`DELETE FROM pool WHERE scope = ? AND pool = ? AND lo <= ? AND hi >= ?`).bind(scope, pool, hi, lo),
    ...merged.map(([a, b]) => db.prepare(`INSERT INTO pool (scope, pool, lo, hi) VALUES (?, ?, ?, ?)`).bind(scope, pool, a, b)),
  ];
}

// Statements that take `ranges` out of a pool (ids a late result just claimed).
async function poolRemove(db: D1Database, scope: string, pool: string, ranges: Range[]): Promise<Stmt[]> {
  if (!ranges.length) return [];
  const lo = ranges[0][0];
  const hi = ranges[ranges.length - 1][1];
  const { results } = await db
    .prepare(`SELECT lo, hi FROM pool WHERE scope = ? AND pool = ? AND lo < ? AND hi > ?`)
    .bind(scope, pool, hi, lo)
    .all<{ lo: number; hi: number }>();
  if (!results.length) return [];
  const old = normalize(results.map((r) => [r.lo, r.hi] as Range));
  const left = subtract(old, ranges);
  return [
    db.prepare(`DELETE FROM pool WHERE scope = ? AND pool = ? AND lo < ? AND hi > ?`).bind(scope, pool, hi, lo),
    ...left.map(([a, b]) => db.prepare(`INSERT INTO pool (scope, pool, lo, hi) VALUES (?, ?, ?, ?)`).bind(scope, pool, a, b)),
  ];
}

// The lowest pooled ids, at most n of them, as one contiguous range.
async function poolTake(
  db: D1Database,
  scope: string,
  pool: string,
  n: number,
): Promise<{ range: Range; stmts: Stmt[] } | null> {
  const row = await db
    .prepare(`SELECT lo, hi FROM pool WHERE scope = ? AND pool = ? ORDER BY lo LIMIT 1`)
    .bind(scope, pool)
    .first<{ lo: number; hi: number }>();
  if (!row) return null;
  const hi = Math.min(row.hi, row.lo + n);
  const stmts = [db.prepare(`DELETE FROM pool WHERE scope = ? AND pool = ? AND lo = ?`).bind(scope, pool, row.lo)];
  if (hi < row.hi) {
    stmts.push(db.prepare(`INSERT INTO pool (scope, pool, lo, hi) VALUES (?, ?, ?, ?)`).bind(scope, pool, hi, row.hi));
  }
  return { range: [row.lo, hi], stmts };
}

export async function poolSize(db: D1Database, scope: string, pool?: string): Promise<number> {
  const q = pool
    ? db.prepare(`SELECT COALESCE(SUM(hi - lo), 0) AS n FROM pool WHERE scope = ? AND pool = ?`).bind(scope, pool)
    : db.prepare(`SELECT COALESCE(SUM(hi - lo), 0) AS n FROM pool WHERE scope = ?`).bind(scope);
  return (await q.first<{ n: number }>())?.n ?? 0;
}

// ---------- quota ----------

export interface SelfplayState {
  gen: number | null;
  quota: number;
  cap: number; // quota * (1 + slack)
  accepted: number; // trainer's count for the current generation
  leased: number; // unclaimed ids in active self-play leases
  pending: number; // credited games the trainer hasn't acked yet
  in_flight: number; // leased + pending
  headroom: number; // cap - accepted - in_flight
}

export async function selfplayState(db: D1Database, run: RunRow, sp: SelfplayCfg): Promise<SelfplayState> {
  const scope = runScope(run.run);
  const cap = Math.floor(sp.quota * (1 + sp.slack) + 1e-9);
  const [acc, leased, pending] = await db.batch<{ n: number }>([
    db.prepare(`SELECT accepted AS n FROM quotas WHERE run = ? AND gen = ?`).bind(run.run, run.current_gen ?? -1),
    db
      .prepare(`SELECT COALESCE(SUM(hi - lo - claimed), 0) AS n FROM leases WHERE scope = ? AND kind = 'selfplay' AND state = 'active'`)
      .bind(scope),
    db
      .prepare(`SELECT COALESCE(SUM(games), 0) AS n FROM results WHERE scope = ? AND kind = 'selfplay' AND acked_at IS NULL`)
      .bind(scope),
  ]);
  const accepted = acc.results[0]?.n ?? 0;
  const l = leased.results[0]?.n ?? 0;
  const p = pending.results[0]?.n ?? 0;
  return {
    gen: run.current_gen,
    quota: sp.quota,
    cap,
    accepted,
    leased: l,
    pending: p,
    in_flight: l + p,
    headroom: cap - accepted - l - p,
  };
}

// ---------- expiry and closing ----------

export async function sweep(env: Env, scope: string, now: number): Promise<number> {
  const { results } = await env.DB.prepare(`SELECT * FROM leases WHERE scope = ? AND state = 'active' AND expires_at < ?`)
    .bind(scope, now)
    .all<LeaseRow>();
  for (const l of results) await closeLease(env, l, "expired", "silent", now);
  return results.length;
}

// Ids per running second, folded into the machine's rate for this kind and scope. Paused time and
// time the worker was starved of CPU (Stats `starved_ms`) are not running time.
function rateStmt(db: D1Database, l: LeaseRow, end: number): Stmt | null {
  let paused = l.paused_s;
  if (l.beat_state === "paused" && l.last_beat !== null) paused += Math.max(0, end - l.last_beat);
  const starvedMs = Number(parseJson<Record<string, unknown>>(l.stats, {}).starved_ms);
  const starved = Number.isFinite(starvedMs) && starvedMs > 0 ? starvedMs / 1000 : 0;
  const running = end - l.issued_at - paused - starved;
  if (l.units_done <= 0 || running < 30) return null;
  return db
    .prepare(
      `INSERT INTO rates (machine_id, kind, scope, rate, samples, t) VALUES (?, ?, ?, ?, 1, ?)
       ON CONFLICT (machine_id, kind, scope) DO UPDATE
       SET rate = rate * 0.7 + excluded.rate * 0.3, samples = samples + 1, t = excluded.t`,
    )
    .bind(l.machine_id, l.kind, l.scope, l.units_done / running, end);
}

// Closes an active lease: its unclaimed ids go back to the pool (unless they belong to an eval
// batch that has since been replaced), and its rate is recorded.
async function closeLease(
  env: Env,
  l: LeaseRow,
  state: "done" | "given_back" | "expired" | "cancelled",
  reason: string,
  now: number,
  extra: Stmt[] = [],
): Promise<Range[]> {
  const db = env.DB;
  const stmts: Stmt[] = [];
  let unfinished: Range[] = [];
  if (l.pool !== null && l.lo !== null && l.hi !== null) {
    unfinished = subtract([[l.lo, l.hi]], await claimedIn(db, l.scope, [[l.lo, l.hi]]));
    let keep = true;
    if (l.pool.startsWith("eval:") && l.run) {
      const run = await getRun(db, l.run);
      keep = run?.eval_batch === l.pool.slice(5);
    }
    if (keep) stmts.push(...(await poolAdd(db, l.scope, l.pool, unfinished)));
  }
  stmts.push(
    db
      .prepare(
        `UPDATE leases SET state = ?, closed_at = ?, close_reason = ?, pending = NULL WHERE lease_id = ? AND state = 'active'`,
      )
      .bind(state, now, reason, l.lease_id),
  );
  if (state !== "cancelled") {
    const r = rateStmt(db, l, state === "expired" ? (l.last_beat ?? l.issued_at) : now);
    if (r) stmts.push(r);
  }
  if (l.diag_id !== null && (state === "done" || state === "given_back")) {
    stmts.push(db.prepare(`UPDATE diag_requests SET done_at = ? WHERE id = ? AND done_at IS NULL`).bind(now, l.diag_id));
  }
  await db.batch([...stmts, ...extra]);
  return unfinished;
}

export async function cancelLease(env: Env, leaseId: string, reason: string, now: number): Promise<boolean> {
  const l = await getLease(env.DB, leaseId);
  if (!l || l.state !== "active") return false;
  await closeLease(env, l, "cancelled", reason, now);
  return true;
}

// The next beat of every active lease in the scope (optionally only one kind) answers "cancel".
export async function cancelScope(env: Env, scope: string, kind: Kind | null): Promise<number> {
  const q = kind
    ? env.DB.prepare(`UPDATE leases SET pending = 'cancel' WHERE scope = ? AND state = 'active' AND kind = ?`).bind(scope, kind)
    : env.DB.prepare(`UPDATE leases SET pending = 'cancel' WHERE scope = ? AND state = 'active'`).bind(scope);
  return (await q.run()).meta.changes ?? 0;
}

// ---------- granting ----------

export interface GrantReq {
  now: number;
  machine_id: string;
  kind: Kind;
  want: number; // ids to aim for (lease sizing happens in the Worker, from measured rates)
  run?: string;
  campaign?: string;
  diag_id?: number;
}

export interface Lease {
  lease_id: string;
  kind: Kind;
  issued_at: number;
  expires_at: number;
  beat_s: number;
  slack_s: number;
  run: string | null;
  campaign: string | null;
  build: { build_id: string; manifest: string };
  runtime: { build_id: string; manifest: string } | null;
  checkpoint: ReturnType<typeof checkpointOf> | null;
  job: Record<string, Json>;
}

const buildRef = (id: string) => ({ build_id: id, manifest: `builds/${id}/manifest.json` });

// The run's base seed: the job's "seed" if it sets one, else derived from the run name. It is the
// same for every lease of the run; workers derive each game's seed from (seed, gid).
export function baseSeed(name: string, job: Record<string, Json>): number {
  if (typeof job.seed === "number" && Number.isSafeInteger(job.seed)) return job.seed;
  let h = 0x811c9dc5;
  for (let i = 0; i < name.length; i++) {
    h ^= name.charCodeAt(i);
    h = Math.imul(h, 0x01000193) >>> 0;
  }
  return h;
}

export async function grant(env: Env, req: GrantReq): Promise<Lease | null> {
  if (req.kind === "diag") return grantDiag(env, req);
  if (req.kind === "fuzz") return grantFuzz(env, req);
  return grantRunGames(env, req);
}

function insertLease(db: D1Database, l: Lease, row: Partial<LeaseRow> & { scope: string }): Stmt {
  return db
    .prepare(
      `INSERT INTO leases (lease_id, machine_id, scope, run, campaign, kind, pool, lo, hi, gen, build_id, diag_id,
         lease, state, beat_s, slack_s, issued_at, expires_at)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'active', ?, ?, ?, ?)`,
    )
    .bind(
      l.lease_id,
      row.machine_id,
      row.scope,
      l.run,
      l.campaign,
      l.kind,
      row.pool ?? null,
      row.lo ?? null,
      row.hi ?? null,
      l.checkpoint?.gen ?? null,
      l.build.build_id,
      row.diag_id ?? null,
      JSON.stringify(l),
      l.beat_s,
      l.slack_s,
      l.issued_at,
      l.expires_at,
    );
}

function baseLease(now: number, kind: Kind): Pick<Lease, "lease_id" | "kind" | "issued_at" | "expires_at" | "beat_s" | "slack_s"> {
  return {
    lease_id: newLeaseId(),
    kind,
    issued_at: now,
    expires_at: now + EXPIRE_BEATS * BEAT_S,
    beat_s: BEAT_S,
    slack_s: SLACK_S,
  };
}

async function grantRunGames(env: Env, req: GrantReq): Promise<Lease | null> {
  const db = env.DB;
  const scope = runScope(req.run!);
  await sweep(env, scope, req.now);
  const run = await getRun(db, req.run!);
  if (!run || run.state !== "active") return null;
  const stmts: Stmt[] = [];
  let range: Range;
  let pool: string;
  let gen: number | null;
  let job: Record<string, Json>;

  if (req.kind === "selfplay") {
    const sp = parseJson<SelfplayCfg | null>(run.selfplay, null);
    if (!sp || run.current_gen === null) return null;
    const st = await selfplayState(db, run, sp);
    const n = Math.min(req.want, st.headroom);
    if (n < 1) return null;
    pool = "selfplay";
    const took = await poolTake(db, scope, pool, n);
    if (took) {
      range = took.range;
      stmts.push(...took.stmts);
    } else {
      range = [run.next_gid, run.next_gid + n];
      stmts.push(db.prepare(`UPDATE runs SET next_gid = ? WHERE run = ? AND next_gid = ?`).bind(range[1], run.run, run.next_gid));
    }
    gen = run.current_gen;
    job = { ...sp.job, kind: "selfplay", games: range[1] - range[0], gid_base: range[0], seed: baseSeed(run.run, sp.job) };
  } else {
    const ev = parseJson<EvalCfg | null>(run.eval, null);
    if (!ev || run.eval_batch !== ev.batch) return null;
    pool = `eval:${ev.batch}`;
    let n = req.want;
    if (ev.job.paired === true) n = Math.max(2, n - (n % 2));
    const took = await poolTake(db, scope, pool, n);
    if (!took) return null;
    range = took.range;
    stmts.push(...took.stmts);
    gen = ev.gen ?? run.current_gen;
    job = { ...ev.job, kind: "eval", games: range[1] - range[0], gid_base: range[0], seed: baseSeed(run.run, ev.job) };
  }

  const ckpt = await getCheckpoint(db, run.run, gen);
  if (req.kind === "selfplay" && !ckpt) return null;
  const lease: Lease = {
    ...baseLease(req.now, req.kind),
    run: run.run,
    campaign: null,
    build: buildRef(run.build_id),
    runtime: run.runtime ? buildRef(run.runtime) : null,
    checkpoint: ckpt ? checkpointOf(ckpt) : null,
    job,
  };
  stmts.push(insertLease(db, lease, { scope, machine_id: req.machine_id, pool, lo: range[0], hi: range[1] }));
  await db.batch(stmts);
  return lease;
}

async function grantFuzz(env: Env, req: GrantReq): Promise<Lease | null> {
  const db = env.DB;
  const scope = campaignScope(req.campaign!);
  await sweep(env, scope, req.now);
  const c = await getCampaign(db, req.campaign!);
  if (!c || c.state !== "active") return null;
  const n = Math.max(1, Math.min(req.want, MAX_LEASE_IDS));
  const stmts: Stmt[] = [];
  let range: Range;
  const took = await poolTake(db, scope, "fuzz", n);
  if (took) {
    range = took.range;
    stmts.push(...took.stmts);
  } else {
    // The cursor only ever moves forward.
    range = [c.cursor, c.cursor + n];
    stmts.push(db.prepare(`UPDATE campaigns SET cursor = ? WHERE campaign = ? AND cursor = ?`).bind(range[1], c.campaign, c.cursor));
  }
  const lease: Lease = {
    ...baseLease(req.now, "fuzz"),
    run: null,
    campaign: c.campaign,
    build: buildRef(c.build_id),
    runtime: null,
    checkpoint: null,
    job: { kind: "fuzz", mode: c.mode, env: parseJson<Json>(c.env, {}), first: range[0], count: range[1] - range[0] },
  };
  stmts.push(insertLease(db, lease, { scope, machine_id: req.machine_id, pool: "fuzz", lo: range[0], hi: range[1] }));
  await db.batch(stmts);
  return lease;
}

async function grantDiag(env: Env, req: GrantReq): Promise<Lease | null> {
  const db = env.DB;
  await sweep(env, DIAG_SCOPE, req.now);
  const d = await db
    .prepare(
      `SELECT d.* FROM diag_requests d WHERE d.id = ? AND d.machine_id = ? AND d.done_at IS NULL AND d.kind != 'logs'
       AND NOT EXISTS (SELECT 1 FROM leases l WHERE l.lease_id = d.lease_id AND l.state = 'active')`,
    )
    .bind(req.diag_id!, req.machine_id)
    .first<{ id: number; kind: string; params: string | null }>();
  if (!d) return null;
  const params = parseJson<Record<string, Json>>(d.params, {});
  // Which worker build runs it: params.build_id, else params.run's, else the busiest run's,
  // else an active campaign's.
  let buildId: string | null = typeof params.build_id === "string" ? params.build_id : null;
  let run: RunRow | null = typeof params.run === "string" ? await getRun(db, params.run) : null;
  if (!buildId && !run) {
    run = await db.prepare(`SELECT * FROM runs WHERE state = 'active' ORDER BY priority DESC, run LIMIT 1`).first<RunRow>();
  }
  if (!buildId && run) buildId = run.build_id;
  if (!buildId) {
    const c = await db.prepare(`SELECT build_id FROM campaigns WHERE state = 'active' ORDER BY priority DESC LIMIT 1`).first<{
      build_id: string;
    }>();
    buildId = c?.build_id ?? null;
  }
  if (!buildId) return null;
  const ckpt = run ? await getCheckpoint(db, run.run, run.current_gen) : null;
  const lease: Lease = {
    ...baseLease(req.now, "diag"),
    run: run?.run ?? null,
    campaign: null,
    build: buildRef(buildId),
    runtime: run?.runtime ? buildRef(run.runtime) : null,
    checkpoint: ckpt ? checkpointOf(ckpt) : null,
    job: { kind: "diag", what: d.kind, params },
  };
  await db.batch([
    insertLease(db, lease, { scope: DIAG_SCOPE, machine_id: req.machine_id, diag_id: d.id }),
    db.prepare(`UPDATE diag_requests SET delivered_at = ?, lease_id = ? WHERE id = ?`).bind(req.now, lease.lease_id, d.id),
  ]);
  return lease;
}

// ---------- beats ----------

export interface BeatReq {
  now: number;
  machine_id: string;
  lease_id: string;
  scope: string;
  state: "running" | "paused";
  progress: Json | undefined;
  stats: Json | undefined;
}

export interface BeatAnswer {
  action: "continue" | "cancel" | "give_back";
  expires_at: number;
  checkpoint: ReturnType<typeof checkpointOf> | null;
}

async function ownLease(db: D1Database, leaseId: string, machineId: string): Promise<LeaseRow> {
  const l = await getLease(db, leaseId);
  if (!l || l.machine_id !== machineId) throw notFound("no such lease");
  return l;
}

export async function beat(env: Env, r: BeatReq): Promise<BeatAnswer> {
  const db = env.DB;
  await sweep(env, r.scope, r.now);
  const l = await ownLease(db, r.lease_id, r.machine_id);
  if (l.state === "expired") throw gone("lease expired");
  if (l.state === "cancelled") throw gone("lease was cancelled");
  if (l.state !== "active") throw gone("lease is closed");

  let pausedS = l.paused_s;
  if (l.beat_state === "paused" && l.last_beat !== null) pausedS += Math.max(0, r.now - l.last_beat);
  const paused = r.state === "paused";
  const pausedSince = paused ? (l.beat_state === "paused" ? (l.paused_since ?? r.now) : r.now) : null;
  const progress = r.progress === undefined ? l.progress : JSON.stringify(r.progress);
  const stats = r.stats === undefined ? l.stats : JSON.stringify(r.stats);

  let action: BeatAnswer["action"] = "continue";
  if (l.pending === "cancel") action = "cancel";
  else if (l.pending === "give_back") action = "give_back";
  else if (pausedSince !== null && r.now - pausedSince > l.slack_s) action = "give_back"; // the agent should have

  const upd = db
    .prepare(
      `UPDATE leases SET last_beat = ?, beat_state = ?, paused_since = ?, paused_s = ?, progress = ?, stats = ?,
         expires_at = ?, pending = ? WHERE lease_id = ?`,
    )
    .bind(
      r.now,
      r.state,
      pausedSince,
      pausedS,
      progress,
      stats,
      action === "cancel" ? l.expires_at : r.now + EXPIRE_BEATS * l.beat_s,
      action === "give_back" ? "give_back" : l.pending,
      l.lease_id,
    );
  if (action === "cancel") {
    await db.batch([upd]);
    const fresh = (await getLease(db, l.lease_id))!;
    await closeLease(env, fresh, "cancelled", "cancel", r.now);
    return { action, expires_at: r.now, checkpoint: null };
  }
  await db.batch([upd]);

  // A newer checkpoint reaches self-play leases here, on every beat until the lease ends (the
  // agent switches at the next game boundary and ignores repeats).
  let checkpoint: BeatAnswer["checkpoint"] = null;
  if (l.kind === "selfplay" && l.run) {
    const run = await getRun(db, l.run);
    if (run && run.current_gen !== null && (l.gen === null || run.current_gen > l.gen)) {
      const c = await getCheckpoint(db, l.run, run.current_gen);
      if (c) checkpoint = checkpointOf(c);
    }
  }
  return { action, expires_at: r.now + EXPIRE_BEATS * l.beat_s, checkpoint };
}

// ---------- results ----------

export interface ResultReq {
  now: number;
  machine_id: string;
  lease_id: string;
  seq: number;
  sha256: string;
  size: number;
  ids: Json | null; // x-ids, parsed JSON, validated here against the lease
  gen: number | null; // x-gen
  key: string; // where the Worker already stored the bytes (resultKey)
  fuzz: FuzzResult | null; // a fuzz result's parsed failures
}

export interface ResultAnswer {
  ok: true;
  duplicate: boolean; // this exact upload was already stored
  credited: number; // ids credited to this chunk
  already_credited: number; // ids another chunk had credited first
}

// The Worker writes a result's bytes to R2 *before* LeaseBook commits it (bodies up to 32 MB don't
// fit through Durable Object RPC). The key carries the hash, so two different bodies for one seq
// never overwrite each other: whichever LeaseBook commits first is the result, and the Worker
// deletes the other's object (keep = false).
export interface ResultCommit extends ResultAnswer {
  keep: boolean;
}

export function resultKey(l: LeaseRow, seq: number, sha256: string, now: number): string {
  const tag = `${seq}-${sha256.slice(0, 16)}`;
  return l.scope === DIAG_SCOPE ? `diag/${l.machine_id}/${now}-${l.lease_id}-${tag}` : `results/${l.run ?? l.campaign}/${l.lease_id}/${tag}`;
}

export const MAX_RESULT = 32 * 1024 * 1024;

interface FuzzFailure {
  seed: number;
  site: string;
  panic: string | null;
  repro: string | null;
  output_tail: string | null;
}

export interface FuzzResult {
  failures: FuzzFailure[];
  mode: string | null;
  build_id: string | null;
  engine_commit: string | null;
}

const MAX_FAILURE_TEXT = 4_000_000; // chars of failure text per result (it crosses RPC)

function cut(v: unknown, max: number): string | null {
  return typeof v === "string" ? v.slice(0, max) : null;
}

// A fuzz result (PROTOCOL 5.1): we only need its failures.
export function parseFuzz(bytes: Uint8Array): FuzzResult {
  let v: any;
  try {
    v = JSON.parse(new TextDecoder().decode(bytes));
  } catch {
    throw bad("a fuzz result must be JSON (PROTOCOL 5.1)");
  }
  if (!v || typeof v !== "object" || !Array.isArray(v.failures)) throw bad("a fuzz result needs a failures list");
  if (v.failures.length > 10000) throw bad("too many failures in one result");
  const failures: FuzzFailure[] = v.failures.map((f: any, i: number) => {
    if (!f || typeof f !== "object" || !Number.isSafeInteger(f.seed) || typeof f.site !== "string") {
      throw bad(`failures[${i}] needs an integer seed and a string site`);
    }
    return {
      seed: f.seed,
      site: f.site.slice(0, 512),
      panic: cut(f.panic, 4096),
      repro: cut(f.repro, 8192),
      output_tail: cut(f.output_tail, 16384),
    };
  });
  const text = failures.reduce((n, f) => n + f.site.length + (f.panic?.length ?? 0) + (f.repro?.length ?? 0) + (f.output_tail?.length ?? 0), 0);
  if (text > MAX_FAILURE_TEXT) throw bad("too much failure text in one result");
  return { failures, mode: cut(v.mode, 64), build_id: cut(v.build_id, 64), engine_commit: cut(v.engine_commit, 64) };
}

export async function putResult(env: Env, r: ResultReq): Promise<ResultCommit> {
  const db = env.DB;
  let l = await ownLease(db, r.lease_id, r.machine_id);
  await sweep(env, l.scope, r.now); // an expired lease must count as expired here
  l = (await getLease(db, r.lease_id))!;
  const existing = await db
    .prepare(`SELECT sha256, games, dup_ids, key, deleted_at FROM results WHERE lease_id = ? AND seq = ?`)
    .bind(l.lease_id, r.seq)
    .first<{ sha256: string; games: number; dup_ids: string; key: string; deleted_at: number | null }>();
  if (existing) {
    if (existing.sha256 === r.sha256) {
      return {
        ok: true,
        duplicate: true,
        credited: existing.games,
        already_credited: total(parseJson(existing.dup_ids, [])),
        keep: existing.key === r.key && existing.deleted_at === null,
      };
    }
    throw conflict(`result ${r.seq} of this lease was already uploaded with a different sha256`);
  }
  // A late chunk (the lease expired, was given back or finished) is still taken: the trainer reads
  // results by cursor, so it sees it whatever it acked before. Only a cancelled lease's chunks
  // and self-play chunks older than max_stale generations are refused.
  if (l.state === "cancelled") throw gone("lease was cancelled");

  // Which ids this chunk completes.
  let ids: Range[] = [];
  if (r.ids !== null) {
    if (l.pool === null) throw bad("x-ids: a diag lease has no ids");
    ids = parseRanges(r.ids, "x-ids", [l.lo!, l.hi!]);
  }

  let gen: number | null = l.gen;
  if (l.kind === "selfplay" && l.run) {
    const run = await getRun(db, l.run);
    const sp = parseJson<SelfplayCfg | null>(run?.selfplay ?? null, null);
    if (r.gen !== null) {
      if (l.gen !== null && r.gen < l.gen) throw bad(`x-gen ${r.gen} is older than the lease's checkpoint ${l.gen}`);
      if (run?.current_gen !== null && run?.current_gen !== undefined && r.gen > run.current_gen) {
        throw bad(`x-gen ${r.gen} is newer than the run's current checkpoint ${run.current_gen}`);
      }
      gen = r.gen;
    }
    if (run && sp && run.current_gen !== null && gen !== null && gen < run.current_gen - sp.max_stale) {
      throw gone(`stale: played with checkpoint ${gen}, current is ${run.current_gen}, max_stale ${sp.max_stale}`);
    }
  }

  const fuzz = r.fuzz;
  if (l.kind === "fuzz" && !fuzz) throw bad("a fuzz result must be JSON (PROTOCOL 5.1)");

  const claimed = await claimedIn(db, l.scope, ids);
  const credited = subtract(ids, claimed);
  const dup = intersect(ids, claimed);

  const key = r.key;
  const rid = `(SELECT id FROM results WHERE lease_id = ? AND seq = ?)`; // bound after the row's own values
  const stmts: Stmt[] = [
    db
      .prepare(
        `INSERT INTO results (scope, lease_id, seq, machine_id, kind, gen, key, sha256, size, games, ids, dup_ids, t)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
      )
      .bind(
        l.scope,
        l.lease_id,
        r.seq,
        l.machine_id,
        l.kind,
        gen,
        key,
        r.sha256,
        r.size,
        total(credited),
        JSON.stringify(credited),
        JSON.stringify(dup),
        r.now,
      ),
    ...credited.map(([a, b]) =>
      db
        .prepare(`INSERT INTO claims (scope, lo, hi, lease_id, result_id) VALUES (?, ?, ?, ?, ${rid})`)
        .bind(l.scope, a, b, l.lease_id, l.lease_id, r.seq),
    ),
    db.prepare(`UPDATE leases SET units_done = units_done + ? WHERE lease_id = ?`).bind(total(ids), l.lease_id),
  ];

  if (credited.length) {
    // Active leases holding any of these ids (this one, or one that was given ids this lease had
    // let go of): their unclaimed count drops.
    const lo = credited[0][0];
    const hi = credited[credited.length - 1][1];
    const { results } = await db
      .prepare(`SELECT lease_id, lo, hi FROM leases WHERE scope = ? AND state = 'active' AND pool IS NOT NULL AND lo < ? AND hi > ?`)
      .bind(l.scope, hi, lo)
      .all<{ lease_id: string; lo: number; hi: number }>();
    for (const o of results) {
      const n = total(intersect(credited, [[o.lo, o.hi]]));
      if (n) stmts.push(db.prepare(`UPDATE leases SET claimed = claimed + ? WHERE lease_id = ?`).bind(n, o.lease_id));
    }
    // A late result can claim ids that already went back to the pool.
    if (l.state !== "active" && l.pool) stmts.push(...(await poolRemove(db, l.scope, l.pool, credited)));
  }

  if (fuzz && fuzz.failures.length && l.campaign) {
    const sites = new Set(
      (
        await db
          .prepare(`SELECT DISTINCT site FROM failures WHERE campaign = ? AND site IN (${fuzz.failures.map(() => "?").join(",")})`)
          .bind(l.campaign, ...fuzz.failures.map((f) => f.site))
          .all<{ site: string }>()
      ).results.map((x) => x.site),
    );
    for (const f of fuzz.failures) {
      const first = sites.has(f.site) ? 0 : 1;
      sites.add(f.site);
      stmts.push(
        db
          .prepare(
            `INSERT OR IGNORE INTO failures (campaign, seed, mode, site, panic, repro, output_tail, build_id, engine_commit,
               machine_id, lease_id, result_id, first_of_site, t)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ${rid}, ?, ?)`,
          )
          .bind(
            l.campaign,
            f.seed,
            fuzz.mode,
            f.site,
            f.panic,
            f.repro,
            f.output_tail,
            fuzz.build_id ?? l.build_id,
            fuzz.engine_commit,
            l.machine_id,
            l.lease_id,
            l.lease_id,
            r.seq,
            first,
            r.now,
          ),
      );
    }
  }
  await db.batch(stmts);
  return { ok: true, duplicate: false, credited: total(credited), already_credited: total(dup), keep: true };
}

// ---------- done ----------

export interface DoneReq {
  now: number;
  machine_id: string;
  lease_id: string;
  scope: string;
  results: number[];
  stats: Json | undefined;
  given_back: boolean;
  unfinished: Record<string, Json> | null; // {"gids": [...]} or {"seeds": [...]}; checked, but the coordinator
  // works out what is unfinished itself (the lease's ids minus everything claimed)
}

export interface DoneAnswer {
  ok: true;
  returned: number; // ids that went back to the pool
}

export async function done(env: Env, r: DoneReq): Promise<DoneAnswer> {
  const db = env.DB;
  await sweep(env, r.scope, r.now);
  const l = await ownLease(db, r.lease_id, r.machine_id);
  if (l.state === "cancelled") throw gone("lease was cancelled");
  if (l.state === "done" || l.state === "given_back") return { ok: true, returned: 0 };

  const { results } = await db.prepare(`SELECT seq FROM results WHERE lease_id = ?`).bind(l.lease_id).all<{ seq: number }>();
  const have = new Set(results.map((x) => x.seq));
  const missing = [...new Set(r.results)].filter((s) => !have.has(s)).sort((a, b) => a - b);
  if (missing.length) throw conflict("some results were never received: upload them, then call done again", { missing });

  if (r.unfinished !== null && l.pool !== null) {
    const field = l.kind === "fuzz" ? "seeds" : "gids";
    const u = r.unfinished;
    if (u[field] !== undefined) parseRanges(u[field], `unfinished.${field}`, [l.lo!, l.hi!]);
  }
  const stats = r.stats === undefined ? null : JSON.stringify(r.stats);

  if (l.state === "expired") {
    // Its ids already went back to the pool when it expired; keep the stats.
    if (stats) await db.prepare(`UPDATE leases SET stats = ? WHERE lease_id = ?`).bind(stats, l.lease_id).run();
    return { ok: true, returned: 0 };
  }
  const extra = stats ? [db.prepare(`UPDATE leases SET stats = ? WHERE lease_id = ?`).bind(stats, l.lease_id)] : [];
  // The final Stats (e.g. starved_ms) feed the rate.
  const final = stats ? { ...l, stats } : l;
  const back = await closeLease(env, final, r.given_back ? "given_back" : "done", r.given_back ? "given_back" : "done", r.now, extra);
  return { ok: true, returned: total(back) };
}

// ---------- desktop calls ----------

export interface RunCfg {
  run: string;
  game: string;
  build_id: string;
  runtime: string | null;
  priority: number;
  state: "active" | "paused";
  selfplay: SelfplayCfg | null;
  eval: EvalCfg | null;
}

export async function putRun(env: Env, cfg: RunCfg, now: number): Promise<{ ok: true; created: boolean }> {
  const db = env.DB;
  const scope = runScope(cfg.run);
  const old = await getRun(db, cfg.run);
  const stmts: Stmt[] = [];
  if (!old) {
    stmts.push(
      db
        .prepare(
          `INSERT INTO runs (run, game, build_id, runtime, priority, state, selfplay, eval, next_gid, created_at, updated_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?)`,
        )
        .bind(cfg.run, cfg.game, cfg.build_id, cfg.runtime, cfg.priority, cfg.state, JSON.stringify(cfg.selfplay), JSON.stringify(cfg.eval), now, now),
    );
  } else {
    stmts.push(
      db
        .prepare(
          `UPDATE runs SET game = ?, build_id = ?, runtime = ?, priority = ?, state = ?, selfplay = ?, eval = ?, updated_at = ? WHERE run = ?`,
        )
        .bind(cfg.game, cfg.build_id, cfg.runtime, cfg.priority, cfg.state, JSON.stringify(cfg.selfplay), JSON.stringify(cfg.eval), now, cfg.run),
    );
  }
  // A new eval batch reserves its game ids now; the old batch's unplayed ids are dropped.
  const oldBatch = old?.eval_batch ?? null;
  const newBatch = cfg.eval?.batch ?? null;
  if (newBatch !== oldBatch) {
    stmts.push(db.prepare(`DELETE FROM pool WHERE scope = ? AND pool LIKE 'eval:%'`).bind(scope));
    if (cfg.eval) {
      const lo = old?.next_gid ?? 0;
      stmts.push(
        db.prepare(`INSERT INTO pool (scope, pool, lo, hi) VALUES (?, ?, ?, ?)`).bind(scope, `eval:${cfg.eval.batch}`, lo, lo + cfg.eval.games),
        db.prepare(`UPDATE runs SET next_gid = ?, eval_batch = ? WHERE run = ?`).bind(lo + cfg.eval.games, cfg.eval.batch, cfg.run),
      );
    } else {
      stmts.push(db.prepare(`UPDATE runs SET eval_batch = NULL WHERE run = ?`).bind(cfg.run));
    }
  }
  await db.batch(stmts);
  if (cfg.state === "paused") await cancelScope(env, scope, null);
  else {
    if (!cfg.selfplay) await cancelScope(env, scope, "selfplay");
    if (!cfg.eval) await cancelScope(env, scope, "eval");
  }
  return { ok: true, created: !old };
}

export interface CampaignCfg {
  campaign: string;
  build_id: string;
  mode: string;
  env: Record<string, string>;
  seed_base: number;
  chunk: "auto" | number;
  priority: number;
  state: "active" | "paused";
  needs: Needs | null;
}

export async function putCampaign(env: Env, cfg: CampaignCfg, now: number): Promise<{ ok: true; created: boolean; cursor: number }> {
  const db = env.DB;
  const old = await getCampaign(db, cfg.campaign);
  // The cursor never rewinds: a lower seed_base leaves it where it is, a higher one moves it up.
  const cursor = old ? Math.max(old.cursor, cfg.seed_base) : cfg.seed_base;
  await db
    .prepare(
      `INSERT INTO campaigns (campaign, build_id, mode, env, seed_base, cursor, chunk, priority, state, needs, created_at, updated_at)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
       ON CONFLICT (campaign) DO UPDATE SET build_id = excluded.build_id, mode = excluded.mode, env = excluded.env,
         seed_base = excluded.seed_base, cursor = MAX(campaigns.cursor, excluded.cursor), chunk = excluded.chunk,
         priority = excluded.priority, state = excluded.state, needs = excluded.needs, updated_at = excluded.updated_at`,
    )
    .bind(
      cfg.campaign,
      cfg.build_id,
      cfg.mode,
      JSON.stringify(cfg.env),
      cfg.seed_base,
      cursor,
      String(cfg.chunk),
      cfg.priority,
      cfg.state,
      cfg.needs ? JSON.stringify(cfg.needs) : null,
      now,
      now,
    )
    .run();
  if (cfg.state === "paused") await cancelScope(env, campaignScope(cfg.campaign), null);
  return { ok: true, created: !old, cursor };
}

export interface CheckpointReq {
  run: string;
  gen: number;
  model: string;
  sha256: string;
  reference: string | null;
  reference_sha256: string | null;
  size: number | null;
  now: number;
}

export async function publishCheckpoint(env: Env, c: CheckpointReq): Promise<{ ok: true; unchanged: boolean }> {
  const db = env.DB;
  const run = await getRun(db, c.run);
  if (!run) throw notFound(`no run ${c.run}`);
  if (run.current_gen !== null && c.gen < run.current_gen) {
    throw conflict(`gen ${c.gen} is older than the current checkpoint ${run.current_gen}`);
  }
  const old = await getCheckpoint(db, c.run, c.gen);
  if (old) {
    if (old.sha256 === c.sha256 && old.reference_sha256 === c.reference_sha256 && old.model === c.model && old.reference === c.reference) {
      return { ok: true, unchanged: true };
    }
    throw conflict(`checkpoint ${c.gen} of ${c.run} was already published with different files`);
  }
  await db.batch([
    db
      .prepare(`INSERT INTO checkpoints (run, gen, model, sha256, reference, reference_sha256, size, t) VALUES (?, ?, ?, ?, ?, ?, ?, ?)`)
      .bind(c.run, c.gen, c.model, c.sha256, c.reference, c.reference_sha256, c.size, c.now),
    db.prepare(`UPDATE runs SET current_gen = ?, updated_at = ? WHERE run = ?`).bind(c.gen, c.now, c.run),
    // A new generation starts its quota count from zero.
    db.prepare(`INSERT OR REPLACE INTO quotas (run, gen, accepted, t) VALUES (?, ?, 0, ?)`).bind(c.run, c.gen, c.now),
  ]);
  return { ok: true, unchanged: false };
}

export async function accept(env: Env, run: string, gen: number, games: number, now: number): Promise<SelfplayState | null> {
  const db = env.DB;
  const r = await getRun(db, run);
  if (!r) throw notFound(`no run ${run}`);
  await db
    .prepare(
      `INSERT INTO quotas (run, gen, accepted, t) VALUES (?, ?, ?, ?)
       ON CONFLICT (run, gen) DO UPDATE SET accepted = excluded.accepted, t = excluded.t`,
    )
    .bind(run, gen, games, now)
    .run();
  const sp = parseJson<SelfplayCfg | null>(r.selfplay, null);
  return sp ? selfplayState(db, r, sp) : null;
}

export async function ack(env: Env, scope: string, cursor: number, now: number): Promise<{ ok: true; acked: number }> {
  const res = await env.DB.prepare(`UPDATE results SET acked_at = ? WHERE scope = ? AND id <= ? AND acked_at IS NULL`)
    .bind(now, scope, cursor)
    .run();
  return { ok: true, acked: res.meta.changes ?? 0 };
}
