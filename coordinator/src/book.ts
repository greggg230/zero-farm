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
//
// Paired eval batches (PROTOCOL 5.2) claim whole pairs (2k, 2k+1) only, so their claims, pool
// ranges and leases always start and end on even ids. A game whose pair-mate its lease hasn't
// reported yet is "held" (not claimed) until a later chunk of the same lease brings the mate.

import {
  type Def,
  type DefinitionRow,
  type JobKind,
  baseSeed,
  checkpointProblem,
  currentDef,
  defAt,
  defRef,
  evalProblem,
  leaseProblem,
} from "./definitions";
import { intersect, normalize, parseRanges, subtract, total, type Range } from "./ranges";
import type { GpuMode, Provider } from "./routing";
import {
  HttpError,
  type Json,
  bad,
  canonical,
  conflict,
  gone,
  newLeaseId,
  notFound,
  parseJson,
} from "./util";

export { baseSeed } from "./definitions";

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
  gpu: GpuMode;
  providers?: Provider[] | null; // rev 7: allowlist (null or absent: any provider)
  needs: Needs;
  job: Record<string, Json>;
}
export interface EvalCfg {
  batch: string;
  games: number;
  checkpoints: number[]; // rev 5: every network the seats name (rev 2's single `gen` is refused)
  games_per_lease: "auto" | number;
  gpu: GpuMode;
  providers?: Provider[] | null; // rev 7: allowlist (null or absent: any provider)
  needs: Needs;
  job: Record<string, Json>; // {settings, seats, paired}
}

export const isPaired = (ev: EvalCfg) => ev.job.paired === true;

// An eval batch's identity: what can't change once the batch exists.
export const evalSpec = (ev: EvalCfg) => canonical({ games: ev.games, checkpoints: ev.checkpoints, job: ev.job });

export interface EvalBatchRow {
  run: string;
  batch: string;
  lo: number;
  hi: number;
  games: number;
  paired: number;
  checkpoints: string;
  spec: string;
  created_at: number;
  replaced_at: number | null;
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
  min_agent: string | null; // rev 9: the oldest agent its leases go to (NULL: no such limit)
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
  gpu: string; // rev 7: any | preferred | required
  providers: string | null; // rev 7: JSON allowlist or NULL (any provider)
  min_agent: string | null; // rev 9: the oldest agent its leases go to (NULL: no such limit)
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
  def_version: number | null; // the definition it was issued with (null: legacy)
}

export interface CheckpointRow {
  run: string;
  gen: number;
  model: string;
  sha256: string;
  reference: string | null;
  reference_sha256: string | null;
  size: number | null;
  reference_size: number | null;
}

