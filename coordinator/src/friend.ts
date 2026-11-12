// Friend-facing API (PROTOCOL 3): bearer = machine token, except /v1/enroll.

import * as book from "./book";
import { blockedBuilds, recordBlock } from "./blocks";
import { authMachine, type Ctx, type MachineRow } from "./ctx";
import { bookFor } from "./leasebook";
import { minAgentOk } from "./manifest";
import { parseRanges, total } from "./ranges";
import { take } from "./ratelimit";
import { cleanStats, telemetry } from "./telemetry";
import {
  BUILD_RE,
  HEX64_RE,
  HttpError,
  INVITE_RE,
  type Json,
  LEASE_RE,
  bad,
  bool,
  checkKey,
  clamp,
  cmpSemver,
  conflict,
  int,
  isSemver,
  json,
  maxSemver,
  newMachineId,
  newToken,
  notFound,
  num,
  obj,
  oneOf,
  parseJson,
  readBounded,
  readJson,
  sha256Hex,
  str,
  strList,
  unwrap,
} from "./util";

export const KINDS = ["fuzz", "eval", "selfplay", "diag"] as const;
export const CHANNELS = ["canary", "stable"] as const;
// Rev 5: the first agent that downloads an eval lease's `checkpoints` (older agents know only
// `checkpoint`, so they'd start the worker without its networks).
export const EVAL_MIN_AGENT = "0.2.3";
const WAIT_S = 300;
const LOGS_RESEND_S = 1800; // re-ask for an upload_diag that never arrived

// ---------- profile ----------

export interface Profile {
  os?: string;
  cpu?: string;
  cores?: number;
  threads?: number;
  perf_cores?: number;
  eff_cores?: number;
  ram_mb?: number;
  disk_free_mb?: number;
  gpus?: { vendor?: string; name?: string; vram_mb?: number; driver?: string }[];
  providers?: string[];
  idle_h_day?: number;
  on_h_day?: number;
  [k: string]: unknown;
}

// Type-checks the fields PROTOCOL 3 defines (unknown fields are kept, for newer agents).
export function cleanProfile(p: Record<string, any>): Profile {
  for (const k of ["os", "cpu"]) str(p[k], `profile.${k}`, { optional: true, max: 200 });
  for (const k of ["cores", "threads", "perf_cores", "eff_cores", "ram_mb", "disk_free_mb"]) {
    int(p[k], `profile.${k}`, { optional: true, min: 0 });
  }
  if (p.gpus !== undefined) {
    if (!Array.isArray(p.gpus) || p.gpus.length > 16) throw bad("profile.gpus must be a list");
    p.gpus.forEach((g: unknown, i: number) => {
      const o = obj(g, `profile.gpus[${i}]`);
      str(o.vendor, `profile.gpus[${i}].vendor`, { optional: true, max: 64 });
      str(o.name, `profile.gpus[${i}].name`, { optional: true, max: 200 });
      str(o.driver, `profile.gpus[${i}].driver`, { optional: true, max: 64 });
      int(o.vram_mb, `profile.gpus[${i}].vram_mb`, { optional: true, min: 0 });
    });
  }
  strList(p.providers, "profile.providers", { optional: true, max: 32, itemMax: 64 });
  // Availability: hours per day the idle policy let it work / the agent was running (7-day means).
  for (const k of ["idle_h_day", "on_h_day"]) num(p[k], `profile.${k}`, { optional: true, min: 0, max: 24 });
  if (JSON.stringify(p).length > 8192) throw bad("profile is larger than 8 KB");
  return p as Profile;
}

export function classOf(p: Profile | null): string | null {
  if (!p) return null;
  const gpu = p.gpus?.find((g) => g.name)?.name ?? "no gpu";
  return `${p.cpu ?? "unknown cpu"} / ${gpu}`.slice(0, 200);
}

async function latestRelease(db: D1Database, channel: string) {
  return db
    .prepare(`SELECT * FROM agent_releases WHERE channel = ? ORDER BY id DESC LIMIT 1`)
    .bind(channel)
    .first<{ version: string; build_id: string; min_agent: string; channel: string }>();
}

// ---------- POST /v1/enroll ----------

