// Friend-facing API (PROTOCOL 3): bearer = machine token, except /v1/enroll.

import * as book from "./book";
import { blockedBuilds, recordBlock } from "./blocks";
import { config } from "./config";
import { authMachine, type Ctx, type MachineRow } from "./ctx";
import { currentDefs, DEF_KEY_RE, hasVariants, leaseProblem, type JobKind } from "./definitions";
import { bookFor } from "./leasebook";
import { minAgentOk } from "./manifest";
import { DEFAULT_PLATFORM, type Platform, requestPlatform, storedPlatform } from "./platform";
import { parseRanges, total } from "./ranges";
import { ipKey, take } from "./ratelimit";
import {
  type Alternate,
  type CampaignAlternate,
  GPU_LEASE_MIN,
  LUID_RE,
  MAX_RUNTIME_PROVIDERS,
  MAX_RUNTIMES,
  type Provider,
  type Routing,
  altPlatform,
  altProviders,
  gpuEligible,
  gpuNeedOf,
  hasAdapter,
  isGpuProvider,
  jobRank,
  kindRank,
  machineOptIns,
  optInOf,
  place,
  requiresRuntimeOf,
  runtimeProvidersOf,
  storedRouting,
  verifiedProviders,
  vendorPackProblem,
  vendorRestricted,
  vendorsOffOf,
  CPU_SELFPLAY_TIER,
  DEFERRED_EVAL_RANK,
  FUZZ_TAIL_S,
  type HwRouting,
  LANE_SOON_S,
  LANE_WAIT_S,
  type SortKey,
  batchMachines,
  evalBehind,
  evalCpuFloor,
  hwRoutingOf,
  freshApplies,
  laneApplies,
  sortsBefore,
  yieldTarget,
  ASK_BUILDS_MAX,
  LANE_MAX_DRAIN_S,
  drainEstimate,
  laneMissed,
  laneSettled,
  laneStateOf,
  laneStateText,
} from "./routing";
import { presenceOf } from "./presence";
import { machineFreshOf, runFresh } from "./fresh";
import { TUNING_FEATURE, cleanTuning, hintFor, storedTuning, type Hint } from "./tuning";
import { cleanStats, telemetry } from "./telemetry";
import {
  BUILD_RE,
  HEX64_RE,
  HttpError,
  INVITE_RE,
  type Json,
  LEASE_RE,
  SLUG_RE,
  bad,
  bool,
  checkKey,
  clamp,
  cmpSemver,
  conflict,
  gone,
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
  uriDecode,
  IN_JSON,
  inJson,
} from "./util";

export const KINDS = ["fuzz", "eval", "selfplay", "diag"] as const;
export const CHANNELS = ["canary", "stable"] as const;
// Rev 5: the first agent that downloads an eval lease's `checkpoints` (older agents know only
// `checkpoint`, so they'd start the worker without its networks).
export const EVAL_MIN_AGENT = "0.2.3";
// Crash kinds (PROTOCOL 3). Rev 6: `definition`, a lease the agent refused because its definition
// didn't verify or didn't match (PROTOCOL 7), reported once.
// `probe_unfinished` (0.2.11): the agent or the PC stopped during a provider probe or a bench, found at the next start.
export const CRASH_KINDS = ["panic", "crash", "agent", "blocked", "definition", "probe_unfinished"] as const;
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
  gpus?: { vendor?: string; name?: string; vram_mb?: number; driver?: string; luid?: string }[];
  providers?: string[]; // rev 7: every provider that passed this PC's probe, preference order, cpu last
  runtime_providers?: Record<string, string[]>; // rev 7: runtime build_id -> providers that passed with it
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
      str(o.luid, `profile.gpus[${i}].luid`, { optional: true, re: LUID_RE }); // rev 7: lowercase hex
    });
  }
  // Provider names this coordinator doesn't know are kept (a newer agent may know more); routing
  // ignores them.
  strList(p.providers, "profile.providers", { optional: true, max: 32, itemMax: 64 });
  if (p.runtime_providers !== undefined && p.runtime_providers !== null) {
    const rp = obj(p.runtime_providers, "profile.runtime_providers");
    const ids = Object.keys(rp);
    if (ids.length > MAX_RUNTIMES) throw bad(`profile.runtime_providers has more than ${MAX_RUNTIMES} runtimes`);
    for (const id of ids) {
      if (!BUILD_RE.test(id)) throw bad(`profile.runtime_providers: ${JSON.stringify(id.slice(0, 64))} is not a runtime build_id`);
      strList(rp[id], `profile.runtime_providers.${id}`, { max: MAX_RUNTIME_PROVIDERS, itemMax: 64 });
    }
  }
  // Availability: hours per day the idle policy let it work / the agent was running (7-day means).
  for (const k of ["idle_h_day", "on_h_day"]) num(p[k], `profile.${k}`, { optional: true, min: 0, max: 24 });
  if (JSON.stringify(p).length > 8192) throw bad("profile is larger than 8 KB");
  return p as Profile;
}

// A machine's hardware class: its CPU and its biggest GPU (most VRAM; the first listed on a tie). On a laptop
// with switchable graphics Windows lists the integrated GPU first, so the first GPU would name the Intel part.
export function classOf(p: Profile | null): string | null {
  if (!p) return null;
  const named = (p.gpus ?? []).filter((g) => g.name);
  const best = named.reduce<(typeof named)[number] | undefined>((b, g) => (!b || (g.vram_mb ?? 0) > (b.vram_mb ?? 0) ? g : b), undefined);
  const gpu = best?.name ?? "no gpu";
  return `${p.cpu ?? "unknown cpu"} / ${gpu}`.slice(0, 200);
}

// Rev 17 (PROTOCOL 11.2): the newest release of (channel, platform); `latest` and `min_agent` are per platform.
async function latestRelease(db: D1Database, channel: string, platform: string) {
  return db
    .prepare(`SELECT * FROM agent_releases WHERE channel = ? AND platform = ? ORDER BY id DESC LIMIT 1`)
    .bind(channel, platform)
    .first<{ version: string; build_id: string; min_agent: string; channel: string }>();
}

// GET /download for a channel and platform (each named only when not the default: stable, windows-x64).
export function downloadUrl(origin: string, channel: string, platform: string): string {
  const q = new URLSearchParams();
  if (platform !== DEFAULT_PLATFORM) q.set("platform", platform);
  if (channel !== "stable") q.set("channel", channel);
  const t = q.toString();
  return `${origin}/download${t ? `?${t}` : ""}`;
}

// ---------- POST /v1/enroll ----------