export const ckptFacts = (c: CheckpointRow) => ({
  gen: c.gen,
  sha256: c.sha256,
  size: c.size,
  reference_sha256: c.reference_sha256,
  reference_size: c.reference_size ?? null,
});

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
export async function getBatch(db: D1Database, run: string, batch: string): Promise<EvalBatchRow | null> {
  return db.prepare(`SELECT * FROM eval_batches WHERE run = ? AND batch = ?`).bind(run, batch).first<EvalBatchRow>();
}
// The checkpoints `gens` of a run, in that order; null if any is missing.
async function checkpointsOf(db: D1Database, run: string, gens: number[]): Promise<CheckpointRow[] | null> {
  if (!gens.length) return [];
  const { results } = await db
    .prepare(`SELECT * FROM checkpoints WHERE run = ? AND gen IN (${gens.map(() => "?").join(",")})`)
    .bind(run, ...gens)
    .all<CheckpointRow>();
  const by = new Map(results.map((c) => [c.gen, c]));
  const out = gens.map((g) => by.get(g));
  return out.every((c) => c) ? (out as CheckpointRow[]) : null;
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

// ---------- eval progress ----------

export interface EvalProgress {
  batch: string;
  games: number;
  paired: boolean;
  checkpoints: number[];
  lo: number; // game ids [lo, hi)
  hi: number;
  done: number; // credited games (whole pairs for a paired batch)
  leased: number; // ids in active leases not credited yet
  unissued: number; // ids in the pool (0 once the batch was replaced: they were dropped)
  created_at: number;
  replaced_at: number | null;
}

export async function evalProgress(db: D1Database, b: EvalBatchRow): Promise<EvalProgress> {
  const scope = runScope(b.run);
  const pool = `eval:${b.batch}`;
  const [done, leased, unissued] = await db.batch<{ n: number }>([
    db.prepare(`SELECT COALESCE(SUM(hi - lo), 0) AS n FROM claims WHERE scope = ? AND lo >= ? AND hi <= ?`).bind(scope, b.lo, b.hi),
    db.prepare(`SELECT COALESCE(SUM(hi - lo - claimed), 0) AS n FROM leases WHERE scope = ? AND pool = ? AND state = 'active'`).bind(scope, pool),
    db.prepare(`SELECT COALESCE(SUM(hi - lo), 0) AS n FROM pool WHERE scope = ? AND pool = ?`).bind(scope, pool),
  ]);
  return {
    batch: b.batch,
    games: b.games,
    paired: !!b.paired,
    checkpoints: parseJson<number[]>(b.checkpoints, []),
    lo: b.lo,
    hi: b.hi,
    done: done.results[0]?.n ?? 0,
    leased: leased.results[0]?.n ?? 0,
    unissued: unissued.results[0]?.n ?? 0,
    created_at: b.created_at,
    replaced_at: b.replaced_at,
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
  // The agent verifies definitions (it is DEFINITION_AGENT or newer): it gets leases only with a
  // definition; an older agent only legacy leases, for a scope without one (PROTOCOL 7).
  def_aware: boolean;
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
  checkpoints: ReturnType<typeof checkpointOf>[] | null; // eval only (rev 5)
  // Rev 6: the signed definition this lease is issued under (PROTOCOL 7); absent on legacy leases.
  // Then `job` holds only the per-lease fields (and `kind`): the agent builds the job line from
  // the definition's template.
  definition?: { key: string; sha256: string };
  job: Record<string, Json>;
  // Rev 7 (PROTOCOL 8): the job's provider allowlist, a routing hint that is not part of the signed
  // job (null: any). friend.ts adds it to the answer; the stored lease JSON doesn't carry it.
  providers?: Provider[] | null;
}

const buildRef = (id: string) => ({ build_id: id, manifest: `builds/${id}/manifest.json` });

export async function grant(env: Env, req: GrantReq): Promise<Lease | null> {
  if (req.kind === "diag") return grantDiag(env, req);
  if (req.kind === "fuzz") return grantFuzz(env, req);
  return grantRunGames(env, req);
}

function insertLease(db: D1Database, l: Lease, row: Partial<LeaseRow> & { scope: string }): Stmt {
  return db
    .prepare(
      `INSERT INTO leases (lease_id, machine_id, scope, run, campaign, kind, pool, lo, hi, gen, build_id, diag_id,
         lease, state, beat_s, slack_s, issued_at, expires_at, def_version)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'active', ?, ?, ?, ?, ?)`,
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
      row.def_version ?? null,
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

// Rev 6 (PROTOCOL 7): the definition a lease of this scope is issued under, `undefined` for a
// legacy lease, or null when this agent gets no lease here: a definition-aware agent needs one, an
// older agent can't have one.
async function grantDef(
  db: D1Database,
  req: GrantReq,
  kind: "run" | "campaign",
  id: string,
  jobKind: JobKind,
  build: string,
  runtime: string | null,
): Promise<Def | undefined | null> {
  const d = await currentDef(db, kind, id);
  if (!d) return req.def_aware ? null : undefined;
  if (!req.def_aware) return null;
  return leaseProblem(d, jobKind, build, runtime, req.now) === null ? d : null;
}

async function grantRunGames(env: Env, req: GrantReq): Promise<Lease | null> {
  const db = env.DB;
  const scope = runScope(req.run!);
  await sweep(env, scope, req.now);
  const run = await getRun(db, req.run!);
  if (!run || run.state !== "active") return null;
  const def = await grantDef(db, req, "run", run.run, req.kind as JobKind, run.build_id, run.runtime);
  if (def === null) return null;
  const stmts: Stmt[] = [];
  let range: Range;
  let pool: string;
  let gen: number | null = null;
  let job: Record<string, Json>;
  let evalCkpts: CheckpointRow[] | null = null;

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
    // The seed is the run's (its job's `seed`, else from its name) with or without a definition.
    const per = { kind: "selfplay", games: range[1] - range[0], gid_base: range[0], seed: baseSeed(run.run, sp.job) };
    job = def ? per : { ...sp.job, ...per };
  } else {
    // Rev 5: an eval lease carries every network its seats name (`checkpoints`), never one
    // `checkpoint`; a paired batch is leased in whole pairs.
    const ev = parseJson<EvalCfg | null>(run.eval, null);
    if (!ev || !Array.isArray(ev.checkpoints) || run.eval_batch !== ev.batch) return null;
    if (!(await getBatch(db, run.run, ev.batch))) return null; // put before rev 5: never offered
    evalCkpts = await checkpointsOf(db, run.run, ev.checkpoints);
    if (!evalCkpts) return null;
    // Rev 6: the batch's networks are the ones the definition seats, each signed as registered.
    if (def && (evalProblem(def, { checkpoints: ev.checkpoints, paired: isPaired(ev) }) || evalCkpts.some((c) => checkpointProblem(def, ckptFacts(c))))) {
      return null;
    }
    pool = `eval:${ev.batch}`;
    let n = req.want;
    if (isPaired(ev)) n = Math.max(2, n - (n % 2));
    const took = await poolTake(db, scope, pool, n);
    if (!took) return null;
    range = took.range;
    if (isPaired(ev) && (range[0] % 2 !== 0 || range[1] % 2 !== 0)) {
      throw new Error(`eval pool ${scope}/${pool} holds a broken pair: ${JSON.stringify(range)}`);
    }
    stmts.push(...took.stmts);
    const per = { kind: "eval", games: range[1] - range[0], gid_base: range[0], seed: baseSeed(run.run, ev.job) };
    job = def ? per : { ...ev.job, ...per };
  }

  const ckpt = await getCheckpoint(db, run.run, gen);
  if (req.kind === "selfplay" && !ckpt) return null;
  // Rev 6: self-play plays the current checkpoint only once the definition signs it.
  if (def && ckpt && checkpointProblem(def, ckptFacts(ckpt))) return null;
  const lease: Lease = {
    ...baseLease(req.now, req.kind),
    run: run.run,
    campaign: null,
    build: buildRef(run.build_id),
    runtime: run.runtime ? buildRef(run.runtime) : null,
    checkpoint: ckpt ? checkpointOf(ckpt) : null,
    checkpoints: evalCkpts ? evalCkpts.map(checkpointOf) : null,
    ...(def ? { definition: defRef(def) } : {}),
    job,
  };
  stmts.push(insertLease(db, lease, { scope, machine_id: req.machine_id, pool, lo: range[0], hi: range[1], def_version: def?.version ?? null }));
  await db.batch(stmts);
  return lease;
}

async function grantFuzz(env: Env, req: GrantReq): Promise<Lease | null> {
  const db = env.DB;
  const scope = campaignScope(req.campaign!);
  await sweep(env, scope, req.now);
  const c = await getCampaign(db, req.campaign!);
  if (!c || c.state !== "active") return null;
  const def = await grantDef(db, req, "campaign", c.campaign, "fuzz", c.build_id, null);
  if (def === null) return null;
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
    checkpoints: null,
    ...(def ? { definition: defRef(def) } : {}),
    job: def
      ? { kind: "fuzz", first: range[0], count: range[1] - range[0] }
      : { kind: "fuzz", mode: c.mode, env: parseJson<Json>(c.env, {}), first: range[0], count: range[1] - range[0] },
  };
  stmts.push(insertLease(db, lease, { scope, machine_id: req.machine_id, pool: "fuzz", lo: range[0], hi: range[1], def_version: def?.version ?? null }));
  await db.batch(stmts);
  return lease;
}

// Diag leases (sweep / profile / stability) carry admin-chosen params and no definition (PROTOCOL
// 7 covers runs and campaigns), so a definition-aware agent, which refuses any lease without one,
// never gets them. Logs requests (upload_diag) are not leases and go to every agent.
async function grantDiag(env: Env, req: GrantReq): Promise<Lease | null> {
  if (req.def_aware) return null;
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
    checkpoints: null,
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
  // Rev 6: a definition lease switches only to a gen its own definition signs (the agent checks
  // every checkpoint against the lease's definition). A new gen normally comes with a new
  // definition version, so it reaches new leases instead.
  let checkpoint: BeatAnswer["checkpoint"] = null;
  if (l.kind === "selfplay" && l.run) {
    const run = await getRun(db, l.run);
    if (run && run.current_gen !== null && (l.gen === null || run.current_gen > l.gen)) {
      const c = await getCheckpoint(db, l.run, run.current_gen);
      const def = c && l.def_version !== null ? await defAt(db, "run", l.run, l.def_version) : null;
      if (c && (l.def_version === null || (def && !checkpointProblem(def, ckptFacts(c))))) checkpoint = checkpointOf(c);
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
  limits: ResultLimits;
}

// Per lease (security review M3): at most `chunks` result chunks and `bytes` bytes, and none more
// than `late_s` after the lease closed.
export interface ResultLimits {
  chunks: number;
  bytes: number;
  late_s: number;
}

// Throws when one more chunk of `size` bytes would break the lease's limits.
export async function checkResultLimits(db: D1Database, l: LeaseRow, size: number, lim: ResultLimits, now: number): Promise<void> {
  if (l.state !== "active" && l.closed_at !== null && now - l.closed_at > lim.late_s) {
    throw gone(`lease closed ${now - l.closed_at} s ago: results are taken until ${lim.late_s} s after a lease ends`);
  }
  const r = await db
    .prepare(`SELECT COUNT(*) AS n, COALESCE(SUM(size), 0) AS bytes FROM results WHERE lease_id = ?`)
    .bind(l.lease_id)
    .first<{ n: number; bytes: number }>();
  if ((r?.n ?? 0) >= lim.chunks) {
    throw new HttpError(413, "too_large", `this lease already has ${lim.chunks} result chunks, the most a lease may have`, { limit: "chunks" });
  }
  if ((r?.bytes ?? 0) + size > lim.bytes) {
    throw new HttpError(413, "too_large", `this lease's results would exceed ${lim.bytes} bytes`, { limit: "bytes" });
  }
}

export interface ResultAnswer {
  ok: true;
  duplicate: boolean; // this exact upload was already stored
  credited: number; // ids credited to this chunk (eval: with the `also` ids it completed)
  already_credited: number; // ids another chunk had credited first
  held?: number; // eval: ids of this chunk waiting for their pair-mate
}

// Held games of earlier chunks of the same lease that a chunk completed (PROTOCOL 5.2): their
// rows are in that earlier chunk (`key`).
export interface AlsoEntry {
  seq: number;
  key: string;
  ids: Range[];
}

// How a chunk's reported ids are counted: credited with rows here, credited before elsewhere,
// held for a pair-mate, and earlier held games this chunk completed.
interface Split {
  here: Range[];
  dup: Range[];
  held: Range[];
  also: AlsoEntry[];
}

const idList = (rs: Range[]) => rs.flatMap(([a, b]) => Array.from({ length: b - a }, (_, i) => a + i));
// The whole pairs (2k, 2k+1) inside normalized ranges.
const wholePairs = (rs: Range[]): Range[] => rs.map(([a, b]) => [a + (a % 2), b - (b % 2)] as Range).filter(([a, b]) => b > a);

async function splitPlain(db: D1Database, scope: string, ids: Range[]): Promise<Split> {
  const claimed = await claimedIn(db, scope, ids);
  return { here: subtract(ids, claimed), dup: intersect(ids, claimed), held: [], also: [] };
}

// A paired eval chunk: a pair is credited only when both of its games are in, from this lease.
// Pairs whole in this chunk are credited here; a lone game whose mate an earlier chunk of this
// lease holds completes that pair (`also` names the earlier chunk); any other lone game is held.
async function splitPaired(db: D1Database, l: LeaseRow, ids: Range[]): Promise<Split> {
  const claimed = await claimedIn(db, l.scope, [[l.lo!, l.hi!]]);
  const dup = intersect(ids, claimed);
  const fresh = subtract(ids, claimed);
  const whole = wholePairs(fresh);
  const halves = idList(subtract(fresh, whole)); // at most two per range
  const here: Range[] = [...whole];
  const held: Range[] = [];
  const alsoBy = new Map<number, AlsoEntry>();
  if (halves.length) {
    const { results } = await db
      .prepare(`SELECT seq, key, held, also, deleted_at FROM results WHERE lease_id = ? ORDER BY seq`)
      .bind(l.lease_id)
      .all<{ seq: number; key: string; held: string | null; also: string | null; deleted_at: number | null }>();
    const completed = normalize(results.flatMap((r) => parseJson<AlsoEntry[]>(r.also, []).flatMap((a) => a.ids)));
    const heldBy = new Map<number, { seq: number; key: string }>();
    for (const r of results) {
      if (r.deleted_at !== null) continue; // its rows are gone
      const still = subtract(subtract(normalize(parseJson<Range[]>(r.held, [])), completed), claimed);
      for (const g of idList(still)) if (!heldBy.has(g)) heldBy.set(g, { seq: r.seq, key: r.key });
    }
    for (const g of halves) {
      const mate = g % 2 === 0 ? g + 1 : g - 1;
      const from = heldBy.get(mate);
      if (!from) {
        held.push([g, g + 1]);
        continue;
      }
      here.push([g, g + 1]);
      const e = alsoBy.get(from.seq) ?? { seq: from.seq, key: from.key, ids: [] };
      e.ids.push([mate, mate + 1]);
      alsoBy.set(from.seq, e);
    }
  }
  const also = [...alsoBy.values()].sort((a, b) => a.seq - b.seq).map((e) => ({ ...e, ids: normalize(e.ids) }));
  return { here: normalize(here), dup, held: normalize(held), also };
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
    .prepare(`SELECT sha256, games, dup_ids, held, kind, key, deleted_at FROM results WHERE lease_id = ? AND seq = ?`)
    .bind(l.lease_id, r.seq)
    .first<{ sha256: string; games: number; dup_ids: string; held: string | null; kind: string; key: string; deleted_at: number | null }>();
  if (existing) {
    if (existing.sha256 === r.sha256) {
      return {
        ok: true,
        duplicate: true,
        credited: existing.games,
        already_credited: total(parseJson(existing.dup_ids, [])),
        ...(existing.kind === "eval" ? { held: total(parseJson(existing.held, [])) } : {}),
        keep: existing.key === r.key && existing.deleted_at === null,
      };
    }
    throw conflict(`result ${r.seq} of this lease was already uploaded with a different sha256`);
  }
  // A late chunk (the lease expired, was given back or finished) is still taken: the trainer reads
  // results by cursor, so it sees it whatever it acked before. Only a cancelled lease's chunks
  // and self-play chunks older than max_stale generations are refused.
  if (l.state === "cancelled") throw gone("lease was cancelled");
  await checkResultLimits(db, l, r.size, r.limits, r.now);

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
  // A failure names a seed of this lease (review M3: no rows for seeds it never ran).
  const outside = fuzz?.failures.find((f) => l.lo === null || l.hi === null || f.seed < l.lo || f.seed >= l.hi);
  if (outside) throw bad(`failure seed ${outside.seed} is outside this lease's seeds [${l.lo}, ${l.hi})`);

  // Eval chunks belong to a batch; a paired batch credits whole pairs only (x-gen is ignored:
  // eval networks never go stale, and l.gen is null for eval leases).
  let batch: string | null = null;
  let paired = false;
  if (l.kind === "eval" && l.run && l.pool?.startsWith("eval:")) {
    batch = l.pool.slice(5);
    paired = !!(await getBatch(db, l.run, batch))?.paired;
  }
  const { here, dup, held, also } = paired ? await splitPaired(db, l, ids) : await splitPlain(db, l.scope, ids);
  const credited = normalize([...here, ...also.flatMap((a) => a.ids)]);
  const isEval = l.kind === "eval";

  const key = r.key;
  const rid = `(SELECT id FROM results WHERE lease_id = ? AND seq = ?)`; // bound after the row's own values
  const stmts: Stmt[] = [
    db
      .prepare(
        `INSERT INTO results (scope, lease_id, seq, machine_id, kind, gen, key, sha256, size, games, ids, dup_ids, t, batch, held, also)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
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
        JSON.stringify(here),
        JSON.stringify(dup),
        r.now,
        batch,
        isEval ? JSON.stringify(held) : null,
        isEval ? JSON.stringify(also) : null,
      ),
    ...credited.map(([a, b]) =>
      db
        .prepare(`INSERT INTO claims (scope, lo, hi, lease_id, result_id) VALUES (?, ?, ?, ?, ${rid})`)
        .bind(l.scope, a, b, l.lease_id, l.lease_id, r.seq),
    ),
    // Only ids credited here count toward the machine's rate (review M4: re-reporting ids can't
    // inflate it and win bigger leases).
    db.prepare(`UPDATE leases SET units_done = units_done + ? WHERE lease_id = ?`).bind(total(credited), l.lease_id),
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
  return {
    ok: true,
    duplicate: false,
    credited: total(credited),
    already_credited: total(dup),
    ...(isEval ? { held: total(held) } : {}),
    keep: true,
  };
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
  refused: string | null; // rev 6: the agent refused the lease ("definition"); closed as given back
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
  const state = r.given_back || r.refused ? "given_back" : "done";
  const back = await closeLease(env, final, state, r.refused ? `refused:${r.refused}` : state, r.now, extra);
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
  min_agent: string | null; // rev 9
  selfplay: SelfplayCfg | null;
  eval: EvalCfg | null;
}

export async function putRun(env: Env, cfg: RunCfg, now: number): Promise<{ ok: true; created: boolean }> {
  const db = env.DB;
  const scope = runScope(cfg.run);
  const old = await getRun(db, cfg.run);
  const stmts: Stmt[] = [];

  // Rev 5 eval batches (PROTOCOL 5.2): every network registered; a batch never changes and a batch
  // name is never used twice in a run.
  let existing: EvalBatchRow | null = null;
  if (cfg.eval) {
    const ev = cfg.eval;
    if (ev.checkpoints.length) {
      const have = old ? await checkpointsOf(db, cfg.run, ev.checkpoints) : null;
      if (!have) {
        const { results } = old
          ? await db.prepare(`SELECT gen FROM checkpoints WHERE run = ?`).bind(cfg.run).all<{ gen: number }>()
          : { results: [] as { gen: number }[] };
        const known = new Set(results.map((x) => x.gen));
        const missing = ev.checkpoints.filter((g) => !known.has(g));
        throw bad(
          `eval.checkpoints: ${missing.join(", ")} not registered on run ${cfg.run} (PUT /v1/admin/run/${cfg.run}/checkpoint/{gen} first;` +
            ` a new run is put with "eval": null, then its checkpoints, then the batch)`,
          { missing },
        );
      }
    }
    existing = await getBatch(db, cfg.run, ev.batch);
    if (existing && old?.eval_batch !== ev.batch) {
      throw conflict(`eval batch ${ev.batch} was used before in run ${cfg.run}: name a new batch`);
    }
    if (existing && existing.spec !== evalSpec(ev)) {
      throw conflict(`eval batch ${ev.batch} exists with other games, checkpoints or job: name a new batch`);
    }
  }

  if (!old) {
    stmts.push(
      db
        .prepare(
          `INSERT INTO runs (run, game, build_id, runtime, priority, state, selfplay, eval, next_gid, min_agent, created_at, updated_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?, ?)`,
        )
        .bind(cfg.run, cfg.game, cfg.build_id, cfg.runtime, cfg.priority, cfg.state, JSON.stringify(cfg.selfplay), JSON.stringify(cfg.eval), cfg.min_agent, now, now),
    );
  } else {
    stmts.push(
      db
        .prepare(
          `UPDATE runs SET game = ?, build_id = ?, runtime = ?, priority = ?, state = ?, selfplay = ?, eval = ?, min_agent = ?, updated_at = ? WHERE run = ?`,
        )
        .bind(cfg.game, cfg.build_id, cfg.runtime, cfg.priority, cfg.state, JSON.stringify(cfg.selfplay), JSON.stringify(cfg.eval), cfg.min_agent, now, cfg.run),
    );
  }
  // A new eval batch reserves its game ids now; the old batch's unplayed ids are dropped. A paired
  // batch starts on an even id: an odd id skipped to get there goes to the self-play pool.
  const oldBatch = old?.eval_batch ?? null;
  const replaceOld = () =>
    oldBatch === null
      ? []
      : [db.prepare(`UPDATE eval_batches SET replaced_at = ? WHERE run = ? AND batch = ? AND replaced_at IS NULL`).bind(now, cfg.run, oldBatch)];
  if (cfg.eval && !existing) {
    const ev = cfg.eval;
    let lo = old?.next_gid ?? 0;
    stmts.push(db.prepare(`DELETE FROM pool WHERE scope = ? AND pool LIKE 'eval:%'`).bind(scope), ...replaceOld());
    if (isPaired(ev) && lo % 2 !== 0) {
      stmts.push(...(await poolAdd(db, scope, "selfplay", [[lo, lo + 1]])));
      lo += 1;
    }
    stmts.push(
      db.prepare(`INSERT INTO pool (scope, pool, lo, hi) VALUES (?, ?, ?, ?)`).bind(scope, `eval:${ev.batch}`, lo, lo + ev.games),
      db.prepare(`UPDATE runs SET next_gid = ?, eval_batch = ? WHERE run = ?`).bind(lo + ev.games, ev.batch, cfg.run),
      db
        .prepare(
          `INSERT INTO eval_batches (run, batch, lo, hi, games, paired, checkpoints, spec, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)`,
        )
        .bind(cfg.run, ev.batch, lo, lo + ev.games, ev.games, isPaired(ev) ? 1 : 0, JSON.stringify(ev.checkpoints), evalSpec(ev), now),
    );
  } else if (!cfg.eval && oldBatch !== null) {
    stmts.push(
      db.prepare(`DELETE FROM pool WHERE scope = ? AND pool LIKE 'eval:%'`).bind(scope),
      db.prepare(`UPDATE runs SET eval_batch = NULL WHERE run = ?`).bind(cfg.run),
      ...replaceOld(),
    );
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
  gpu: GpuMode; // rev 7
  providers: Provider[] | null; // rev 7
  min_agent: string | null; // rev 9
}

export async function putCampaign(env: Env, cfg: CampaignCfg, now: number): Promise<{ ok: true; created: boolean; cursor: number }> {
  const db = env.DB;
  const old = await getCampaign(db, cfg.campaign);
  // The cursor never rewinds: a lower seed_base leaves it where it is, a higher one moves it up.
  const cursor = old ? Math.max(old.cursor, cfg.seed_base) : cfg.seed_base;
  await db
    .prepare(
      `INSERT INTO campaigns (campaign, build_id, mode, env, seed_base, cursor, chunk, priority, state, needs, gpu, providers, min_agent, created_at, updated_at)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
       ON CONFLICT (campaign) DO UPDATE SET build_id = excluded.build_id, mode = excluded.mode, env = excluded.env,
         seed_base = excluded.seed_base, cursor = MAX(campaigns.cursor, excluded.cursor), chunk = excluded.chunk,
         priority = excluded.priority, state = excluded.state, needs = excluded.needs, gpu = excluded.gpu,
         providers = excluded.providers, min_agent = excluded.min_agent, updated_at = excluded.updated_at`,
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
      cfg.gpu,
      cfg.providers ? JSON.stringify(cfg.providers) : null,
      cfg.min_agent,
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
  reference_size: number | null;
  now: number;
}

// Registers a checkpoint. A gen newer than the current one becomes current for self-play (and
// starts its quota count); rev 5: an older one is only registered, as an eval opponent or a
// head-to-head's label (`current: false`).
export async function publishCheckpoint(env: Env, c: CheckpointReq): Promise<{ ok: true; unchanged: boolean; current: boolean }> {
  const db = env.DB;
  const run = await getRun(db, c.run);
  if (!run) throw notFound(`no run ${c.run}`);
  const old = await getCheckpoint(db, c.run, c.gen);
  if (old) {
    if (old.sha256 === c.sha256 && old.reference_sha256 === c.reference_sha256 && old.model === c.model && old.reference === c.reference) {
      return { ok: true, unchanged: true, current: run.current_gen === c.gen };
    }
    throw conflict(`checkpoint ${c.gen} of ${c.run} was already published with different files`);
  }
  const insert = db
    .prepare(
      `INSERT INTO checkpoints (run, gen, model, sha256, reference, reference_sha256, size, reference_size, t) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)`,
    )
    .bind(c.run, c.gen, c.model, c.sha256, c.reference, c.reference_sha256, c.size, c.reference_size, c.now);
  if (run.current_gen !== null && c.gen < run.current_gen) {
    await db.batch([insert]);
    return { ok: true, unchanged: false, current: false };
  }
  await db.batch([
    insert,
    db.prepare(`UPDATE runs SET current_gen = ?, updated_at = ? WHERE run = ?`).bind(c.gen, c.now, c.run),
    // A new generation starts its quota count from zero.
    db.prepare(`INSERT OR REPLACE INTO quotas (run, gen, accepted, t) VALUES (?, ?, 0, ?)`).bind(c.run, c.gen, c.now),
  ]);
  return { ok: true, unchanged: false, current: true };
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

// ---------- definitions (rev 6) ----------

export interface DefinitionAnswer {
  ok: true;
  kind: string;
  id: string;
  version: number;
  key: string;
  sha256: string;
  unchanged: boolean;
  current: true;
}

// Registers a verified definition (the Worker checked its signature and contents) and makes it the
// current one of its run or campaign. Versions only go up: the same version again is fine only
// with the same bytes.
export async function putDefinition(env: Env, row: DefinitionRow): Promise<DefinitionAnswer> {
  const db = env.DB;
  const cur = await currentDef(db, row.kind, row.id);
  const ans = { ok: true as const, kind: row.kind, id: row.id, version: row.version, key: row.key, sha256: row.sha256, current: true as const };
  if (cur && row.version === cur.version) {
    if (cur.sha256 === row.sha256) return { ...ans, unchanged: true };
    throw conflict(`${row.kind} ${row.id} version ${row.version} was registered with other bytes: sign a new version`);
  }
  if (cur && row.version < cur.version) {
    throw conflict(`${row.kind} ${row.id} is at version ${cur.version}: versions only go up (got ${row.version})`, { current: cur.version });
  }
  await db
    .prepare(
      `INSERT INTO definitions (kind, id, version, key, sha256, size, created, expires, game, builds, runtime, checkpoints, jobs, key_id, registered_at)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
    )
    .bind(
      row.kind,
      row.id,
      row.version,
      row.key,
      row.sha256,
      row.size,
      row.created,
      row.expires,
      row.game,
      row.builds,
      row.runtime,
      row.checkpoints,
      row.jobs,
      row.key_id,
      row.registered_at,
    )
    .run();
  return { ...ans, unchanged: false };
}