export async function enroll(c: Ctx): Promise<Response> {
  // Per client IP (Cloudflare always sets cf-connecting-ip; local tools may not).
  const ip = c.req.headers.get("cf-connecting-ip");
  if (ip) await take(c, `ip:${ip.slice(0, 64)}`, "enroll", "enroll");
  const b = await readJson(c.req);
  const invite = str(b.invite, "invite", { max: 32 }).trim().toUpperCase();
  const name = str(b.name, "name", { min: 1, max: 64 }).trim();
  if (!name) throw bad("name is required");
  const profile = cleanProfile(obj(b.profile, "profile"));
  const digest = str(b.profile_digest, "profile_digest", { optional: true, re: HEX64_RE }) ?? null;
  const used = () => new HttpError(403, "unauthorized", "unknown or already used invite code");
  if (!INVITE_RE.test(invite)) throw used();
  const inv = await c.env.DB.prepare(`SELECT * FROM invites WHERE code = ?`).bind(invite).first<{ used_at: number | null; channel: string }>();
  if (!inv || inv.used_at !== null) throw used();

  const agent = c.req.headers.get("x-farm-agent");
  if (agent !== null && isSemver(agent)) {
    const rel = await latestRelease(c.env.DB, inv.channel);
    if (rel && cmpSemver(agent, rel.min_agent) < 0) {
      // An unenrolled agent can't update itself: the friend needs a newer installer.
      const download = `${c.url.origin}/download${inv.channel === "stable" ? "" : `?channel=${inv.channel}`}`;
      throw new HttpError(426, "update_required", `agent ${agent} is older than ${rel.min_agent}`, { min_agent: rel.min_agent, download });
    }
  }

  const machineId = newMachineId();
  const token = newToken();
  const [claim, insert] = await c.env.DB.batch([
    c.env.DB.prepare(`UPDATE invites SET used_at = ?, machine_id = ? WHERE code = ? AND used_at IS NULL`).bind(c.now, machineId, invite),
    // Inserted only if the UPDATE above won the invite (a concurrent enroll can't use it twice).
    c.env.DB.prepare(
      `INSERT INTO machines (machine_id, token_hash, name, channel, invite, created_at, last_seen, agent_version, profile, profile_digest, class)
       SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM invites WHERE code = ? AND machine_id = ?)`,
    ).bind(
      machineId,
      await sha256Hex(token),
      name,
      inv.channel,
      invite,
      c.now,
      c.now,
      agent !== null && isSemver(agent) ? agent : null,
      JSON.stringify(profile),
      digest,
      classOf(profile),
      invite,
      machineId,
    ),
  ]);
  if (!claim.meta.changes || !insert.meta.changes) throw used();
  return json({ machine_id: machineId, token, channel: inv.channel });
}

// ---------- POST /v1/lease ----------

interface Caps {
  threads: number;
  ram_mb: number;
  gpu: boolean;
  disk_mb: number;
}

interface BuildInfo {
  build_id: string;
  kind: string;
  min_agent: string | null;
  aw: number | null;
  size: number;
}

interface Candidate {
  scope: string;
  kind: book.Kind;
  run?: string;
  campaign?: string;
  priority: number;
  tier: number; // 1: a run's games; 2: backfill (fuzz, or a GPU-preferring run on a CPU-only PC)
  fit: number;
  configured: "auto" | number;
}

// Lease sizing: about TARGET_S seconds of this machine's measured rate (ids per running second),
// clamped; DEFAULT before any history from this machine or its hardware class.
export const TARGET_S = 450;
export const SIZING: Record<Exclude<book.Kind, "diag">, { def: number; min: number; max: number }> = {
  selfplay: { def: 16, min: 1, max: 4096 },
  eval: { def: 10, min: 2, max: 1024 },
  fuzz: { def: 20000, min: 1000, max: book.MAX_LEASE_IDS },
};

export function leaseSize(kind: Exclude<book.Kind, "diag">, configured: "auto" | number, rate: number | null): number {
  if (typeof configured === "number") return configured;
  const s = SIZING[kind];
  if (rate !== null && rate > 0) return clamp(Math.round(rate * TARGET_S), s.min, s.max);
  return s.def;
}