export async function enroll(c: Ctx): Promise<Response> {
  // Per client IP (Cloudflare always sets cf-connecting-ip; local tools may not). Only an HMAC of
  // the address (its /64 for IPv6) is stored, for minutes (ratelimit.ts).
  const ip = c.req.headers.get("cf-connecting-ip");
  if (ip) await take(c, await ipKey(ip.slice(0, 64), c.env.IP_SALT), "enroll", "enroll");
  const b = await readJson(c.req);
  const invite = str(b.invite, "invite", { max: 32 }).trim().toUpperCase();
  const name = str(b.name, "name", { min: 1, max: 64 }).trim();
  if (!name) throw bad("name is required");
  const profile = cleanProfile(obj(b.profile, "profile"));
  const digest = str(b.profile_digest, "profile_digest", { optional: true, re: HEX64_RE }) ?? null;
  // Rev 17 (PROTOCOL 11.1): the agent's platform (no header: windows-x64), fixed for the machine from now on.
  const platform = requestPlatform(c.req);
  const used = () => new HttpError(403, "unauthorized", "unknown or already used invite code");
  if (!INVITE_RE.test(invite)) throw used();
  const inv = await c.env.DB.prepare(`SELECT * FROM invites WHERE code = ?`).bind(invite).first<{ used_at: number | null; channel: string }>();
  if (!inv || inv.used_at !== null) throw used();

  const agent = c.req.headers.get("x-farm-agent");
  if (agent !== null && isSemver(agent)) {
    const rel = await latestRelease(c.env.DB, inv.channel, platform);
    if (rel && cmpSemver(agent, rel.min_agent) < 0) {
      // An unenrolled agent can't update itself: the friend needs a newer installer (of its own platform).
      const download = downloadUrl(c.url.origin, inv.channel, platform);
      throw new HttpError(426, "update_required", `agent ${agent} is older than ${rel.min_agent}`, { min_agent: rel.min_agent, download });
    }
  }

  const machineId = newMachineId();
  const token = newToken();
  const [claim, insert] = await c.env.DB.batch([
    c.env.DB.prepare(`UPDATE invites SET used_at = ?, machine_id = ? WHERE code = ? AND used_at IS NULL`).bind(c.now, machineId, invite),
    // Inserted only if the UPDATE above won the invite (a concurrent enroll can't use it twice).
    c.env.DB.prepare(
      `INSERT INTO machines (machine_id, token_hash, name, channel, invite, created_at, last_seen, agent_version, profile, profile_digest, class, platform)
       SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM invites WHERE code = ? AND machine_id = ?)`,
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
      platform,
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
  manifest: string;
  platform: string; // rev 17
}

interface Candidate {
  scope: string;
  kind: book.Kind;
  run?: string;
  campaign?: string;
  priority: number;
  tier: number; // 1: a run's games; 2: backfill (fuzz, or a GPU-preferring job on a PC that isn't GPU-eligible); 3 (rev 18): CPU self-play of a run with a GPU version
  fit: number;
  gpu_play?: boolean; // rev 18: self-play this PC plays on its GPU (a GPU lease whose GPU pays here)
  unissued?: number; // rev 18: an eval batch's unissued games
  platforms?: Platform[]; // rev 23: an eval batch's platforms (its run's default build's and alternates'): whose CPU PCs can play it
  deferred?: boolean; // rev 18: an eval batch this GPU self-play PC takes after its self-play (not behind)
  batch?: string; // rev 24: an eval candidate's batch
  soon?: boolean; // rev 24 (PROTOCOL 15.2): an eval batch this machine could play with no unissued id now, leases still on it
  configured: "auto" | number;
  gpu_configured?: "auto" | number; // rev 11: the job's gpu_games_per_lease (absent: none)
  max_stale?: number; // self-play: the run's staleness window in gens
  providers: Provider[] | null; // rev 7: the job's allowlist, sent with the lease
  // Rev 11 (PROTOCOL 9.4): the worker build and runtime the lease carries (a run's default, or an alternate; rev 17: a
  // campaign's build, or its alternate of the machine's platform)
  build_id: string;
  runtime: string | null;
  alt: boolean;
  model?: string; // rev 20 (PROTOCOL 14.3): the model variant the chosen alternate names (book.ts decides per gen)
  gpu_lease: boolean; // an alternate, or a machine GPU-eligible for the job: `auto` leases get GPU_LEASE_MIN
  share?: number; // rev 13: a background job's share of the runs' active leases (PROTOCOL 5.3)
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
    .prepare(`UPDATE diag_requests SET delivered_at = ? WHERE id IN ${IN_JSON}`)
    .bind(now, inJson(results.map((r) => r.id)))
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
  // Rev 11 (PROTOCOL 3, 9.4): GPU work allowed now, whether or not a GPU provider passed a probe yet (agents
  // 0.2.10+; older ones send only `gpu`).
  const gpuAllowed = bool(capsIn.gpu_allowed, "caps.gpu_allowed", { optional: true }) ?? caps.gpu;
  // Rev 11 (PROTOCOL 9.4): per run, the fewest games per lease at which this machine's GPU pays (its bench).
  const gpuGames = new Map<string, number>();
  if (capsIn.gpu_games !== undefined && capsIn.gpu_games !== null) {
    const g = obj(capsIn.gpu_games, "caps.gpu_games");
    if (Object.keys(g).length > 32) throw bad("caps.gpu_games names more than 32 runs");
    for (const [run, n] of Object.entries(g)) {
      if (!SLUG_RE.test(run)) throw bad(`caps.gpu_games: ${JSON.stringify(run.slice(0, 64))} is not a run`);
      gpuGames.set(run, int(n, `caps.gpu_games.${run}`, { min: 0, max: 100000 })); // 0: no GPU pays here
    }
  }
  const cachedIn = obj(b.cached, "cached", { optional: true }) ?? {};
  const cachedBuilds = new Set(strList(cachedIn.builds, "cached.builds", { optional: true, max: 1000, itemMax: 64 }));
  const cachedCkpts = new Set(strList(cachedIn.ckpts, "cached.ckpts", { optional: true, max: 1000, itemMax: 128 }));
  // Kinds this coordinator doesn't know are ignored (a newer agent may offer more).
  const kinds = new Set(strList(b.kinds, "kinds", { max: 32, itemMax: 32 }).filter((k) => (KINDS as readonly string[]).includes(k)));
  // Rev 15 (PROTOCOL 10.2): optional features of the agent; names this coordinator doesn't know are ignored.
  const features = new Set(strList(b.features, "features", { optional: true, max: 32, itemMax: 64 }));
  // Rev 19 (PROTOCOL 13): the CPU layouts the agent tuned (kept per machine; hints come from them).
  const tuning = cleanTuning(b.tuning);
  const agent = c.req.headers.get("x-farm-agent");
  if (!agent || !isSemver(agent)) throw bad("x-farm-agent header (the agent's semver) is required");
  const awHeader = c.req.headers.get("x-farm-aw") ?? "1";
  if (!/^\d{1,4}$/.test(awHeader)) throw bad("x-farm-aw must be an integer");
  const aw = Number(awHeader);
  const cfg = config(c.env);

  // Profile: the digest is the agent's; we only compare it with the last one.
  if (digest !== m.profile_digest) {
    if (!profile) throw conflict("unknown profile digest: send the profile with it", { need_profile: true });
    await db
      .prepare(`UPDATE machines SET profile = ?, profile_digest = ?, class = ? WHERE machine_id = ?`)
      .bind(JSON.stringify(profile), digest, classOf(profile), m.machine_id)
      .run();
    m.class = classOf(profile);
  }
  // Rev 18 (PROTOCOL 12.2): the stored caps also keep `gpu_allowed` and `gpu_games`, so a rolling lease's renewal
  // (otherWorkWaits) sees the same GPU self-play this request does (an agent may send gpu false, gpu_allowed true).
  const capsJson = JSON.stringify({ ...caps, gpu_allowed: gpuAllowed, ...(gpuGames.size ? { gpu_games: Object.fromEntries(gpuGames) } : {}) });
  if (capsJson !== m.caps) await db.prepare(`UPDATE machines SET caps = ? WHERE machine_id = ?`).bind(capsJson, m.machine_id).run();
  // Rev 29 (PROTOCOL 15.8): and the kinds and cached builds it offers, so the lane (15.3, 15.4) and the renewal decide
  // with the candidates this request sees (before rev 29 they assumed every kind and nothing cached: a PC whose free disk
  // was below its CUDA pack had no GPU self-play in the lane's eyes, so a batch the grant defers looked like its next
  // job, and the lane finished its self-play for nothing, lease after lease). An ask that offers no kind at all says
  // nothing about the work the machine takes: the stored one stays.
  const askJson = JSON.stringify({ kinds: [...kinds].sort(), builds: [...cachedBuilds].sort().slice(0, ASK_BUILDS_MAX) });
  if (kinds.size && askJson !== m.ask) await db.prepare(`UPDATE machines SET ask = ? WHERE machine_id = ?`).bind(askJson, m.machine_id).run();
  if (tuning && tuning.length) {
    const t = JSON.stringify(tuning);
    if (t !== m.tuning) await db.prepare(`UPDATE machines SET tuning = ?, tuning_t = ? WHERE machine_id = ?`).bind(t, c.now, m.machine_id).run();
  }
  // Rev 7 routing reads the profile the coordinator keeps: the one just sent, else the stored one.
  const machineProfile = profile && digest !== m.profile_digest ? profile : parseJson<Profile | null>(m.profile, null);
  const o = await offer(c, m, { caps, gpuAllowed, gpuGames, cachedBuilds, cachedCkpts, kinds, agent, aw }, machineProfile);
  if (o.need && cmpSemver(agent, o.need) < 0) return json({ update: { min_agent: o.need } });

  const uploadDiag = await takeUploadDiag(db, m, c.now);
  // Rev 28 (PROTOCOL 20.7): every answer below tells an agent from DISK_CAP_AGENT the owner's disk cap for this machine
  // (null: none), which it uses in place of its own setting while it is set - so its next ask's caps.disk_mb follows it
  const diskCap = cmpSemver(agent, cfg.diskCapAgent) >= 0 ? { owner_disk_cap_mb: m.disk_cap_mb ?? null } : {};
  const withDiag = (x: Record<string, unknown>) => json(uploadDiag ? { ...x, ...diskCap, upload_diag: uploadDiag } : { ...x, ...diskCap });

  if (o.farmPaused || m.paused) return withDiag({ wait_s: WAIT_S, reason: "paused_by_admin" });
  // Review M4: one machine can't hoard work (an agent runs one lease at a time; one more covers a
  // lease it is still closing).
  if (o.mine >= cfg.maxActiveLeases) return withDiag({ wait_s: WAIT_S, reason: "too_many_leases" });

  // --- diagnostics asked for this machine come first (legacy agents only: PROTOCOL 7) ---
  if (kinds.has("diag") && !o.defAware) {
    for (const id of o.diag) {
      const l = unwrap(
        await bookFor(c.env, book.DIAG_SCOPE).grant({
          now: c.now,
          machine_id: m.machine_id,
          kind: "diag",
          want: 0,
          diag_id: id,
          def_aware: o.defAware,
          platform: m.platform,
        }),
      );
      if (l) return withDiag({ lease: { ...l, providers: null } });
    }
  }

  // Rev 15 (PROTOCOL 10.2): this agent takes rolling self-play leases (a run with `selfplay.rolling` then gives one).
  const rolling = o.defAware && features.has("rolling") && cmpSemver(agent, cfg.rollingAgent) >= 0;
  // Rev 26 (PROTOCOL 18): and is sent the run's freshness budget with it (rev 28, PROTOCOL 20.3: this machine's own, with
  // the owner's `share`) and, rev 28 (20.2), the owner's `poll_s` for this machine
  const fresh = rolling && freshFor(cfg.freshAgent, agent, o.hw, m);
  const mf = machineFreshOf(m.fresh);
  const pollS = rolling && cmpSemver(agent, cfg.pollAgent) >= 0 ? mf.poll_s : undefined;
  // --- then runs' games by priority and fit, then backfill; share by active leases (offer's order) ---
  for (const cd of o.usable) {
    const want = await wantFor(db, o, cd, caps, gpuGames);
    const l = unwrap(
      await bookFor(c.env, cd.scope).grant({
        now: c.now,
        machine_id: m.machine_id,
        kind: cd.kind,
        want,
        run: cd.run,
        campaign: cd.campaign,
        def_aware: o.defAware,
        platform: m.platform, // rev 17: checked again under the scope's lock
        ...(cd.alt ? { alt: { build_id: cd.build_id, runtime: cd.runtime } } : {}),
        // rev 20 (PROTOCOL 14.3): this agent takes model variants
        ...(cmpSemver(agent, cfg.variantAgent) >= 0 ? { variants: true } : {}),
        ...(rolling && cd.kind === "selfplay" ? { rolling: true } : {}),
        ...(fresh && cd.kind === "selfplay" ? { fresh: true, ...(mf.share ? { fresh_share: mf.share } : {}) } : {}),
        ...(pollS && cd.kind === "selfplay" ? { poll_s: pollS } : {}),
      }),
    );
    // Rev 7: the job's provider allowlist rides along (a routing hint, not part of the signed job). Rev 19: so does
    // the best layout this hardware class reported for a rolling self-play lease's build and runtime.
    if (l) {
      await laneSettle(c, m, cd.kind === "eval");
      const hint = (l as any).rolling && features.has(TUNING_FEATURE) ? await tuningHint(db, m, l as any) : null;
      return withDiag({ lease: { ...l, providers: cd.providers, ...(hint ? { tuning_hint: hint } : {}) } });
    }
  }
  await laneSettle(c, m, false);
  // Rev 24 (PROTOCOL 15.2): eval work this machine could play is about to need it (ids may come back to a batch it was
  // offered, or a batch whose leases are still out, or a run being set up): it asks again sooner.
  const soon = o.hw.priority_lane !== "off" && (o.soon || o.usable.some((cd) => cd.kind === "eval"));
  return withDiag({ wait_s: soon ? LANE_WAIT_S : WAIT_S, reason: "no_work" });
}

// Rev 29 (PROTOCOL 15.8): the first lease request after a lane yield settles it - once the lease that gave way has closed
// (an agent asks while it still finishes a lease only to fill a second slot). Given eval work, the yield did its job;
// given anything else, or nothing, the batch is a miss for this machine (laneTarget skips it for LANE_MISS_S).
async function laneSettle(c: Ctx, m: MachineRow, gotEval: boolean): Promise<void> {
  const st = laneStateOf(m.lane);
  if (!st.yield) return;
  const db = c.env.DB;
  const draining = await db
    .prepare(`SELECT 1 FROM leases WHERE machine_id = ? AND state = 'active' AND expires_at >= ? AND pending LIKE 'yield:%' LIMIT 1`)
    .bind(m.machine_id, c.now)
    .first();
  if (draining) return;
  const text = laneStateText(laneSettled(st, gotEval, c.now), c.now);
  await db.prepare(`UPDATE machines SET lane = ? WHERE machine_id = ?`).bind(text, m.machine_id).run();
  m.lane = text;
}

// Rev 29 (PROTOCOL 15.8): the lease of machine `m` now gives way to the eval batch of `scope` (the book marked it
// `yield:<scope>`): remembered with the batch, for the next lease request to settle.
async function laneMarkYield(c: Ctx, m: MachineRow, scope: string): Promise<void> {
  const db = c.env.DB;
  const r = scope.startsWith("run:") ? await book.getRun(db, scope.slice(4)) : null;
  const st = laneStateOf(m.lane);
  const text = laneStateText({ ...st, yield: { scope, batch: r?.eval_batch ?? null, t: c.now } }, c.now);
  await db.prepare(`UPDATE machines SET lane = ? WHERE machine_id = ?`).bind(text, m.machine_id).run();
  m.lane = text;
}

// The ids a lease of candidate `cd` asks for on this machine (PROTOCOL 3: about TARGET_S of its measured rate; rev 11's
// GPU lease size; rev 18's CPU eval floor). The grant may give fewer (the pool or the headroom has fewer).
async function wantFor(db: D1Database, o: Offer, cd: Candidate, caps: Caps, gpuGames: Map<string, number>): Promise<number> {
  const key = `${cd.scope}|${cd.kind}`;
  let want = leaseSize(cd.kind as Exclude<book.Kind, "diag">, cd.configured, o.rate.get(key) ?? o.classRate.get(key) ?? null);
  // Rev 11 (PROTOCOL 9.4): a GPU lease gets enough games for big batches.
  if (cd.gpu_lease && cd.run && (cd.kind === "selfplay" || cd.kind === "eval")) {
    want = await gpuLeaseSize(db, cd, want, o.rate.get(key) ?? null, gpuGames.get(cd.run));
  } else if (cd.kind === "eval" && cd.configured === "auto") {
    // Rev 18 (PROTOCOL 12.3): an eval lease takes about as long as its slowest game, so a CPU lease of a few games
    // measures a low rate and stays small; enough games keep every thread busy.
    want = Math.max(want, evalCpuFloor(o.hw.eval_games_per_thread, caps.threads, caps.ram_mb, SIZING.eval.min, SIZING.eval.max));
  }
  return want;
}

// Rev 19 (PROTOCOL 13): the best layout the machines of `m`'s hardware class and platform reported for the lease's
// build and runtime (its own report included: a machine whose agent lost its results starts from it), or null.
async function tuningHint(db: D1Database, m: MachineRow, l: { build: { build_id: string }; runtime: { build_id: string } | null; run?: string | null }): Promise<Hint | null> {
  if (!m.class) return null;
  const { results } = await db
    .prepare(`SELECT machine_id, tuning FROM machines WHERE class = ? AND platform = ? AND tuning IS NOT NULL AND revoked_at IS NULL`)
    .bind(m.class, m.platform)
    .all<{ machine_id: string; tuning: string }>();
  const reports = results.map((r) => ({ machine_id: r.machine_id, reports: storedTuning(r.tuning) }));
  return hintFor(reports, l.build.build_id, l.runtime?.build_id ?? null, l.run ?? null);
}

// What a machine asks with (POST /v1/lease); for an extension's renewal (rev 15), what the coordinator last stored.
interface Ask {
  caps: Caps;
  gpuAllowed: boolean;
  gpuGames: Map<string, number>;
  cachedBuilds: Set<string>;
  cachedCkpts: Set<string>;
  kinds: Set<string>;
  agent: string;
  aw: number;
}

// The work a machine could get now, in the order PROTOCOL 3 picks it (lease(), and rev 15's renewal decision).
interface Offer {
  usable: Candidate[]; // sorted: tier, rank, score
  need: string | null; // an agent version some job needs that the machine's channel offers
  defAware: boolean;
  farmPaused: boolean;
  mine: number; // the machine's live leases
  diag: number[]; // diagnostics requests for it (diag leases)
  rank: (cd: Candidate) => number;
  score: (cd: Candidate) => number;
  rate: Map<string, number>;
  classRate: Map<string, number>;
  hw: HwRouting; // rev 18: the owner's hardware-routing setting
  // Rev 24 (PROTOCOL 15): eval work is about to need this machine (15.2: a batch it could play with leases still out, or
  // a run being set up); the active leases per "scope|kind", the runs' active self-play and eval leases, and the scopes
  // of background jobs (a run job with a `share`).
  soon: boolean;
  active: Map<string, number>;
  activeRuns: number;
  bgScopes: Set<string>;
}

// Rev 18 (PROTOCOL 12.2): the eval rate (games per second, summed) of the PCs that would play an eval batch on the CPU
// and are online and free to work: not playing self-play on a GPU (the `gpu_play` their last offer recorded), seen in
// the last ONLINE_S, not paused or revoked, no fresh presence (in use by their owner). Each counts its measured rate for
// this batch, else its average over every eval it played, else nothing (unknown: the batch counts as behind sooner).
// Rev 23: only PCs of a platform the run has a build for (its default's, its alternates') - a Mac can't play a batch
// of a run without a macOS build, so it is no capacity for it.
export const ONLINE_S = 300;
// Rev 24 (PROTOCOL 15.5): `lane` (the owner's priority_lane, given when the lane applies to the asking PC): a PC that holds
// a live fuzz lease counts only when the lane applies to it too (its fuzz is given back for the batch, 15.3); otherwise
// it is busy for the rest of that lease (a campaign's first fuzz lease, at the 20,000-seed default, ran 56 minutes).
async function cpuEvalRate(db: D1Database, scope: string, self: string, now: number, platforms: Platform[], lane: HwRouting["priority_lane"] | null = null): Promise<number> {
  if (!platforms.length) return 0;
  const busy =
    lane === null
      ? ""
      : `AND NOT EXISTS (SELECT 1 FROM leases l WHERE l.machine_id = m.machine_id AND l.state = 'active' AND l.kind = 'fuzz' AND l.expires_at >= ?5
           AND NOT (?6 = 'all' OR (?6 = 'canary' AND m.channel = 'canary')))`;
  const { results } = await db
    .prepare(
      `SELECT m.last_seen, m.presence, m.presence_t,
         (SELECT r.rate FROM rates r WHERE r.machine_id = m.machine_id AND r.kind = 'eval' AND r.scope = ?1) AS own,
         (SELECT AVG(r.rate) FROM rates r WHERE r.machine_id = m.machine_id AND r.kind = 'eval') AS any_rate
       FROM machines m
       WHERE m.machine_id != ?2 AND COALESCE(m.gpu_play, 0) = 0 AND m.revoked_at IS NULL AND m.paused = 0 AND m.last_seen >= ?3
         AND m.platform IN (SELECT value FROM json_each(?4)) ${busy}`,
    )
    .bind(scope, self, now - ONLINE_S, inJson(platforms), ...(lane === null ? [] : [now, lane]))
    .all<{ last_seen: number | null; presence: string | null; presence_t: number | null; own: number | null; any_rate: number | null }>();
  return results.filter((x) => !presenceOf(x, now)).reduce((s, x) => s + (x.own ?? x.any_rate ?? 0), 0);
}

// Rev 24 (PROTOCOL 15.5): an eval batch nobody has taken or worked on for `stallS` seconds: older than that, no lease of
// it active, none issued in that time (the CPU PCs counted as its capacity are not coming).
async function batchStalled(db: D1Database, run: string, batch: string, stallS: number, now: number): Promise<boolean> {
  const scope = book.runScope(run);
  const pool = `eval:${batch}`;
  const row = await db
    .prepare(
      `SELECT b.created_at,
         EXISTS (SELECT 1 FROM leases l WHERE l.scope = ?1 AND l.state = 'active' AND l.pool = ?2) AS working,
         EXISTS (SELECT 1 FROM leases l WHERE l.issued_at >= ?3 AND l.scope = ?1 AND l.pool = ?2) AS taken
       FROM eval_batches b WHERE b.run = ?4 AND b.batch = ?5`,
    )
    .bind(scope, pool, now - stallS, run, batch)
    .first<{ created_at: number; working: number; taken: number }>();
  return !!row && row.created_at <= now - stallS && !row.working && !row.taken;
}

// Rev 29 (PROTOCOL 15.8): the lane's view of a machine's order: its own lease `closing` (not counted: the order once it has
// closed) and the machines on their way to each batch (`transit`, scope -> leases marked `yield:<scope>`) counted as the
// batch's leases. A lease request passes none: the order is section 3's.
interface OfferView {
  closing?: Pick<book.LeaseRow, "scope" | "kind">;
  transit?: Map<string, number>;
}

async function offer(c: Ctx, m: MachineRow, a: Ask, machineProfile: Profile | null, view: OfferView = {}): Promise<Offer> {
  const db = c.env.DB;
  const { caps, gpuAllowed, cachedBuilds, cachedCkpts, kinds, agent, aw } = a;
  const cfg = config(c.env);
  // Rev 6 (PROTOCOL 7): this agent verifies definitions, so it gets leases only with one.
  const defAware = cmpSemver(agent, cfg.definitionAgent) >= 0;
  // Rev 11 (PROTOCOL 9): this agent takes run alternates and packs with a GPU requirement.
  const altAware = defAware && cmpSemver(agent, cfg.altRuntimeAgent) >= 0;
  // Rev 20 (PROTOCOL 14): this agent takes model variants (and can read a definition that signs them).
  const variantAware = defAware && cmpSemver(agent, cfg.variantAgent) >= 0;
  // Rev 22 (PROTOCOL 9.8): this agent reports AMD adapters and runs only DirectML on them: it may get AMD alternates.
  const amdAware = altAware && cmpSemver(agent, cfg.amdDmlAgent) >= 0;
  // Rev 27 (PROTOCOL 19): this agent takes opt-in runtime packs (manifest `opt_in`, provider torch-cuda), and the opt-in
  // names the owner allowed on this machine. An opt-in pack - an alternate's (`when.opt_in` is its name) or a run's
  // default - goes only to such an agent on a machine whose list holds its name.
  const torchAware = altAware && cmpSemver(agent, cfg.torchAgent) >= 0;
  const optIns = machineOptIns(m.opt_in);
  const optInOk = (name: string | null | undefined) => !name || (torchAware && optIns.has(name));
  // Where a job goes on this machine (PROTOCOL 8): GPU-eligible when it may use the GPU and
  // verified a provider the job allows, with the job's runtime pack if the profile says.
  const placeFor = (kind: "selfplay" | "eval" | "fuzz", r: Routing, runtime: string | null) =>
    place(kind, r, gpuEligible(caps.gpu, verifiedProviders(machineProfile, runtime), r.providers), caps.gpu);
  // Rev 17 (PROTOCOL 11): the machine's platform. Every build it is offered is of it; its channel's releases are too.
  const platform: Platform = storedPlatform(m.platform);

  const [runsR, campsR, releaseR, pausedR, activeR, poolR, ratesR, classR, diagR, mineR, hwR, offR] = await db.batch<any>([
    db.prepare(`SELECT * FROM runs WHERE state = 'active'`),
    db.prepare(`SELECT * FROM campaigns WHERE state = 'active'`),
    db.prepare(`SELECT * FROM agent_releases WHERE channel = ? AND platform = ? ORDER BY id DESC LIMIT 1`).bind(m.channel, platform),
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
    // This machine's live leases (an expired one not swept yet doesn't count).
    db.prepare(`SELECT COUNT(*) AS n FROM leases WHERE machine_id = ? AND state = 'active' AND expires_at >= ?`).bind(m.machine_id, c.now),
    db.prepare(`SELECT value FROM settings WHERE key = 'hw_routing'`),
    db.prepare(`SELECT value FROM settings WHERE key = 'gpu_vendors_off'`),
  ]);
  const hw = hwRoutingOf(hwR.results[0]?.value ?? null);
  // Rev 22 (PROTOCOL 9.8): the owner's kill switch - no alternate (nor a default pack) for these vendors' GPUs is offered
  const vendorsOff = vendorsOffOf(offR.results[0]?.value ?? null);
  // Rev 22: a vendor with a provider restriction (amd: DirectML only) only for agents that keep it, and only with a pack
  // that keeps it (`gpu` naming the vendor and exactly its providers, no Windows ML catalog: vendorPackProblem)
  const vendorOk = (vendor: string, packManifest: string | null | undefined) =>
    !vendorsOff.has(vendor) && (!vendorRestricted(vendor) || (amdAware && vendorPackProblem(vendor, packManifest) === null));
  const defs = await currentDefs(db);
  const runs = runsR.results as book.RunRow[];
  const camps = campsR.results as book.CampaignRow[];
  const release = (releaseR.results[0] ?? null) as { version: string; min_agent: string } | null;

  const altsOf = (r: book.RunRow) => parseJson<Alternate[] | null>(r.alternates ?? null, null) ?? [];
  const campAltsOf = (x: book.CampaignRow) => parseJson<CampaignAlternate[] | null>(x.alternates ?? null, null) ?? [];
  const buildIds = [
    ...new Set(
      [
        ...runs.flatMap((r) => [r.build_id, r.runtime, ...altsOf(r).flatMap((a) => [a.build_id, a.runtime])]),
        ...camps.flatMap((x) => [x.build_id, ...campAltsOf(x).map((a) => a.build_id)]),
      ].filter(Boolean),
    ),
  ] as string[];
  const builds = new Map<string, BuildInfo>();
  if (buildIds.length) {
    const { results } = await db
      .prepare(`SELECT build_id, kind, min_agent, aw, size, manifest, platform FROM builds WHERE build_id IN (${buildIds.map(() => "?").join(",")})`)
      .bind(...buildIds)
      .all<BuildInfo>();
    for (const x of results) builds.set(x.build_id, x);
  }
  // --- builds Windows refused to start on this machine (rev 5): not offered to it for a while ---
  const blocked = await blockedBuilds(db, m.machine_id, agent, c.now);

  // --- candidates: kinds the agent offers, that fit its caps ---
  const cands: Candidate[] = [];
  const poolN = new Map<string, number>((poolR.results as any[]).map((p) => [`${p.scope}|${p.pool}`, p.n]));
  const needsOk = (n: book.Needs | null | undefined) => !n || (caps.threads >= n.threads && caps.ram_mb >= n.ram_mb);
  const diskOk = (ids: (string | null)[], extra = 0) => {
    const need = ids.reduce((s, id) => s + (id && !cachedBuilds.has(id) ? (builds.get(id)?.size ?? 0) : 0), extra);
    return need <= caps.disk_mb * 1024 * 1024;
  };
  // Rev 11 (PROTOCOL 9.2): a runtime pack with a GPU requirement goes only to agents that read it, on a machine
  // with a matching adapter (default and alternate alike).
  const needOf = (id: string | null) => (id ? gpuNeedOf(builds.get(id)?.manifest) : null);
  const packFits = (id: string | null) => {
    const need = needOf(id);
    // (rev 27: an opt-in pack only where it is opted in)
    if (!optInOk(optInOf(id ? builds.get(id)?.manifest : null))) return false;
    return !need || (altAware && vendorOk(need.vendor, builds.get(id!)?.manifest) && hasAdapter(machineProfile, need.vendor, [need.min_driver]));
  };
  // Rev 17 (PROTOCOL 11.4): a build's platform (one not registered counts as windows-x64; such a candidate is dropped
  // below anyway); no runtime fits every platform.
  const platOf = (id: string | null) => (id ? storedPlatform(builds.get(id)?.platform) : platform);
  // The build and runtime a run's job lease carries here (PROTOCOL 9.4, rev 17 11.4): the first alternate of this
  // machine's platform it qualifies for, else the default when it is of this machine's platform (null: nothing from
  // this run for this machine). A GPU alternate (a `vendor`) needs 9.4's conditions 1-7; a CPU alternate (no vendor)
  // conditions 5-7 and the build's `aw`. One that only its `min_agent` keeps from this agent is held: when the default
  // isn't of this platform it is the run's build here, so section 3's update rule asks for that agent (as it would for
  // a default build) instead of the run giving this machine nothing without a word.
  type Choice = { build_id: string; runtime: string | null; alt: boolean; eligible: boolean; gpuLease: boolean; model?: string };
  const choiceFor = (r: book.RunRow, kind: "selfplay" | "eval", routing: Routing): Choice | null => {
    let held: Choice | null = null;
    if (altAware) {
      for (const a of altsOf(r)) {
        if (altPlatform(a) !== platform) continue;
        const vendor = a.when.vendor || null; // null: a CPU alternate
        if (vendor && !gpuAllowed) continue; // 1
        const bi = builds.get(a.build_id);
        const ri = builds.get(a.runtime);
        if (!bi || !ri || bi.kind !== "worker" || ri.kind !== "runtime") continue; // 6: registered
        if (platOf(a.build_id) !== platform || platOf(a.runtime) !== platform) continue;
        if (requiresRuntimeOf(bi.manifest) !== a.runtime) continue;
        // rev 27 (PROTOCOL 19): an opt-in alternate (and any opt-in pack) only to an agent from TORCH_AGENT on a machine
        // the owner opted in
        if (!optInOk(a.when.opt_in) || !optInOk(optInOf(ri.manifest))) continue;
        const need = gpuNeedOf(ri.manifest);
        let eligible = true;
        if (!vendor) {
          if (need) continue; // a CPU alternate's pack has no GPU requirement (checked at PUT too)
          eligible = gpuEligible(caps.gpu, verifiedProviders(machineProfile, a.runtime), routing.providers);
        } else {
          if (need && need.vendor !== vendor) continue;
          // rev 22 (PROTOCOL 9.8): not a vendor the owner turned off; amd only to agents from AMD_DML_AGENT, with a
          // DirectML-only pack (an AMD alternate stored before rev 12 has none)
          if (!vendorOk(vendor, ri.manifest)) continue;
          if (!hasAdapter(machineProfile, vendor, [a.when.min_driver, need?.min_driver])) continue; // 2
          const allowed = altProviders(need, routing.providers, vendor); // 3
          if (!allowed.length) continue;
          const rp = runtimeProvidersOf(machineProfile, a.runtime); // 4: never probed here, or a provider passed
          if (rp !== undefined && !rp.some((p) => (allowed as string[]).includes(p))) continue;
        }
        if (!diskOk([a.build_id, a.runtime])) continue; // 5
        if (blocked.has(a.build_id) || blocked.has(a.runtime)) continue; // 6
        if (bi.aw !== null && bi.aw !== aw) continue;
        const def = defs.get(`run:${r.run}`); // 7: the definition authorizes it
        if (!def || leaseProblem(def, kind, a.build_id, a.runtime, c.now, a.runtime)) continue;
        const v: Choice = { build_id: a.build_id, runtime: a.runtime, alt: true, eligible, gpuLease: !!vendor, ...(a.model ? { model: a.model } : {}) };
        if ([bi.min_agent, ri.min_agent].some((x) => x && !minAgentOk(agent, x))) {
          held ??= v; // 6: min_agent
          continue;
        }
        return v;
      }
    }
    // Rev 17: the default only for a machine of its platform (a Mac never gets a Windows build, nor the reverse).
    if (platOf(r.build_id) !== platform || platOf(r.runtime) !== platform) return held;
    if (!packFits(r.runtime)) return null;
    const eligible = gpuEligible(caps.gpu, verifiedProviders(machineProfile, r.runtime), routing.providers);
    // a GPU lease (its own size, 9.4) only when a GPU provider passed with this very pack here
    const withPack = r.runtime ? (runtimeProvidersOf(machineProfile, r.runtime) ?? []) : [];
    const gpuLease = eligible && withPack.some((p) => isGpuProvider(p) && (!routing.providers || (routing.providers as string[]).includes(p)));
    return { build_id: r.build_id, runtime: r.runtime, alt: false, eligible, gpuLease };
  };
  // Rev 17 (PROTOCOL 11.4): the worker build a campaign's lease carries here: its alternate of this machine's platform
  // (registered, not blocked here, `aw` and `min_agent` met, in the campaign's current definition's `builds`), else its
  // build when that is of this machine's platform (null: nothing from this campaign for this machine). An alternate
  // that only its `min_agent` keeps from this agent is held, as for runs.
  const campaignBuildFor = (x: book.CampaignRow): { build_id: string; alt: boolean } | null => {
    let held: { build_id: string; alt: boolean } | null = null;
    for (const a of campAltsOf(x)) {
      if (storedPlatform(a.when?.platform) !== platform) continue;
      const bi = builds.get(a.build_id);
      if (!bi || bi.kind !== "worker" || platOf(a.build_id) !== platform) continue;
      if (blocked.has(a.build_id)) continue;
      if (bi.aw !== null && bi.aw !== aw) continue;
      const def = defs.get(`campaign:${x.campaign}`);
      if (!def || leaseProblem(def, "fuzz", a.build_id, null, c.now)) continue;
      if (!minAgentOk(agent, bi.min_agent)) {
        held ??= { build_id: a.build_id, alt: true };
        continue;
      }
      return { build_id: a.build_id, alt: true };
    }
    if (platOf(x.build_id) === platform) return { build_id: x.build_id, alt: false };
    return held;
  };
  // Rev 18 (PROTOCOL 12.1): a run has a GPU version when it lists an alternate for a GPU vendor the farm uses, or its
  // default runtime pack requires a GPU (rev 17: a CPU alternate, without `vendor`, is not one; a default pack that only may use one, DirectML, counts by its alternates).
  // Rev 22: a vendor the owner turned off doesn't count; an AMD alternate counts when its pack is DirectML-only (whoever
  // asks: the run has that GPU version for the farm, not just for this agent).
  const usedVendor = (vendor: string, packManifest: string | null | undefined) =>
    !vendorsOff.has(vendor) && (!vendorRestricted(vendor) || vendorPackProblem(vendor, packManifest) === null);
  // Rev 27 (PROTOCOL 19): an opt-in alternate is not the farm's GPU version (it serves the opted-in machines only).
  const hasGpuVersion = (r: book.RunRow) => {
    const need = needOf(r.runtime);
    return (
      altsOf(r).some((x) => !!x.when.vendor && !x.when.opt_in && usedVendor(x.when.vendor, builds.get(x.runtime)?.manifest)) ||
      (need !== null && usedVendor(need.vendor, builds.get(r.runtime!)?.manifest))
    );
  };
  const active = new Map<string, number>((activeR.results as any[]).map((x) => [`${x.scope}|${x.kind}`, x.n]));
  // Rev 29 (PROTOCOL 15.8): the lane's view - the order once the machine's own lease has closed (not counted)
  if (view.closing) {
    const k = `${view.closing.scope}|${view.closing.kind}`;
    active.set(k, Math.max(0, (active.get(k) ?? 0) - 1));
  }
  // Rev 24 (PROTOCOL 15): the lane applies to this machine; background jobs' scopes (a run job with a `share`, 5.3).
  const lane = laneApplies(hw, m.channel);
  const bgScopes = new Set<string>();
  // Rev 24 (15.2): a run being set up that this machine could play (no self-play, no batch, put within LANE_SOON_S).
  let setupSoon = false;
  for (const r of runs) {
    const scope = book.runScope(r.run);
    const spCfg = parseJson<book.SelfplayCfg | null>(r.selfplay, null);
    const evCfg = parseJson<book.EvalCfg | null>(r.eval, null);
    if (typeof spCfg?.share === "number" || typeof evCfg?.share === "number") bgScopes.add(scope);
    if (kinds.has("eval") && !spCfg && !evCfg && c.now - r.updated_at <= LANE_SOON_S) {
      setupSoon ||= [platOf(r.build_id), ...altsOf(r).map((x) => altPlatform(x))].includes(platform);
    }
    if (kinds.has("selfplay") && r.selfplay && r.current_gen !== null) {
      const sp = JSON.parse(r.selfplay) as book.SelfplayCfg;
      const routing = sp ? storedRouting(sp.gpu, sp.providers) : null;
      const v = routing ? choiceFor(r, "selfplay", routing) : null;
      const at = routing && v ? place("selfplay", routing, v.eligible, caps.gpu) : null;
      if (sp && routing && v && at && needsOk(sp.needs) && diskOk([v.build_id, v.runtime])) {
        // rev 20: a lease of an alternate naming a variant brings that file (agents from 0.3.5 list it as run/gen/name)
        const ckptKey = v.model && variantAware ? `${r.run}/${r.current_gen}/${v.model}` : `${r.run}/${r.current_gen}`;
        const ckptBonus = cachedCkpts.has(ckptKey) || cachedCkpts.has(`${r.run}/${r.current_gen}`) ? 1.1 : 1;
        // Rev 18 (PROTOCOL 12.1): played on this PC's GPU (a GPU lease, and the agent's bench didn't say no GPU pays
        // for this run here); else, while the run has a GPU version, this PC's self-play is the last resort.
        const gpuPlay = altAware && v.gpuLease && a.gpuGames.get(r.run) !== 0;
        const cpuBackfill = hw.on && !gpuPlay && hasGpuVersion(r);
        cands.push({
          scope,
          kind: "selfplay",
          run: r.run,
          priority: r.priority,
          tier: cpuBackfill ? CPU_SELFPLAY_TIER : at.tier,
          gpu_play: gpuPlay,
          fit: at.fit * (cachedBuilds.has(v.build_id) ? 1.2 : 1) * ckptBonus,
          configured: sp.games_per_lease,
          gpu_configured: sp.gpu_games_per_lease,
          max_stale: sp.max_stale,
          providers: routing.providers,
          build_id: v.build_id,
          runtime: v.runtime,
          alt: v.alt,
          ...(v.model ? { model: v.model } : {}),
          gpu_lease: altAware && v.gpuLease,
          ...(typeof sp.share === "number" ? { share: sp.share } : {}),
        });
      }
    }
    if (kinds.has("eval") && r.eval) {
      const ev = JSON.parse(r.eval) as book.EvalCfg | null;
      const routing = ev ? storedRouting(ev.gpu, ev.providers) : null;
      const v = routing ? choiceFor(r, "eval", routing) : null;
      const at = routing && v ? place("eval", routing, v.eligible, caps.gpu) : null;
      const unissued = ev ? (poolN.get(`${scope}|eval:${ev.batch}`) ?? 0) : 0;
      // Rev 24 (PROTOCOL 15.2): a batch with no unissued id whose leases are still out (ids may come back) is a candidate
      // only to say this machine could play it (`soon`: never offered, never asks for an update).
      const soon = hw.priority_lane !== "off" && unissued === 0 && (active.get(`${scope}|eval`) ?? 0) > 0;
      if (ev && routing && v && at && Array.isArray(ev.checkpoints) && (unissued > 0 || soon) && needsOk(ev.needs)) {
        // Rev 5: the lease brings every network the seats name; uncached ones count against the disk. Rev 20: a gen's
        // variant instead (its own entry and size) when the alternate names one and the gen has it registered.
        const vname = v.model && variantAware ? v.model : null;
        const keys = ev.checkpoints.map((g) => `${r.run}/${g}`);
        let ckptBytes = 0;
        let ckptKeys = keys;
        if (vname || keys.some((k) => !cachedCkpts.has(k))) {
          const { results } = await db
            .prepare(`SELECT gen, size, variants FROM checkpoints WHERE run = ? AND gen IN (${ev.checkpoints.map(() => "?").join(",")})`)
            .bind(r.run, ...ev.checkpoints)
            .all<{ gen: number; size: number | null; variants: string | null }>();
          const fileOf = (x: { gen: number; size: number | null; variants: string | null }) => {
            const vf = vname ? book.variantsOf(x)[vname] : undefined;
            return vf ? { key: `${r.run}/${x.gen}/${vname}`, size: vf.size ?? 0 } : { key: `${r.run}/${x.gen}`, size: x.size ?? 0 };
          };
          ckptKeys = results.map((x) => fileOf(x).key);
          ckptBytes = results.reduce((s, x) => s + (cachedCkpts.has(fileOf(x).key) ? 0 : fileOf(x).size), 0);
        }
        if (diskOk([v.build_id, v.runtime], ckptBytes)) {
          const ckptBonus = ckptKeys.length && ckptKeys.every((k) => cachedCkpts.has(k)) ? 1.1 : 1;
          cands.push({
            scope,
            kind: "eval",
            run: r.run,
            priority: r.priority,
            tier: at.tier,
            fit: at.fit * (cachedBuilds.has(v.build_id) ? 1.2 : 1) * ckptBonus,
            // (a batch that requires a GPU has no CPU PCs to wait for: never deferred)
            ...(routing.gpu === "required" ? {} : { unissued }),
            platforms: [...new Set([platOf(r.build_id), ...altsOf(r).map((a) => altPlatform(a))])],
            batch: ev.batch,
            ...(soon ? { soon: true } : {}),
            configured: ev.games_per_lease,
            gpu_configured: ev.gpu_games_per_lease,
            providers: routing.providers,
            build_id: v.build_id,
            runtime: v.runtime,
            alt: v.alt,
            ...(v.model ? { model: v.model } : {}),
            gpu_lease: altAware && v.gpuLease,
            ...(typeof ev.share === "number" ? { share: ev.share } : {}),
          });
        }
      }
    }
  }
  if (kinds.has("fuzz")) {
    for (const x of camps) {
      const needs = parseJson<book.Needs | null>(x.needs, null);
      // A campaign has no runtime pack: its machines' verified providers are their union.
      const routing = storedRouting(x.gpu, parseJson<unknown>(x.providers, null));
      const at = placeFor("fuzz", routing, null);
      const v = campaignBuildFor(x); // rev 17
      if (!v || !at || !needsOk(needs) || !diskOk([v.build_id])) continue;
      // Rev 14 (PROTOCOL 5.4): a campaign with a share competes with the runs' games (tier 1) within that share.
      const share = typeof x.share === "number" ? x.share : undefined;
      cands.push({
        scope: book.campaignScope(x.campaign),
        kind: "fuzz",
        campaign: x.campaign,
        priority: x.priority,
        tier: share === undefined ? at.tier : 1,
        fit: at.fit * (cachedBuilds.has(v.build_id) ? 1.2 : 1),
        configured: x.chunk === "auto" ? "auto" : Number(x.chunk),
        providers: routing.providers,
        build_id: v.build_id,
        runtime: null,
        alt: v.alt,
        gpu_lease: false,
        ...(share === undefined ? {} : { share }),
      });
    }
  }

  // --- agent version: a job whose build needs a newer agent makes the agent update first ---
  // Rev 6: a run or campaign with a definition needs a definition-aware agent, like a build's
  // min_agent; one without a definition gets no definition-aware agent (PROTOCOL 7, rollout).
  // Rev 9: so does a run's or campaign's own `min_agent` (an update only when the channel offers one
  // new enough; else that run or campaign is skipped and the agent gets other work).
  let need: string | null = release && cmpSemver(agent, release.min_agent) < 0 ? release.min_agent : null;
  const passed = cands.filter((cd) => {
    const r = cd.run ? runs.find((x) => x.run === cd.run) : null;
    const camp = r ? null : camps.find((x) => x.campaign === cd.campaign)!;
    // Rev 11: the candidate's own build and runtime (a run's default, or the alternate choiceFor chose; rev 17: a
    // campaign's build or its alternate).
    const ids = r ? [cd.build_id, cd.runtime] : [cd.build_id];
    if (ids.some((id) => id && blocked.has(id))) return false;
    const infos = ids.filter(Boolean).map((id) => builds.get(id!));
    if (infos.some((i) => !i)) return false; // build not registered
    if (infos.some((i) => storedPlatform(i!.platform) !== platform)) return false; // rev 17: never another platform's build
    const def = defs.get(r ? `run:${r.run}` : `campaign:${camp!.campaign}`);
    if (!def && defAware) return false;
    if (def && defAware && leaseProblem(def, cd.kind as JobKind, ids[0]!, r ? cd.runtime : null, c.now, cd.alt ? cd.runtime : undefined)) return false;
    // Rev 9: the run's or campaign's own min_agent counts like a build's.
    const own = r ? r.min_agent : camp!.min_agent;
    // Rev 20 (PROTOCOL 14.1): a definition that signs a model variant can't be read by agents before VARIANT_AGENT.
    const defAgent = def ? (hasVariants(def) ? maxSemver([cfg.definitionAgent, cfg.variantAgent]) : cfg.definitionAgent) : null;
    const req = maxSemver([...infos.map((i) => i!.min_agent), cd.kind === "eval" ? EVAL_MIN_AGENT : null, defAgent, own]);
    if (req && !minAgentOk(agent, req)) {
      // Only ask for an update the channel can deliver (rev 24: never for a `soon` batch, which has nothing to give).
      if (!cd.soon && release && cmpSemver(req, release.version) <= 0) need = maxSemver([need, req]);
      return false;
    }
    const worker = infos.find((i) => i!.kind === "worker");
    return !worker || worker.aw === null || worker.aw === aw;
  });
  // Rev 24 (15.2): `soon` batches only say that eval work is about to need this machine; they are never offered.
  const usable = passed.filter((cd) => !cd.soon);
  const soon = setupSoon || passed.length > usable.length;

  const rate = new Map<string, number>((ratesR.results as any[]).map((x) => [`${x.scope}|${x.kind}`, x.rate]));
  const classRate = new Map<string, number>((classR.results as any[]).map((x) => [`${x.scope}|${x.kind}`, x.rate]));
  // Rev 13: the runs' active leases (self-play and eval), for a background batch's share.
  const activeRuns = [...active].filter(([k]) => k.endsWith("|selfplay") || k.endsWith("|eval")).reduce((t, [, n]) => t + n, 0);
  // Rev 29 (PROTOCOL 15.8): in the lane's view, the machines on their way to a batch count as its leases (their own
  // self-play leases are still active, so the runs' total above already has them).
  const counted = new Map(active);
  for (const [scope, n] of view.transit ?? []) counted.set(`${scope}|eval`, (counted.get(`${scope}|eval`) ?? 0) + n);
  const score = (cd: Candidate) => (cd.priority * cd.fit) / (1 + (counted.get(`${cd.scope}|${cd.kind}`) ?? 0));

  // Rev 18 (PROTOCOL 12): a PC that plays some run's self-play on its GPU records it (the CPU PCs' eval capacity reads
  // it), and takes an eval batch after that self-play unless the CPU PCs online couldn't finish it within the horizon.
  const gpuPlay = usable.some((cd) => cd.kind === "selfplay" && cd.gpu_play);
  if (m.gpu_play !== (gpuPlay ? 1 : 0)) {
    await db.prepare(`UPDATE machines SET gpu_play = ? WHERE machine_id = ?`).bind(gpuPlay ? 1 : 0, m.machine_id).run();
    m.gpu_play = gpuPlay ? 1 : 0;
  }
  if (hw.on && gpuPlay && hw.eval_horizon_s > 0) {
    for (const cd of usable) {
      if (cd.kind !== "eval" || cd.tier !== 1 || cd.unissued === undefined) continue;
      // Rev 24 (PROTOCOL 15.5): under the lane a PC busy on fuzz the lane won't give back is no capacity, and a batch
      // nobody has taken or worked on for eval_stall_s is behind whatever the capacity says.
      const cpu = await cpuEvalRate(db, cd.scope, m.machine_id, c.now, cd.platforms ?? [platform], lane ? hw.priority_lane : null);
      cd.deferred = !evalBehind(cd.unissued, cpu, hw.eval_horizon_s);
      if (cd.deferred && lane && hw.eval_stall_s > 0 && cd.run && cd.batch && (await batchStalled(db, cd.run, cd.batch, hw.eval_stall_s, c.now))) {
        cd.deferred = false;
      }
    }
  }

  // A deferred batch (rev 18) ranks after the self-play, and never before where its share (rev 13) puts it.
  const rank = (cd: Candidate) => {
    const r = jobRank(cd.kind, cd.share, counted.get(`${cd.scope}|${cd.kind}`) ?? 0, activeRuns);
    return cd.deferred ? Math.max(r, DEFERRED_EVAL_RANK) : r;
  };
  // Within a tier, evaluation before self-play (rev 7; a background batch only within its share, rev 13; a GPU self-play
  // PC's batch only when behind, rev 18), then by score.
  usable.sort((x, y) => x.tier - y.tier || rank(x) - rank(y) || score(y) - score(x) || x.scope.localeCompare(y.scope));
  return {
    usable,
    need,
    defAware,
    farmPaused: pausedR.results[0]?.value === "1",
    mine: mineR.results[0]?.n ?? 0,
    diag: (diagR.results as { id: number }[]).map((d) => d.id),
    rank,
    score,
    rate,
    classRate,
    hw,
    soon,
    active: counted,
    activeRuns,
    bgScopes,
  };
}

// Rev 11 (PROTOCOL 9.4): the size of a GPU lease. `gpu_games_per_lease` a number: exactly that. "auto", or absent
// with `games_per_lease` "auto": the larger of the normal size and the games at which this machine's GPU pays
// (`caps.gpu_games`, else GPU_LEASE_MIN before its first bench), at most GPU_LEASE_MAX; for self-play no more than
// 0.8 x the machine's measured rate x the run's iteration interval x max_stale - if the GPU's games don't fit under
// that, the normal size (the agent then measures and runs the CPU). Absent with a number: that number (unchanged).
export const GPU_LEASE_MAX = 4096;
export async function gpuLeaseSize(db: D1Database, cd: Candidate, normal: number, rate: number | null, paysAt: number | undefined): Promise<number> {
  const kind = cd.kind as "selfplay" | "eval";
  if (typeof cd.gpu_configured === "number") return cd.gpu_configured;
  if (cd.gpu_configured === undefined && cd.configured !== "auto") return normal;
  if (paysAt === 0) return normal; // the machine measured that no GPU provider pays for this run's network
  const gpu = Math.min(paysAt ?? GPU_LEASE_MIN[kind], GPU_LEASE_MAX);
  if (kind === "selfplay" && rate !== null && rate > 0) {
    const interval = await iterationInterval(db, cd.run!);
    if (interval !== null) {
      const cap = Math.floor(0.8 * rate * interval * Math.max(1, cd.max_stale ?? 1));
      if (gpu > cap) return normal;
      return Math.min(Math.max(normal, gpu), Math.max(cap, 1));
    }
  }
  return Math.max(normal, gpu);
}

// A run's iteration interval: the median gap between its last checkpoint registrations (null before 3 of them).
export async function iterationInterval(db: D1Database, run: string): Promise<number | null> {
  const { results } = await db.prepare(`SELECT t FROM checkpoints WHERE run = ? ORDER BY gen DESC LIMIT 6`).bind(run).all<{ t: number }>();
  if (results.length < 3) return null;
  const gaps = results.slice(1).map((r, i) => results[i].t - r.t).filter((g) => g > 0).sort((a, b) => a - b);
  return gaps.length ? gaps[Math.floor(gaps.length / 2)] : null;
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
  // Rev 15 (PROTOCOL 10.6): a rolling lease's newest generation in play (ignored for a classic lease).
  const gen = int(b.gen, "gen", { optional: true, min: 0, max: 999_999_999 }) ?? null;
  // Rev 24 (PROTOCOL 15.3): a fuzz lease gives its machine back to an eval batch that waits for it.
  const yieldTo = l.kind === "fuzz" && l.state === "active" && l.pending === null && state === "running" ? await fuzzYield(c, m, l, progress) : null;
  const ans = unwrap(
    await bookFor(c.env, l.scope).beat({ now: c.now, machine_id: m.machine_id, lease_id: leaseId, scope: l.scope, state, progress, stats, gen, yield_to: yieldTo }),
  );
  if (yieldTo && ans.action === "give_back") await laneMarkYield(c, m, yieldTo); // rev 29 (15.8)
  if (stats) {
    telemetry(c.env.TELEMETRY).write({ machine_id: m.machine_id, class: m.class, lease_id: leaseId, kind: l.kind, scope: l.scope, event: "beat", stats });
  }
  const uploadDiag = await takeUploadDiag(c.env.DB, m, c.now);
  // Rev 26 (PROTOCOL 18.2): a rolling self-play lease's answer keeps its freshness budget current (rev 28, 20.2: and its
  // poll interval)
  const live = l.kind === "selfplay" && book.isRolling(l) && l.state === "active";
  const fresh = live ? await leaseFresh(c, m, l.run) : undefined;
  const poll = live ? leasePoll(c, m) : undefined;
  const out = { ...ans, ...(fresh === undefined ? {} : { fresh_s: fresh }), ...(poll === undefined ? {} : { poll_s: poll }) };
  return json(uploadDiag ? { ...out, upload_diag: uploadDiag } : out);
}

// Rev 15 (PROTOCOL 10.4): POST /v1/lease/{id}/extend `{"games": k}` releases k more ids of a rolling lease (fewer at
// the run's headroom or the block's end), answers `wait_s` when the generation's quota is full, or `finish`. Past
// `until` the lease is renewed unless other work waits for this machine (otherWorkWaits). The beat bucket limits it.
export async function extend(c: Ctx, leaseId: string): Promise<Response> {
  const m = await authMachine(c);
  await take(c, m.machine_id, "beat", "extension");
  const l = await ownLeaseRow(c, m, leaseId);
  const b = await readJson(c.req, 64 * 1024);
  const games = int(b.games, "games", { min: 1, max: book.MAX_LEASE_IDS });
  // Rev 28 (PROTOCOL 20.1): the newest generation the lease's workers play (agents from 0.3.20): the answer offers newer
  const gen = int(b.gen, "gen", { optional: true, min: 0, max: 999_999_999 }) ?? null;
  if (l.kind !== "selfplay" || !book.isRolling(l)) throw bad("only a rolling self-play lease is extended (PROTOCOL 10.4)");
  const farm = await c.env.DB.prepare(`SELECT value FROM settings WHERE key = 'paused'`).first<{ value: string }>();
  const paused = !!m.paused || farm?.value === "1";
  const due = l.state === "active" && !paused && (l.until === null || c.now >= l.until);
  // Rev 24 (PROTOCOL 15.4): an eval batch that waits for this machine ends the lease at any extension, not only past
  // `until` (the renewal's rule, below, is unchanged).
  let yieldTo: string | null = null;
  if (l.state === "active" && !paused && l.pending === null) {
    const hw = await hwSetting(c.env.DB);
    if (laneApplies(hw, m.channel)) yieldTo = await laneTarget(c, m, l, hw);
  }
  const waiting = due && !yieldTo ? await otherWorkWaits(c, m, l) : null;
  const ans = unwrap(
    await bookFor(c.env, l.scope).extend({ now: c.now, machine_id: m.machine_id, lease_id: leaseId, scope: l.scope, games, paused, waiting, yield_to: yieldTo, gen }),
  );
  if (yieldTo && ans.finish && ans.reason === "yield") await laneMarkYield(c, m, yieldTo); // rev 29 (15.8)
  // Rev 26 (PROTOCOL 18.2): with the run's freshness budget as it is now (rev 28, 20.2: and the poll interval)
  const fresh = l.state === "active" ? await leaseFresh(c, m, l.run) : undefined;
  const poll = l.state === "active" ? leasePoll(c, m) : undefined;
  return json({ ...ans, ...(fresh === undefined ? {} : { fresh_s: fresh }), ...(poll === undefined ? {} : { poll_s: poll }) });
}

// Rev 28 (PROTOCOL 20.2): POST /v1/lease/{id}/poll `{"gen": g}` - has the run a newer generation for this rolling lease
// than `g`, the newest its workers play? The answer is a beat's offer (`checkpoint`, with `definition` when there is one)
// plus the lease's `fresh_s` and `poll_s` as a beat answer has them. It changes nothing: no id is released, no renewal or
// yield is decided, `cur_gen` is not stored (beats and extensions do that), and it doesn't go through the scope's
// LeaseBook (it reads only). Its own bucket (`poll`, one per 2 s), so a poll every few seconds leaves the beat bucket
// to beats and extensions.
export async function poll(c: Ctx, leaseId: string): Promise<Response> {
  const m = await authMachine(c);
  await take(c, m.machine_id, "poll", "poll");
  const l = await ownLeaseRow(c, m, leaseId);
  const b = await readJson(c.req, 4 * 1024);
  const gen = int(b.gen, "gen", { min: 0, max: 999_999_999 });
  if (l.kind !== "selfplay" || !book.isRolling(l)) throw bad("only a rolling self-play lease is polled (PROTOCOL 20.2)");
  if (l.state === "expired") throw gone("lease expired");
  if (l.state === "cancelled") throw gone("lease was cancelled");
  if (l.state !== "active") throw gone("lease is closed");
  const offer = await book.rollingOffer(c.env.DB, l, gen);
  const fresh = await leaseFresh(c, m, l.run);
  const pollS = leasePoll(c, m);
  return json({
    checkpoint: offer?.checkpoint ?? null,
    ...(offer ? { definition: offer.definition } : {}),
    ...(fresh === undefined ? {} : { fresh_s: fresh }),
    ...(pollS === undefined ? {} : { poll_s: pollS }),
  });
}

// Rev 28 (PROTOCOL 20.2): a rolling lease's poll interval for the answers: undefined for an agent before POLL_AGENT (the
// answer keeps exactly its keys of before), else the owner's `poll_s` for this machine or null (none: stop polling).
function leasePoll(c: Ctx, m: MachineRow): number | null | undefined {
  const agent = m.agent_version;
  if (!agent || !isSemver(agent) || cmpSemver(agent, config(c.env).pollAgent) < 0) return undefined;
  return machineFreshOf(m.fresh).poll_s ?? null;
}

// ---------- rev 26 (PROTOCOL 18): the freshness budget ----------

// The machine is sent its rolling leases' freshness budget: an agent from FRESH_AGENT on a machine the owner's
// `fresh_cap` names (18.4). Rev 28 (PROTOCOL 20.3): or on a machine the owner gave a `share` - unless `fresh_cap` is
// "off", the kill switch for every budget.
function freshFor(freshAgent: string, agent: string | null, hw: HwRouting, m: MachineRow): boolean {
  if (!agent || !isSemver(agent) || cmpSemver(agent, freshAgent) < 0) return false;
  return freshApplies(hw, m.channel) || (hw.fresh_cap !== "off" && machineFreshOf(m.fresh).share !== undefined);
}

// A rolling self-play lease's freshness budget for the beat and extension answers: undefined for an agent before
// FRESH_AGENT (the answer keeps exactly its keys of before); null for a newer agent the owner's `fresh_cap` leaves out
// (so turning it off, or a machine moved off canary, lifts a cap the agent already holds - 18.4); else the run's
// budget now (null: none - not active, not rolling, or too little history).
async function leaseFresh(c: Ctx, m: MachineRow, run: string | null): Promise<number | null | undefined> {
  const agent = m.agent_version;
  // (the agent's version first: no read for an older agent)
  if (!run || !agent || !isSemver(agent) || cmpSemver(agent, config(c.env).freshAgent) < 0) return undefined;
  if (!freshFor(config(c.env).freshAgent, agent, await hwSetting(c.env.DB), m)) return null;
  const r = await book.getRun(c.env.DB, run);
  const sp = r && r.state === "active" ? parseJson<book.SelfplayCfg | null>(r.selfplay, null) : null;
  return runFresh(c.env.DB, run, sp, c.now, machineFreshOf(m.fresh).share);
}

// ---------- rev 24 (PROTOCOL 15): the priority lane ----------

async function hwSetting(db: D1Database): Promise<HwRouting> {
  const row = await db.prepare(`SELECT value FROM settings WHERE key = 'hw_routing'`).first<{ value: string }>();
  return hwRoutingOf(row?.value ?? null);
}

// Active runs' current eval batches with unissued ids: scope -> unissued games.
async function waitingBatches(db: D1Database): Promise<Map<string, number>> {
  const { results } = await db
    .prepare(
      `SELECT p.scope, SUM(p.hi - p.lo) AS n FROM runs r JOIN pool p ON p.scope = 'run:' || r.run AND p.pool = 'eval:' || r.eval_batch
       WHERE r.state = 'active' AND r.eval_batch IS NOT NULL AND r.eval IS NOT NULL AND r.eval != 'null' GROUP BY p.scope`,
    )
    .all<{ scope: string; n: number }>();
  return new Map(results.filter((x) => x.n > 0).map((x) => [x.scope, x.n]));
}

// Live leases on their way to an eval batch (`pending` "yield:<scope>"): scope -> leases.
async function inTransit(db: D1Database, now: number): Promise<Map<string, number>> {
  const { results } = await db
    .prepare(`SELECT pending, COUNT(*) AS n FROM leases WHERE state = 'active' AND expires_at >= ? AND pending LIKE 'yield:%' GROUP BY pending`)
    .bind(now)
    .all<{ pending: string; n: number }>();
  return new Map(results.map((x) => [yieldTarget(x.pending)!, x.n]));
}

// 15.3: a fuzz lease that will be done within FUZZ_TAIL_S at its own pace so far (seeds done per running second; its
// progress from this beat, else its last) is left to finish. Without progress: not known, so not.
export function fuzzAlmostDone(l: Pick<book.LeaseRow, "issued_at" | "paused_s" | "progress">, progress: Record<string, number> | undefined, now: number): boolean {
  const p = progress ?? parseJson<Record<string, unknown> | null>(l.progress, null) ?? {};
  const done = typeof p.games_done === "number" ? p.games_done : 0;
  const total = typeof p.games_total === "number" ? p.games_total : 0;
  if (done <= 0 || total <= 0) return false;
  if (done >= total) return true;
  const running = Math.max(1, now - l.issued_at - l.paused_s);
  return (running * (total - done)) / done <= FUZZ_TAIL_S;
}

async function fuzzYield(c: Ctx, m: MachineRow, l: book.LeaseRow, progress: Record<string, number> | undefined): Promise<string | null> {
  const hw = await hwSetting(c.env.DB);
  if (!laneApplies(hw, m.channel) || fuzzAlmostDone(l, progress, c.now)) return null;
  return laneTarget(c, m, l, hw);
}

// Rev 24 (PROTOCOL 15.3, 15.4): the eval batch (its scope) that machine `m`'s lease `l` - a fuzz lease, or a rolling
// self-play lease - gives way to now, or null. In the machine's order as its next lease request would see it (offer()
// with what it last asked with - rev 29: its kinds and cached builds too - its own lease not counted and the machines on
// their way counted as their batch's leases): the first candidate that would be granted before the lease's own job must
// be an eval batch with unissued ids; the batch can use one more machine (its machines on their way plus this one at
// most its unissued games over this machine's lease size); it sorts before the lease's own job (its share, 5.3; a
// deferred batch, 12.2, never does for a GPU self-play lease); and for a rolling lease every background job's leases and
// machines on their way, with this one, stay within the batch's share of the runs' active leases (several analysis
// campaigns together take no more of a live run than one would).
// Rev 29 (PROTOCOL 15.8): a batch this machine gave way to recently and then was not given is skipped (LANE_MISS_S); and
// a rolling lease gives way only when it can finish its games in flight within LANE_MAX_DRAIN_S and before the other
// PCs that could play the batch would have played all of it.
export async function laneTarget(c: Ctx, m: MachineRow, l: book.LeaseRow, hw: HwRouting): Promise<string | null> {
  const db = c.env.DB;
  if (!laneApplies(hw, m.channel) || m.paused) return null;
  const waiting = await waitingBatches(db);
  if (!waiting.size) return null;
  const transit = await inTransit(db, c.now);
  // (cheap first: every waiting batch already has a machine on its way per smallest eval lease of its unissued games)
  if (![...waiting].some(([s, n]) => (transit.get(s) ?? 0) + 1 <= batchMachines(n, SIZING.eval.min))) return null;
  const ask = storedAsk(c, m, l);
  const o = await offer(c, m, ask, parseJson<Profile | null>(m.profile, null), { closing: l, transit });
  if (o.farmPaused) return null;
  const rolling = l.kind === "selfplay";
  const lane = laneStateOf(m.lane);
  const own = o.usable.find((cd) => cd.scope === l.scope && cd.kind === l.kind);
  const ownKey: SortKey = own ? { tier: own.tier, rank: o.rank(own), score: o.score(own) } : { tier: rolling ? 1 : 2, rank: kindRank(l.kind), score: 0 };
  for (const cd of o.usable) {
    if (cd === own || !sortsBefore({ tier: cd.tier, rank: o.rank(cd), score: o.score(cd) }, ownKey)) return null;
    if (cd.kind === "selfplay") {
      // another run's self-play first: granted only with headroom (then the lane has nothing to do here)
      const r = cd.run ? await book.getRun(db, cd.run) : null;
      const sp = r ? parseJson<book.SelfplayCfg | null>(r.selfplay, null) : null;
      if (!r || !sp || (await book.selfplayState(db, r, sp)).headroom < 1) continue;
      return null;
    }
    if (cd.kind !== "eval") return null;
    const unissued = waiting.get(cd.scope) ?? 0;
    if (unissued < 1) continue;
    if (laneMissed(lane, cd.scope, cd.batch, c.now)) continue; // rev 29: the grant didn't follow the last yield to it
    const t = transit.get(cd.scope) ?? 0;
    if (t + 1 > batchMachines(unissued, await wantFor(db, o, cd, ask.caps, ask.gpuGames))) return null;
    if (rolling && cd.share !== undefined) {
      // (o.active counts the machines on their way as their batch's leases; o.activeRuns no longer counts this lease)
      const bg = [...o.bgScopes].reduce((s, x) => s + (o.active.get(`${x}|selfplay`) ?? 0) + (o.active.get(`${x}|eval`) ?? 0), 0);
      if (bg + 1 > cd.share * (o.activeRuns + 1) + 1e-9) return null;
    }
    if (rolling && !(await drainFits(c, m, l, hw, cd, unissued))) return null;
    return cd.scope;
  }
  return null;
}

// Rev 29 (PROTOCOL 15.8): rolling lease `l` can give way to eval batch `cd` (`unissued` games): its drain (drainEstimate,
// with the machine's measured rate for the run) is unknown, or at most LANE_MAX_DRAIN_S and shorter than the time the
// other PCs that could play the batch on the CPU (cpuEvalRate, as the deferral counts them) would take for all of it -
// unless the batch is stalled (15.5: those PCs are not taking it).
async function drainFits(c: Ctx, m: MachineRow, l: book.LeaseRow, hw: HwRouting, cd: Candidate, unissued: number): Promise<boolean> {
  const db = c.env.DB;
  const known = await db
    .prepare(`SELECT rate FROM rates WHERE machine_id = ? AND kind = 'selfplay' AND scope = ?`)
    .bind(m.machine_id, l.scope)
    .first<{ rate: number }>();
  const running = Math.max(0, c.now - l.issued_at - l.paused_s);
  const drain = drainEstimate(book.batchOf(l), book.unfinishedIds(l), book.finishedIds(l), running, known?.rate ?? 0);
  if (drain === null) return true;
  if (drain > LANE_MAX_DRAIN_S) return false;
  const cpu = await cpuEvalRate(db, cd.scope, m.machine_id, c.now, cd.platforms ?? [storedPlatform(m.platform)], hw.priority_lane);
  if (cpu <= 0 || unissued / cpu >= drain) return true;
  return hw.eval_stall_s > 0 && !!cd.run && !!cd.batch && (await batchStalled(db, cd.run, cd.batch, hw.eval_stall_s, c.now));
}

// Rev 15 (PROTOCOL 10.4): what else waits for this machine, in the order PROTOCOL 3 picks work (offer(), with the caps
// and profile the coordinator last stored for it - rev 29: and the kinds and cached builds it last offered, storedAsk):
// a diagnostics request it would get as a lease; a job that ranks before the rolling lease's own self-play by tier and
// rank (an eval batch with
// unissued ids, a fuzz campaign or a background job within its share); or, at the same rank, another run's
// self-play with a higher priority. A self-play counts only with headroom. Null: nothing waits.
export async function otherWorkWaits(c: Ctx, m: MachineRow, l: book.LeaseRow): Promise<string | null> {
  const db = c.env.DB;
  const o = await offer(c, m, storedAsk(c, m, l), parseJson<Profile | null>(m.profile, null));
  if (!o.defAware && o.diag.length) return "diag";
  const own = o.usable.find((cd) => cd.scope === l.scope && cd.kind === "selfplay");
  const ownRun = await book.getRun(db, l.run!);
  const ownTier = own?.tier ?? 1;
  const ownRank = own ? o.rank(own) : kindRank("selfplay");
  const ownPriority = own?.priority ?? ownRun?.priority ?? 0;
  // Rev 18 (PROTOCOL 12.2): a deferred eval batch still waits while the lease's own generation has no headroom (the
  // rolling lease would only wait for the next generation), as every eval batch did before.
  let ownFull: boolean | null = null;
  const ownQuotaFull = async () => {
    if (ownFull === null) {
      const sp = ownRun ? parseJson<book.SelfplayCfg | null>(ownRun.selfplay, null) : null;
      ownFull = !ownRun || !sp || (await book.selfplayState(db, ownRun, sp)).headroom < 1;
    }
    return ownFull;
  };
  for (const cd of o.usable) {
    if (cd === own) continue;
    if (cd.tier > ownTier) continue;
    if (cd.tier === ownTier) {
      const rk = o.rank(cd);
      if (rk > ownRank || (rk === ownRank && !(cd.priority > ownPriority))) {
        if (!(cd.deferred && (await ownQuotaFull()))) continue;
      }
    }
    if (cd.kind === "selfplay") {
      const r = await book.getRun(db, cd.run!);
      const sp = r ? parseJson<book.SelfplayCfg | null>(r.selfplay, null) : null;
      if (!r || !sp || (await book.selfplayState(db, r, sp)).headroom < 1) continue;
    }
    return `${cd.kind}:${cd.run ?? cd.campaign}`;
  }
  return null;
}

// What a machine last asked with (POST /v1/lease), as the coordinator stored it - its caps, `gpu_allowed` and
// `gpu_games` (rev 18), its agent version, and (rev 29, PROTOCOL 15.8) its job kinds and cached builds: the ask of a
// rolling lease's renewal (rev 15, otherWorkWaits) and of the priority lane's decisions (rev 24, laneTarget), so they
// see the candidates the machine's next lease request will. Cached: also rev 28's (PROTOCOL 20.5) running lease's own
// worker build and runtime pack - they are on the machine's disk, and the stored `disk_mb` is the free space with them
// there (before rev 28 nothing at all: on a PC with little free disk the lease's own run didn't fit its disk cap, so
// another run's self-play "waited" and the lease was finished at every renewal, only for the next lease request - which
// says what is cached - to get the same run again; before rev 29 its other cached builds were missing the same way, and
// every kind was assumed). No checkpoint counts as cached (an eval batch's networks count against the disk: the lane
// may miss a batch the grant would give, never the reverse).
function storedAsk(c: Ctx, m: MachineRow, l?: Pick<book.LeaseRow, "build_id" | "lease">): Ask {
  const stored = parseJson<(Partial<Caps> & { gpu_allowed?: unknown; gpu_games?: unknown }) | null>(m.caps, null) ?? {};
  const caps: Caps = {
    threads: typeof stored.threads === "number" ? stored.threads : 1,
    ram_mb: typeof stored.ram_mb === "number" ? stored.ram_mb : 0,
    gpu: stored.gpu === true,
    disk_mb: typeof stored.disk_mb === "number" ? stored.disk_mb : 0,
  };
  // Rev 18: what the last /v1/lease said of GPU work (caps stored before rev 18: `gpu` alone, no bench).
  const gpuAllowed = typeof stored.gpu_allowed === "boolean" ? stored.gpu_allowed : caps.gpu;
  const g = stored.gpu_games;
  const gpuGames = new Map<string, number>(
    g && typeof g === "object" && !Array.isArray(g) ? Object.entries(g).filter((e): e is [string, number] => typeof e[1] === "number") : [],
  );
  const agent = m.agent_version && isSemver(m.agent_version) ? m.agent_version : "0.0.0";
  const awHeader = c.req.headers.get("x-farm-aw") ?? "1";
  const aw = /^\d{1,4}$/.test(awHeader) ? Number(awHeader) : 1;
  const own = l ? [l.build_id, parseJson<{ runtime?: { build_id?: string } | null }>(l.lease, {}).runtime?.build_id] : [];
  // Rev 29 (PROTOCOL 15.8): and the kinds and cached builds its last /v1/lease offered (stored before rev 29: every kind,
  // nothing cached)
  const asked = parseJson<{ kinds?: unknown; builds?: unknown } | null>(m.ask ?? null, null) ?? {};
  const strs = (x: unknown) => (Array.isArray(x) ? x.filter((v): v is string => typeof v === "string" && v.length > 0) : []);
  const cachedBuilds = new Set([...own.filter((x): x is string => typeof x === "string" && x.length > 0), ...strs(asked.builds)]);
  const kinds = Array.isArray(asked.kinds) ? new Set(strs(asked.kinds).filter((k) => (KINDS as readonly string[]).includes(k))) : new Set<string>(KINDS);
  return { caps, gpuAllowed, gpuGames, cachedBuilds, cachedCkpts: new Set(), kinds, agent, aw };
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
    parseRanges(ids, "x-ids", [l.lo!, book.relHi(l)!]); // rev 15: a rolling lease's released ids
  }
  // Review M3: chunks and bytes per lease, and no results long after it closed. Checked before
  // the body is read (with its declared length) and again by LeaseBook with the real size.
  const cfg = config(c.env);
  const limits = { chunks: cfg.maxLeaseChunks, bytes: cfg.maxLeaseBytes, late_s: cfg.lateResultsS };
  const declared = Number(c.req.headers.get("content-length") ?? "0");
  await book.checkResultLimits(c.env.DB, l, Number.isSafeInteger(declared) && declared > 0 ? declared : 0, limits, c.now);
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
      limits,
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
  // Rev 6: the agent refused the lease (its definition didn't verify or match): closed as given back.
  const refused = str(b.refused, "refused", { optional: true, max: 32, re: /^[a-z][a-z0-9_]{0,31}$/ }) ?? null;
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
      refused,
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
  const key = checkKey(uriDecode(rawKey));
  // Rev 6: definitions (defs/<kind>/<id>/<version>/definition.json and .minisig) too.
  if (!key.startsWith("builds/") && !key.startsWith("ckpt/") && !DEF_KEY_RE.test(key)) throw notFound(`no artifact ${key}`);
  return serveObject(c, key);
}

// Rev 22 (PROTOCOL 9.8): the owner's GPU vendors off (`POST /v1/admin/gpu_vendors_off`), in the order stored.
export async function vendorsOffList(db: D1Database): Promise<string[]> {
  const row = await db.prepare(`SELECT value FROM settings WHERE key = 'gpu_vendors_off'`).first<{ value: string }>();
  return [...vendorsOffOf(row?.value ?? null)];
}

// Rev 12 (PROTOCOL 2): the owner's evict list (`POST /v1/admin/evict`).
export async function evictList(db: D1Database): Promise<string[]> {
  const row = await db.prepare(`SELECT value FROM settings WHERE key = 'evict_builds'`).first<{ value: string }>();
  try {
    const v = row ? JSON.parse(row.value) : [];
    return Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : [];
  } catch {
    return [];
  }
}

export async function agentLatest(c: Ctx): Promise<Response> {
  const m = await authMachine(c);
  // Rev 10: the machine's own channel, always. Agents send the channel they were enrolled on and
  // never learn of a later move (POST /v1/admin/machine/{id} "channel"), so honouring the query
  // left a machine moved to canary asking for stable forever while its leases demanded canary's
  // min_agent (found 2026-11-12). The query is still accepted, and ignored.
  const channel = m.channel;
  // Rev 17 (PROTOCOL 11.2): the newest release of the machine's channel for its platform.
  const rel = await latestRelease(c.env.DB, channel, m.platform);
  if (!rel) throw notFound(`no agent release on ${channel} for ${m.platform}`);
  // Rev 12: the evict list rides along (only when there is one: the answer of before otherwise); rev 22: the GPU vendors
  // off too (agents from AMD_DML_AGENT apply it; absent: none)
  const evict = await evictList(c.env.DB);
  const vendorsOff = await vendorsOffList(c.env.DB);
  return json({
    version: rel.version,
    build_id: rel.build_id,
    manifest: `builds/${rel.build_id}/manifest.json`,
    min_agent: rel.min_agent,
    ...(evict.length ? { evict } : {}),
    ...(vendorsOff.length ? { gpu_vendors_off: vendorsOff } : {}),
  });
}

export async function crash(c: Ctx): Promise<Response> {
  const m = await authMachine(c);
  await take(c, m.machine_id, "crash", "crash report");
  const b = await readJson(c.req, 256 * 1024);
  const kind = oneOf(b.kind, "kind", CRASH_KINDS);
  const buildId = str(b.build_id, "build_id", { optional: true, re: BUILD_RE }) ?? null;
  const leaseId = str(b.lease_id, "lease_id", { optional: true, re: LEASE_RE }) ?? null;
  const osError = int(b.os_error, "os_error", { optional: true, min: -(2 ** 31), max: 2 ** 32 }) ?? null;
  const message = typeof b.message === "string" ? b.message.slice(0, 4096) : null;
  const backtrace = typeof b.backtrace === "string" ? b.backtrace.slice(0, 65536) : null;
  if (kind === "blocked" && !buildId) throw bad("a blocked report needs the build_id Windows refused to start");
  // Review M3: at most MAX_CRASHES_PER_DAY rows per machine per 24 h (rows are deleted after
  // CRASH_RETENTION_DAYS by the cron). Past the cap a report is answered but not stored; a block
  // is still recorded (it changes what the machine is offered).
  const cfg = config(c.env);
  const today = await c.env.DB.prepare(`SELECT COUNT(*) AS n FROM crashes WHERE machine_id = ? AND t > ?`)
    .bind(m.machine_id, c.now - 86400)
    .first<{ n: number }>();
  const store = (today?.n ?? 0) < cfg.maxCrashesPerDay;
  const stmts = store
    ? [
        c.env.DB.prepare(
          `INSERT INTO crashes (machine_id, build_id, kind, message, backtrace, lease_id, agent_version, os_error, t) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)`,
        ).bind(m.machine_id, buildId, kind, message, backtrace, leaseId, m.agent_version, osError, c.now),
      ]
    : [];
  // Rev 5: Windows refused to start this worker build here. It isn't offered to this machine for
  // a day, or until a newer agent asks (blocks.ts).
  if (kind === "blocked") {
    stmts.push(recordBlock(c.env.DB, { machine_id: m.machine_id, build_id: buildId!, agent_version: m.agent_version, os_error: osError, message, now: c.now }));
  }
  if (stmts.length) await c.env.DB.batch(stmts);
  return json(store ? { ok: true } : { ok: true, stored: false });
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