function pendingLogsQuery(db: D1Database, m: MachineRow, now: number) {
  return db
    .prepare(
      `SELECT id FROM diag_requests WHERE machine_id = ? AND kind = 'logs' AND done_at IS NULL
       AND (delivered_at IS NULL OR delivered_at < ?) ORDER BY id`,
    )
    .bind(m.machine_id, now - LOGS_RESEND_S);
}

// upload_diag names to send this machine now (and mark them sent).
export async function takeUploadDiag(db: D1Database, m: MachineRow, now: number): Promise<string[] | null> {
  const { results } = await pendingLogsQuery(db, m, now).all<{ id: number }>();
  if (!results.length) return null;
  await db
    .prepare(`UPDATE diag_requests SET delivered_at = ? WHERE id IN (${results.map(() => "?").join(",")})`)
    .bind(now, ...results.map((r) => r.id))
    .run();
  return ["logs"];
}

export async function lease(c: Ctx): Promise<Response> {
  const m = await authMachine(c);
  await take(c, m.machine_id, "lease", "lease");
  const db = c.env.DB;
  const b = await readJson(c.req);
  const digest = str(b.profile_digest, "profile_digest", { re: HEX64_RE });
  const profile = b.profile === null || b.profile === undefined ? null : cleanProfile(obj(b.profile, "profile"));
  const capsIn = obj(b.caps, "caps");
  const caps: Caps = {
    threads: int(capsIn.threads, "caps.threads", { min: 1, max: 4096 }),
    ram_mb: int(capsIn.ram_mb, "caps.ram_mb", { min: 0 }),
    gpu: bool(capsIn.gpu, "caps.gpu"),
    disk_mb: int(capsIn.disk_mb, "caps.disk_mb", { min: 0 }),
  };
  const cachedIn = obj(b.cached, "cached", { optional: true }) ?? {};
  const cachedBuilds = new Set(strList(cachedIn.builds, "cached.builds", { optional: true, max: 1000, itemMax: 64 }));
  const cachedCkpts = new Set(strList(cachedIn.ckpts, "cached.ckpts", { optional: true, max: 1000, itemMax: 128 }));
  // Kinds this coordinator doesn't know are ignored (a newer agent may offer more).
  const kinds = new Set(strList(b.kinds, "kinds", { max: 32, itemMax: 32 }).filter((k) => (KINDS as readonly string[]).includes(k)));
  const agent = c.req.headers.get("x-farm-agent");
  if (!agent || !isSemver(agent)) throw bad("x-farm-agent header (the agent's semver) is required");
  const awHeader = c.req.headers.get("x-farm-aw") ?? "1";
  if (!/^\d{1,4}$/.test(awHeader)) throw bad("x-farm-aw must be an integer");
  const aw = Number(awHeader);

  // Profile: the digest is the agent's; we only compare it with the last one.
  if (digest !== m.profile_digest) {
    if (!profile) throw conflict("unknown profile digest: send the profile with it", { need_profile: true });
    await db
      .prepare(`UPDATE machines SET profile = ?, profile_digest = ?, class = ? WHERE machine_id = ?`)
      .bind(JSON.stringify(profile), digest, classOf(profile), m.machine_id)
      .run();
    m.class = classOf(profile);
  }
  const capsJson = JSON.stringify(caps);
  if (capsJson !== m.caps) await db.prepare(`UPDATE machines SET caps = ? WHERE machine_id = ?`).bind(capsJson, m.machine_id).run();

  const [runsR, campsR, releaseR, pausedR, activeR, poolR, ratesR, classR, diagR] = await db.batch<any>([
    db.prepare(`SELECT * FROM runs WHERE state = 'active'`),
    db.prepare(`SELECT * FROM campaigns WHERE state = 'active'`),
    db.prepare(`SELECT * FROM agent_releases WHERE channel = ? ORDER BY id DESC LIMIT 1`).bind(m.channel),
    db.prepare(`SELECT value FROM settings WHERE key = 'paused'`),
    db.prepare(`SELECT scope, kind, COUNT(*) AS n FROM leases WHERE state = 'active' GROUP BY scope, kind`),
    db.prepare(`SELECT scope, pool, SUM(hi - lo) AS n FROM pool GROUP BY scope, pool`),
    db.prepare(`SELECT kind, scope, rate FROM rates WHERE machine_id = ?`).bind(m.machine_id),
    db
      .prepare(
        `SELECT r.kind, r.scope, AVG(r.rate) AS rate FROM rates r JOIN machines mm ON mm.machine_id = r.machine_id
         WHERE mm.class = ? GROUP BY r.kind, r.scope`,
      )
      .bind(m.class ?? ""),
    db
      .prepare(
        `SELECT d.id FROM diag_requests d WHERE d.machine_id = ? AND d.done_at IS NULL AND d.kind != 'logs'
         AND NOT EXISTS (SELECT 1 FROM leases l WHERE l.lease_id = d.lease_id AND l.state = 'active') ORDER BY d.id`,
      )
      .bind(m.machine_id),
  ]);
  const runs = runsR.results as book.RunRow[];
  const camps = campsR.results as book.CampaignRow[];
  const release = (releaseR.results[0] ?? null) as { version: string; min_agent: string } | null;

  const buildIds = [...new Set([...runs.flatMap((r) => [r.build_id, r.runtime]), ...camps.map((x) => x.build_id)].filter(Boolean))] as string[];
  const builds = new Map<string, BuildInfo>();
  if (buildIds.length) {
    const { results } = await db
      .prepare(`SELECT build_id, kind, min_agent, aw, size FROM builds WHERE build_id IN (${buildIds.map(() => "?").join(",")})`)
      .bind(...buildIds)
      .all<BuildInfo>();
    for (const x of results) builds.set(x.build_id, x);
  }

  // --- candidates: kinds the agent offers, that fit its caps ---
  const cands: Candidate[] = [];
  const poolN = new Map<string, number>((poolR.results as any[]).map((p) => [`${p.scope}|${p.pool}`, p.n]));
  const needsOk = (n: book.Needs | null | undefined) => !n || (caps.threads >= n.threads && caps.ram_mb >= n.ram_mb);
  const diskOk = (ids: (string | null)[], extra = 0) => {
    const need = ids.reduce((s, id) => s + (id && !cachedBuilds.has(id) ? (builds.get(id)?.size ?? 0) : 0), extra);
    return need <= caps.disk_mb * 1024 * 1024;
  };
  for (const r of runs) {
    const scope = book.runScope(r.run);
    const cachedBonus = cachedBuilds.has(r.build_id) ? 1.2 : 1;
    if (kinds.has("selfplay") && r.selfplay && r.current_gen !== null) {
      const sp = JSON.parse(r.selfplay) as book.SelfplayCfg;
      if (sp && needsOk(sp.needs) && !(sp.gpu === "required" && !caps.gpu) && diskOk([r.build_id, r.runtime])) {
        const backfill = sp.gpu === "preferred" && !caps.gpu;
        const ckptBonus = cachedCkpts.has(`${r.run}/${r.current_gen}`) ? 1.1 : 1;
        cands.push({
          scope,
          kind: "selfplay",
          run: r.run,
          priority: r.priority,
          tier: backfill ? 2 : 1,
          fit: (backfill ? 0.25 : 1) * cachedBonus * ckptBonus,
          configured: sp.games_per_lease,
        });
      }
    }
    if (kinds.has("eval") && r.eval) {
      const ev = JSON.parse(r.eval) as book.EvalCfg | null;
      if (
        ev &&
        Array.isArray(ev.checkpoints) &&
        (poolN.get(`${scope}|eval:${ev.batch}`) ?? 0) > 0 &&
        needsOk(ev.needs) &&
        !(ev.gpu === "required" && !caps.gpu)
      ) {
        // Rev 5: the lease brings every network the seats name; uncached ones count against the disk.
        const keys = ev.checkpoints.map((g) => `${r.run}/${g}`);
        let ckptBytes = 0;
        if (keys.some((k) => !cachedCkpts.has(k))) {
          const { results } = await db
            .prepare(`SELECT gen, size FROM checkpoints WHERE run = ? AND gen IN (${ev.checkpoints.map(() => "?").join(",")})`)
            .bind(r.run, ...ev.checkpoints)
            .all<{ gen: number; size: number | null }>();
          ckptBytes = results.reduce((s, x) => s + (cachedCkpts.has(`${r.run}/${x.gen}`) ? 0 : (x.size ?? 0)), 0);
        }
        if (diskOk([r.build_id, r.runtime], ckptBytes)) {
          const backfill = ev.gpu === "preferred" && !caps.gpu;
          const ckptBonus = keys.length && keys.every((k) => cachedCkpts.has(k)) ? 1.1 : 1;
          cands.push({
            scope,
            kind: "eval",
            run: r.run,
            priority: r.priority,
            tier: backfill ? 2 : 1,
            fit: (backfill ? 0.25 : 1) * cachedBonus * ckptBonus,
            configured: ev.games_per_lease,
          });
        }
      }
    }
  }
  if (kinds.has("fuzz")) {
    for (const x of camps) {
      const needs = parseJson<book.Needs | null>(x.needs, null);
      if (!needsOk(needs) || !diskOk([x.build_id])) continue;
      cands.push({
        scope: book.campaignScope(x.campaign),
        kind: "fuzz",
        campaign: x.campaign,
        priority: x.priority,
        tier: 2,
        // Fuzzing goes preferentially to CPU-only PCs.
        fit: (caps.gpu ? 0.5 : 1) * (cachedBuilds.has(x.build_id) ? 1.2 : 1),
        configured: x.chunk === "auto" ? "auto" : Number(x.chunk),
      });
    }
  }

  // --- builds Windows refused to start on this machine (rev 5): not offered to it for a while ---
  const blocked = await blockedBuilds(db, m.machine_id, agent, c.now);

  // --- agent version: a job whose build needs a newer agent makes the agent update first ---
  let need: string | null = release && cmpSemver(agent, release.min_agent) < 0 ? release.min_agent : null;
  const usable = cands.filter((cd) => {
    const r = cd.run ? runs.find((x) => x.run === cd.run) : null;
    const ids = r ? [r.build_id, r.runtime] : [camps.find((x) => x.campaign === cd.campaign)!.build_id];
    if (ids.some((id) => id && blocked.has(id))) return false;
    const infos = ids.filter(Boolean).map((id) => builds.get(id!));
    if (infos.some((i) => !i)) return false; // build not registered
    const req = maxSemver([...infos.map((i) => i!.min_agent), cd.kind === "eval" ? EVAL_MIN_AGENT : null]);
    if (req && !minAgentOk(agent, req)) {
      // Only ask for an update the channel can deliver.
      if (release && cmpSemver(req, release.version) <= 0) need = maxSemver([need, req]);
      return false;
    }
    const worker = infos.find((i) => i!.kind === "worker");
    return !worker || worker.aw === null || worker.aw === aw;
  });
  if (need && cmpSemver(agent, need) < 0) return json({ update: { min_agent: need } });

  const uploadDiag = await takeUploadDiag(db, m, c.now);
  const withDiag = (o: Record<string, unknown>) => json(uploadDiag ? { ...o, upload_diag: uploadDiag } : o);

  if (pausedR.results[0]?.value === "1" || m.paused) return withDiag({ wait_s: WAIT_S, reason: "paused_by_admin" });

  // --- diagnostics asked for this machine come first ---
  if (kinds.has("diag")) {
    for (const d of diagR.results as { id: number }[]) {
      const l = unwrap(await bookFor(c.env, book.DIAG_SCOPE).grant({ now: c.now, machine_id: m.machine_id, kind: "diag", want: 0, diag_id: d.id }));
      if (l) return withDiag({ lease: l });
    }
  }

  // --- then runs' games by priority and fit, then backfill; share by active leases ---
  const active = new Map<string, number>((activeR.results as any[]).map((a) => [`${a.scope}|${a.kind}`, a.n]));
  const rate = new Map<string, number>((ratesR.results as any[]).map((x) => [`${x.scope}|${x.kind}`, x.rate]));
  const classRate = new Map<string, number>((classR.results as any[]).map((x) => [`${x.scope}|${x.kind}`, x.rate]));
  const score = (cd: Candidate) => (cd.priority * cd.fit) / (1 + (active.get(`${cd.scope}|${cd.kind}`) ?? 0));
  usable.sort((a, b) => a.tier - b.tier || score(b) - score(a) || a.scope.localeCompare(b.scope));
  for (const cd of usable) {
    const key = `${cd.scope}|${cd.kind}`;
    const want = leaseSize(cd.kind as Exclude<book.Kind, "diag">, cd.configured, rate.get(key) ?? classRate.get(key) ?? null);
    const l = unwrap(
      await bookFor(c.env, cd.scope).grant({ now: c.now, machine_id: m.machine_id, kind: cd.kind, want, run: cd.run, campaign: cd.campaign }),
    );
    if (l) return withDiag({ lease: l });
  }
  return withDiag({ wait_s: WAIT_S, reason: "no_work" });
}

// ---------- leases ----------

async function ownLeaseRow(c: Ctx, m: MachineRow, leaseId: string): Promise<book.LeaseRow> {
  if (!LEASE_RE.test(leaseId)) throw notFound("no such lease");
  const l = await book.getLease(c.env.DB, leaseId);
  if (!l || l.machine_id !== m.machine_id) throw notFound("no such lease");
  return l;
}

function cleanProgress(v: unknown): Record<string, number> | undefined {
  if (v === undefined || v === null) return undefined;
  const p = obj(v, "progress");
  const out: Record<string, number> = {};
  for (const k of ["games_done", "games_total"]) {
    const x = int(p[k], `progress.${k}`, { optional: true, min: 0 });
    if (x !== undefined) out[k] = x;
  }
  return out;
}

export async function beat(c: Ctx, leaseId: string): Promise<Response> {
  const m = await authMachine(c);
  await take(c, m.machine_id, "beat", "beat");
  const l = await ownLeaseRow(c, m, leaseId);
  const b = await readJson(c.req, 64 * 1024);
  const state = oneOf(b.state, "state", ["running", "paused"] as const);
  const progress = cleanProgress(b.progress);
  const stats = cleanStats(b.stats);
  const ans = unwrap(
    await bookFor(c.env, l.scope).beat({ now: c.now, machine_id: m.machine_id, lease_id: leaseId, scope: l.scope, state, progress, stats }),
  );
  if (stats) {
    telemetry(c.env.TELEMETRY).write({ machine_id: m.machine_id, class: m.class, lease_id: leaseId, kind: l.kind, scope: l.scope, event: "beat", stats });
  }
  const uploadDiag = await takeUploadDiag(c.env.DB, m, c.now);
  return json(uploadDiag ? { ...ans, upload_diag: uploadDiag } : ans);
}

export async function result(c: Ctx, leaseId: string, seqStr: string): Promise<Response> {
  const m = await authMachine(c);
  if (!/^\d{1,6}$/.test(seqStr)) throw bad("seq must be an integer 0..999999");
  const seq = Number(seqStr);
  const sha = c.req.headers.get("x-sha256")?.toLowerCase() ?? "";
  if (!HEX64_RE.test(sha)) throw bad("x-sha256 header (lowercase hex sha256 of the body) is required");
  let ids: Json = null;
  const idsHeader = c.req.headers.get("x-ids");
  if (idsHeader !== null) {
    if (idsHeader.length > 16384) throw bad("x-ids is longer than 16 KB");
    try {
      ids = JSON.parse(idsHeader);
    } catch {
      throw bad("x-ids must be JSON: [[lo, hi], ...]");
    }
  }
  const genHeader = c.req.headers.get("x-gen");
  if (genHeader !== null && !/^\d{1,9}$/.test(genHeader)) throw bad("x-gen must be an integer");
  const l = await ownLeaseRow(c, m, leaseId);
  // A retry of a stored chunk is answered without touching R2 (LeaseBook re-checks under its lock).
  const prior = await c.env.DB.prepare(`SELECT sha256, games, dup_ids, kind, held FROM results WHERE lease_id = ? AND seq = ?`)
    .bind(leaseId, seq)
    .first<{ sha256: string; games: number; dup_ids: string; kind: string; held: string | null }>();
  if (prior) {
    if (prior.sha256 !== sha) throw conflict(`result ${seq} of this lease was already uploaded with a different sha256`);
    await readBounded(c.req, book.MAX_RESULT);
    const held = prior.kind === "eval" ? { held: total(parseJson(prior.held, [])) } : {};
    return json({ ok: true, duplicate: true, credited: prior.games, already_credited: total(parseJson(prior.dup_ids, [])), ...held });
  }
  if (l.state === "cancelled") throw new HttpError(410, "gone", "lease was cancelled");
  if (ids !== null) {
    if (l.pool === null) throw bad("x-ids: a diag lease has no ids");
    parseRanges(ids, "x-ids", [l.lo!, l.hi!]);
  }
  const body = await readBounded(c.req, book.MAX_RESULT);
  if ((await sha256Hex(body)) !== sha) throw bad("body does not match x-sha256");
  const fuzz = l.kind === "fuzz" ? book.parseFuzz(body) : null;

  // Bytes first (they can be 32 MB, more than Durable Object RPC carries), then LeaseBook decides.
  const key = book.resultKey(l, seq, sha, c.now);
  await c.env.BUCKET.put(key, body, { sha256: sha });
  let out;
  try {
    out = await bookFor(c.env, l.scope).putResult({
      now: c.now,
      machine_id: m.machine_id,
      lease_id: leaseId,
      seq,
      sha256: sha,
      size: body.byteLength,
      ids,
      gen: genHeader === null ? null : Number(genHeader),
      key,
      fuzz,
    });
  } catch (e) {
    await c.env.BUCKET.delete(key);
    throw e;
  }
  if ("err" in out || !out.ok.keep) await c.env.BUCKET.delete(key);
  const { keep: _keep, ...ans } = unwrap(out);
  return json(ans);
}

export async function done(c: Ctx, leaseId: string): Promise<Response> {
  const m = await authMachine(c);
  const l = await ownLeaseRow(c, m, leaseId);
  const b = await readJson(c.req);
  if (!Array.isArray(b.results) || b.results.length > 100000 || b.results.some((s: unknown) => !Number.isSafeInteger(s) || (s as number) < 0)) {
    throw bad("results must be a list of result seqs");
  }
  const stats = cleanStats(b.stats);
  const givenBack = bool(b.given_back, "given_back", { optional: true }) ?? false;
  const unfinished = b.unfinished === undefined || b.unfinished === null ? null : (obj(b.unfinished, "unfinished") as Record<string, Json>);
  const ans = unwrap(
    await bookFor(c.env, l.scope).done({
      now: c.now,
      machine_id: m.machine_id,
      lease_id: leaseId,
      scope: l.scope,
      results: b.results,
      stats,
      given_back: givenBack,
      unfinished,
    }),
  );
  if (stats) {
    telemetry(c.env.TELEMETRY).write({ machine_id: m.machine_id, class: m.class, lease_id: leaseId, kind: l.kind, scope: l.scope, event: "done", stats });
  }
  return json(ans);
}

// ---------- artifacts, updates, crashes, diagnostics ----------

// Serves an R2 object, honouring `range` (206) and `if-none-match` (304).
export async function serveObject(c: Ctx, key: string): Promise<Response> {
  const head = c.req.method === "HEAD";
  const obj = head ? await c.env.BUCKET.head(key) : await c.env.BUCKET.get(key, { range: c.req.headers, onlyIf: c.req.headers });
  if (!obj) throw notFound(`no artifact ${key}`);
  const headers = new Headers({ etag: obj.httpEtag, "accept-ranges": "bytes", "cache-control": "private, no-store" });
  obj.writeHttpMetadata(headers);
  if (!headers.has("content-type")) headers.set("content-type", "application/octet-stream");
  if (head) {
    headers.set("content-length", String(obj.size));
    return new Response(null, { headers });
  }
  if (!("body" in obj)) return new Response(null, { status: 304, headers });
  const body = obj as R2ObjectBody;
  const r = body.range as { offset?: number; length?: number; suffix?: number } | undefined;
  if (c.req.headers.has("range") && r) {
    const start = r.suffix !== undefined ? obj.size - r.suffix : (r.offset ?? 0);
    const len = r.suffix !== undefined ? r.suffix : (r.length ?? obj.size - start);
    headers.set("content-range", `bytes ${start}-${start + len - 1}/${obj.size}`);
    headers.set("content-length", String(len));
    return new Response(body.body, { status: 206, headers });
  }
  headers.set("content-length", String(obj.size));
  return new Response(body.body, { headers });
}

export async function artifact(c: Ctx, rawKey: string): Promise<Response> {
  await authMachine(c);
  const key = checkKey(decodeURIComponent(rawKey));
  if (!key.startsWith("builds/") && !key.startsWith("ckpt/")) throw notFound(`no artifact ${key}`);
  return serveObject(c, key);
}

export async function agentLatest(c: Ctx): Promise<Response> {
  const m = await authMachine(c);
  const channel = oneOf(c.url.searchParams.get("channel") ?? m.channel, "channel", CHANNELS);
  const rel = await latestRelease(c.env.DB, channel);
  if (!rel) throw notFound(`no agent release on ${channel}`);
  return json({ version: rel.version, build_id: rel.build_id, manifest: `builds/${rel.build_id}/manifest.json`, min_agent: rel.min_agent });
}

export async function crash(c: Ctx): Promise<Response> {
  const m = await authMachine(c);
  await take(c, m.machine_id, "crash", "crash report");
  const b = await readJson(c.req, 256 * 1024);
  const kind = oneOf(b.kind, "kind", ["panic", "crash", "agent", "blocked"] as const);
  const buildId = str(b.build_id, "build_id", { optional: true, re: BUILD_RE }) ?? null;
  const leaseId = str(b.lease_id, "lease_id", { optional: true, re: LEASE_RE }) ?? null;
  const osError = int(b.os_error, "os_error", { optional: true, min: -(2 ** 31), max: 2 ** 32 }) ?? null;
  const message = typeof b.message === "string" ? b.message.slice(0, 4096) : null;
  const backtrace = typeof b.backtrace === "string" ? b.backtrace.slice(0, 65536) : null;
  if (kind === "blocked" && !buildId) throw bad("a blocked report needs the build_id Windows refused to start");
  const stmts = [
    c.env.DB.prepare(
      `INSERT INTO crashes (machine_id, build_id, kind, message, backtrace, lease_id, agent_version, os_error, t) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)`,
    ).bind(m.machine_id, buildId, kind, message, backtrace, leaseId, m.agent_version, osError, c.now),
  ];
  // Rev 5: Windows refused to start this worker build here. It isn't offered to this machine for
  // a day, or until a newer agent asks (blocks.ts).
  if (kind === "blocked") {
    stmts.push(recordBlock(c.env.DB, { machine_id: m.machine_id, build_id: buildId!, agent_version: m.agent_version, os_error: osError, message, now: c.now }));
  }
  await c.env.DB.batch(stmts);
  return json({ ok: true });
}

// A friend's "Send diagnostics" (x-farm-diag: user): allowed without a request from the desktop,
// once an hour per machine, up to 20 MB.
export const MAX_USER_DIAG = 20 * 1024 * 1024;

export async function diagUpload(c: Ctx, name: string): Promise<Response> {
  const m = await authMachine(c);
  if (!/^[a-z0-9][a-z0-9._-]{0,63}$/.test(name)) throw bad("bad diag name");
  const req = await c.env.DB.prepare(
    `SELECT id FROM diag_requests WHERE machine_id = ? AND kind = ? AND done_at IS NULL AND delivered_at IS NOT NULL ORDER BY id LIMIT 1`,
  )
    .bind(m.machine_id, name)
    .first<{ id: number }>();
  const user = !req && c.req.headers.get("x-farm-diag")?.toLowerCase() === "user";
  if (!req && !user) throw conflict(`no ${name} upload was asked of this machine`);
  // An oversize upload is refused (413) before it can use up the hour.
  const body = await readBounded(c.req, user ? MAX_USER_DIAG : book.MAX_RESULT);
  if (user) await take(c, m.machine_id, "diag_user", "diagnostics upload");
  const key = `diag/${m.machine_id}/${c.now}-${name}`;
  await c.env.BUCKET.put(key, body);
  await c.env.DB.batch([
    c.env.DB.prepare(`INSERT INTO diag_uploads (machine_id, name, key, size, request_id, user, t) VALUES (?, ?, ?, ?, ?, ?, ?)`).bind(
      m.machine_id,
      name,
      key,
      body.byteLength,
      req?.id ?? null,
      user ? 1 : 0,
      c.now,
    ),
    // Every open request of this kind for the machine is answered by this upload.
    c.env.DB.prepare(`UPDATE diag_requests SET done_at = ? WHERE machine_id = ? AND kind = ? AND done_at IS NULL`).bind(c.now, m.machine_id, name),
  ]);
  return json({ ok: true, key });
}
