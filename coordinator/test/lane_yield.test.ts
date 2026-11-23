// Rev 29 (PROTOCOL 15.8): the priority lane yields a lease only for a batch the machine's next lease request would get.
//
// The loop it fixes (2026-11-23, rev 27): a canary PC with a CUDA GPU and little free disk (less than its CUDA runtime
// pack) played rolling self-play on its GPU while short analysis campaigns came and went. At every extension the lane
// worked out the PC's order from a reconstructed ask that listed nothing cached, so the CUDA pack didn't fit the disk and
// the PC had no GPU self-play in the lane's eyes: no deferral (12.2), and the campaign's batch came first. The lease
// finished its games in flight (2-10 minutes) and the PC asked for a lease - listing its cache - and the grant, which saw
// its GPU self-play, deferred the batch (CPU PCs could play it) and gave it self-play again. Seven times in 45 minutes.
// Rev 28 counted the lease's own build and pack as cached; rev 29 replays the machine's whole last ask (its job kinds
// and cached builds), remembers a yield the grant didn't follow (no yield to that batch for LANE_MISS_S), and gives way
// only when the lease can finish its games in flight in time (LANE_MAX_DRAIN_S, and before the CPU PCs would have played
// the whole batch).

import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import {
  LANE_DRAIN_TAIL,
  LANE_MAX_DRAIN_S,
  LANE_MISS_S,
  LANE_YIELD_TTL_S,
  drainEstimate,
  laneMissed,
  laneSettled,
  laneStateOf,
  laneStateText,
} from "../src/routing";
import { PROFILE, type Machine, admin, advance, beat, call, clock, define, enroll, finish, makeBuild, makeCampaign, makeRun, ok, publishCkpt, runBody } from "./helpers";

const CPU_ONLY = { ...PROFILE, gpus: [], providers: ["cpu"] };
const GPU_ON = { gpu: true, gpu_allowed: true };
const CPU_CAPS = { gpu: false, gpu_allowed: false };
const ROLLING = { inflight: 8, gpu_inflight: 32, block: 1000 };
const SEATS = { settings: {}, seats: [{ net: 1 }, { bot: "random" }], paired: false };
const KINDS = ["fuzz", "eval", "selfplay", "diag"];

const routing = (body: unknown) => admin("POST", "/v1/admin/routing", body);
const row = (id: string) => env.DB.prepare(`SELECT * FROM leases WHERE lease_id = ?`).bind(id).first<any>();
const laneOf = async (m: Machine) => laneStateOf((await env.DB.prepare(`SELECT lane FROM machines WHERE machine_id = ?`).bind(m.machine_id).first<any>()).lane);
const setRate = (m: Machine, kind: string, scope: string, rate: number) =>
  env.DB.prepare(`INSERT OR REPLACE INTO rates (machine_id, kind, scope, rate, samples, t) VALUES (?, ?, ?, ?, 5, ?)`).bind(m.machine_id, kind, scope, rate, clock.now).run();
const online = (m: Machine) => call("POST", "/v1/lease", { token: m.token, body: { profile_digest: "ab".repeat(32), profile: null, caps: { threads: 6, ram_mb: 12000, gpu: false, disk_mb: 2500 }, kinds: [] } });

// An analysis campaign's run (PROTOCOL 5.3): no self-play, one CPU eval batch of `games` in leases of `gpl` (share 0.5
// unless given; null: none).
async function campaignRun(run: string, games: number, o: { share?: number | null; gpl?: number } = {}) {
  const share = o.share === undefined ? 0.5 : o.share;
  await makeRun(run, { selfplay: false, eval: { batch: "c1", games, checkpoints: [1], games_per_lease: o.gpl ?? 16, ...(share === null ? {} : { share }), job: SEATS } });
}

// A run whose rolling self-play has a CPU default pack and a CUDA alternate (a GPU version, PROTOCOL 12.1).
async function gpuRun(run: string, o: { cudaMb?: number } = {}) {
  const cpuRt = await makeBuild({ kind: "runtime", tag: `cpu-${run}` });
  const cuda = await makeBuild({ kind: "runtime", tag: `cuda-${run}`, gpu: { vendor: "nvidia", min_driver: "527.41", providers: ["cuda"] } });
  const def = await makeBuild({ tag: `w-cpu-${run}`, requires_runtime: cpuRt });
  const alt = await makeBuild({ tag: `w-cuda-${run}`, requires_runtime: cuda });
  const b: any = { ...runBody(def, { games_per_lease: "auto", quota: 100000 }), runtime: cpuRt, alternates: [{ build_id: alt, runtime: cuda, when: { vendor: "nvidia" } }] };
  b.selfplay.rolling = ROLLING;
  ok(await admin("PUT", `/v1/admin/run/${run}`, b));
  await publishCkpt(run, 1);
  await define("run", run, (d) => d.builds.push(alt));
  if (o.cudaMb) await env.DB.prepare(`UPDATE builds SET size = ? WHERE build_id = ?`).bind(o.cudaMb * 2 ** 20, cuda).run();
  return { cuda, alt };
}

// A CPU-only run (no GPU version) with rolling self-play.
async function cpuRun(run: string) {
  const w = await makeBuild({ tag: `w-${run}` });
  const b: any = runBody(w, { games_per_lease: "auto", quota: 100000 });
  b.selfplay.rolling = ROLLING;
  ok(await admin("PUT", `/v1/admin/run/${run}`, b));
  await publishCkpt(run, 1);
  await define("run", run);
}

interface AskOpts {
  caps?: Record<string, unknown>;
  cached?: string[];
  kinds?: string[];
  now?: number;
}
const askLease = (m: Machine, o: AskOpts = {}) =>
  call("POST", "/v1/lease", {
    token: m.token,
    agent: "0.2.13",
    now: o.now,
    body: {
      profile_digest: "ab".repeat(32),
      profile: null,
      caps: Object.assign({ threads: 6, ram_mb: 12000, gpu: false, disk_mb: 2500 }, o.caps ?? GPU_ON),
      cached: { builds: o.cached ?? [], ckpts: [] },
      kinds: o.kinds ?? KINDS,
      features: ["rolling"],
    },
  });
async function leaseOf(m: Machine, o: AskOpts = {}) {
  const b = ok(await askLease(m, o));
  expect(b.lease, JSON.stringify(b)).toBeTruthy();
  return b.lease;
}
const extend = (m: Machine, id: string, games = 8) => call("POST", `/v1/lease/${id}/extend`, { token: m.token, body: { games } });
const yielded = (a: any) => a.finish === true && a.reason === "yield";
// keeps a lease alive across a clock jump (as its beats would)
const keep = (id: string) => env.DB.prepare(`UPDATE leases SET expires_at = ? WHERE lease_id = ?`).bind(clock.now + 600, id).run();

describe("the rules as units (rev 29, 15.8)", () => {
  it("drainEstimate: games in flight over the rate, times the tail; unknown without a rate", () => {
    expect(drainEstimate(600, 1000, 0, 0, 1)).toBe(LANE_DRAIN_TAIL * 600); // the beat's games in flight, the measured rate
    expect(drainEstimate(600, 100, 0, 0, 1)).toBe(LANE_DRAIN_TAIL * 100); // never more than its released ids not finished
    expect(drainEstimate(0, 256, 0, 0, 2)).toBe(LANE_DRAIN_TAIL * 128); // before any beat: its released ids
    expect(drainEstimate(100, 1000, 500, 100, 1)).toBe(LANE_DRAIN_TAIL * 20); // its own pace (5/s) beats the known rate
    expect(drainEstimate(100, 1000, 0, 30, 0)).toBeNull(); // no pace, no measured rate
    expect(drainEstimate(100, 0, 0, 30, 0)).toBe(0); // nothing in flight
  });

  it("the lane state: a yield settled by the next lease request, misses expire", () => {
    const now = 10_000;
    const y = { yield: { scope: "run:cmp", batch: "c1", t: now - 100 }, miss: {} };
    expect(laneSettled(y, true, now)).toEqual({ yield: null, miss: {} }); // eval work: done
    const missed = laneSettled(y, false, now);
    expect(missed).toEqual({ yield: null, miss: { "run:cmp": { batch: "c1", t: now } } });
    expect(laneMissed(missed, "run:cmp", "c1", now + LANE_MISS_S - 1)).toBe(true);
    expect(laneMissed(missed, "run:cmp", "c2", now)).toBe(false); // the run's next batch is another matter
    expect(laneMissed(missed, "run:cmp", "c1", now + LANE_MISS_S)).toBe(false);
    expect(laneSettled({ ...y, yield: { ...y.yield, t: now - LANE_YIELD_TTL_S - 1 } }, false, now)).toEqual({ yield: null, miss: {} }); // stale
    expect(laneStateText(missed, now + LANE_MISS_S)).toBeNull(); // nothing left to keep
    expect(laneStateOf(laneStateText(missed, now))).toEqual(missed);
    expect(laneStateOf("not json")).toEqual({ yield: null, miss: {} });
  });
});

describe("the lane and the grant agree (rev 29, 15.8)", () => {
  // The incident: the PC's free disk is below its CUDA pack, which it has cached. A CPU PC online could play the batch
  // within the horizon, so the grant defers it for this GPU self-play PC: the lane must not finish the self-play for it.
  it("a GPU PC whose free disk is below its cached CUDA pack keeps its self-play while the grant would defer the batch", async () => {
    ok(await routing({ priority_lane: "all" }));
    const { cuda, alt } = await gpuRun("sp", { cudaMb: 1500 });
    const nv = await enroll("nv", { channel: "canary" });
    const small = { ...GPU_ON, disk_mb: 600 };
    const l = await leaseOf(nv, { caps: small, cached: [cuda, alt] });
    expect([l.kind, l.build.build_id]).toEqual(["selfplay", alt]); // on its GPU
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    ok(await online(cpu));
    await setRate(cpu, "eval", "run:old", 0.5); // 0.5 game/s x 1800 s > 200: not behind
    await campaignRun("cmp", 200);
    expect(yielded(ok(await extend(nv, l.lease_id)))).toBe(false);
    // ... and the grant agrees: a lease request now (the lease closed) gets the self-play, not the batch
    ok(await finish(nv, l.lease_id, []));
    expect((await leaseOf(nv, { caps: small, cached: [cuda, alt] })).kind).toBe("selfplay");
  });

  it("with no CPU PC to play it the batch is behind: the lane yields, and the grant then gives the batch", async () => {
    ok(await routing({ priority_lane: "all" }));
    const { cuda, alt } = await gpuRun("sp", { cudaMb: 1500 });
    const nv = await enroll("nv", { channel: "canary" });
    const small = { ...GPU_ON, disk_mb: 600 };
    const l = await leaseOf(nv, { caps: small, cached: [cuda, alt] });
    await campaignRun("cmp", 200, { share: null });
    expect(yielded(ok(await extend(nv, l.lease_id)))).toBe(true);
    expect((await laneOf(nv)).yield).toMatchObject({ scope: "run:cmp", batch: "c1" });
    ok(await finish(nv, l.lease_id, []));
    const e = await leaseOf(nv, { caps: small, cached: [cuda, alt] });
    expect([e.kind, e.run]).toEqual(["eval", "cmp"]);
    expect(await laneOf(nv)).toEqual({ yield: null, miss: {} }); // the yield did its job
  });

  it("an agent that doesn't offer eval work is never yielded for a batch", async () => {
    ok(await routing({ priority_lane: "all" }));
    await gpuRun("sp");
    const nv = await enroll("nv", { channel: "canary" });
    const l = await leaseOf(nv, { kinds: ["selfplay", "fuzz"] });
    const nv2 = await enroll("nv2", { channel: "canary" });
    const l2 = await leaseOf(nv2);
    await campaignRun("cmp", 200, { share: null });
    expect(yielded(ok(await extend(nv, l.lease_id)))).toBe(false);
    // the same PC offering eval work is
    expect(yielded(ok(await extend(nv2, l2.lease_id)))).toBe(true);
  });

  // A GPU PC on a fuzz lease (it took it while no self-play had headroom for it); then a GPU run starts whose CUDA pack it
  // has cached but its disk couldn't hold again, and a batch comes that CPU PCs can finish. Its next lease request gets
  // the GPU self-play (the batch is deferred for it), so the fuzz lease is not given back for the batch.
  it("the cached builds the machine listed count, not only its lease's own", async () => {
    ok(await routing({ priority_lane: "all" }));
    await makeCampaign("fz", { seed_base: 0, chunk: 1000 });
    const nv = await enroll("nv", { channel: "canary" });
    const { cuda, alt } = await gpuRun("sp", { cudaMb: 1500 });
    await env.DB.prepare(`UPDATE runs SET state = 'paused' WHERE run = 'sp'`).run();
    const small = { ...GPU_ON, disk_mb: 600 };
    const fl = await leaseOf(nv, { caps: small, cached: [cuda, alt] });
    expect(fl.kind).toBe("fuzz");
    await env.DB.prepare(`UPDATE runs SET state = 'active' WHERE run = 'sp'`).run();
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    ok(await online(cpu));
    await setRate(cpu, "eval", "run:old", 0.5);
    await campaignRun("cmp", 200, { share: null });
    advance(60); // (10 of 1000 seeds in 60 s: far from done)
    const b = ok(await beat(nv, fl.lease_id, "running", { progress: { games_done: 10, games_total: 1000 } }));
    expect(b.action).toBe("continue");
    // without the CPU PC the batch is behind: then the fuzz lease is given back, and the grant gives the batch
    await env.DB.prepare(`UPDATE machines SET last_seen = ? WHERE machine_id = ?`).bind(clock.now - 400, cpu.machine_id).run();
    advance(30);
    expect(ok(await beat(nv, fl.lease_id, "running", { progress: { games_done: 15, games_total: 1000 } })).action).toBe("give_back");
    ok(await finish(nv, fl.lease_id, [], { given_back: true }));
    expect((await leaseOf(nv, { caps: small, cached: [cuda, alt] })).kind).toBe("eval");
  });
});

describe("a yield the grant didn't follow is not repeated (rev 29, 15.8)", () => {
  // No CPU PC: the batch is never deferred. The PC yields; while it finishes its games another PC takes the whole batch;
  // its lease request gets self-play again (a miss). Then the other PC gives its lease back: the batch waits again, but
  // the lane doesn't send the PC to it before LANE_MISS_S - the grant just didn't follow the last such decision.
  it("a miss: no yield to the same batch for LANE_MISS_S, then again", async () => {
    ok(await routing({ priority_lane: "all" }));
    await gpuRun("sp");
    const nv = await enroll("nv", { channel: "canary" });
    const l = await leaseOf(nv);
    const nv2 = await enroll("nv2", { channel: "canary" });
    const l3 = await leaseOf(nv2);
    await campaignRun("cmp", 16, { share: null }); // one lease of 16: one machine
    expect(yielded(ok(await extend(nv, l.lease_id)))).toBe(true);
    expect((await row(l.lease_id)).pending).toBe("yield:run:cmp");
    expect(yielded(ok(await extend(nv2, l3.lease_id)))).toBe(false); // the batch has its machine
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    const el = await leaseOf(cpu, { caps: CPU_CAPS });
    expect([el.kind, el.job.games]).toEqual(["eval", 16]); // the whole batch
    // a lease request while the yielding lease still finishes (a second slot) settles nothing
    const extra = await leaseOf(nv);
    expect((await laneOf(nv)).yield).toMatchObject({ scope: "run:cmp", batch: "c1" });
    ok(await finish(nv, extra.lease_id, []));
    ok(await finish(nv, l.lease_id, []));
    const l2 = await leaseOf(nv);
    expect(l2.kind).toBe("selfplay");
    expect((await laneOf(nv)).miss).toEqual({ "run:cmp": { batch: "c1", t: clock.now } });
    // the ids come back: the batch waits again
    ok(await finish(cpu, el.lease_id, [], { given_back: true }));
    advance(30);
    expect(yielded(ok(await extend(nv, l2.lease_id)))).toBe(false);
    // another canary PC that never missed it is sent, and gets it
    expect(yielded(ok(await extend(nv2, l3.lease_id)))).toBe(true);
    ok(await finish(nv2, l3.lease_id, []));
    expect((await leaseOf(nv2)).kind).toBe("eval");
    expect(await laneOf(nv2)).toEqual({ yield: null, miss: {} });
    // another batch is another matter
    await campaignRun("cmp-b", 16, { share: null });
    advance(30);
    expect(ok(await extend(nv, l2.lease_id))).toMatchObject({ finish: true, reason: "yield" });
    expect((await row(l2.lease_id)).pending).toBe("yield:run:cmp-b");
  });

  it("the backoff ends after LANE_MISS_S", async () => {
    ok(await routing({ priority_lane: "all" }));
    await gpuRun("sp");
    const nv = await enroll("nv", { channel: "canary" });
    const l = await leaseOf(nv);
    await campaignRun("cmp", 16, { share: null });
    expect(yielded(ok(await extend(nv, l.lease_id)))).toBe(true);
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    const el = await leaseOf(cpu, { caps: CPU_CAPS });
    ok(await finish(nv, l.lease_id, []));
    const l2 = await leaseOf(nv);
    ok(await finish(cpu, el.lease_id, [], { given_back: true }));
    advance(LANE_MISS_S - 60);
    await keep(l2.lease_id);
    expect(yielded(ok(await extend(nv, l2.lease_id)))).toBe(false);
    advance(61);
    expect(yielded(ok(await extend(nv, l2.lease_id)))).toBe(true);
  });
});

describe("a rolling lease gives way only when its drain is short (rev 29, 15.8)", () => {
  async function busyLease(inflight: number, rate: number) {
    ok(await routing({ priority_lane: "all" }));
    await gpuRun("sp");
    const nv = await enroll("nv", { channel: "canary" });
    const l = await leaseOf(nv);
    ok(await beat(nv, l.lease_id, "running", { progress: { games_done: 0, games_total: 1 }, stats: { inflight, concurrency: inflight } }));
    await env.DB.prepare(`UPDATE leases SET rel_hi = lo + ? WHERE lease_id = ?`).bind(inflight, l.lease_id).run(); // its ids in flight
    await setRate(nv, "selfplay", "run:sp", rate);
    return { nv, l };
  }

  it(`no yield when finishing its games in flight takes longer than ${LANE_MAX_DRAIN_S} s`, async () => {
    const { nv, l } = await busyLease(800, 1); // 1.5 x 800 / 1 = 1200 s
    await campaignRun("cmp", 200, { share: null });
    expect(yielded(ok(await extend(nv, l.lease_id)))).toBe(false);
  });

  it("a short drain yields", async () => {
    const { nv, l } = await busyLease(800, 10); // 120 s
    await campaignRun("cmp", 200, { share: null });
    expect(yielded(ok(await extend(nv, l.lease_id)))).toBe(true);
  });

  // A CPU PC on rolling self-play of a CPU-only run (no deferral) and an idle CPU PC that plays the batch at 1 game/s.
  async function cpuFleet(games: number) {
    ok(await routing({ priority_lane: "all" }));
    await cpuRun("cs");
    const a = await enroll("a", { channel: "canary", profile: CPU_ONLY });
    const l = await leaseOf(a, { caps: CPU_CAPS });
    expect([l.kind, !!l.rolling]).toEqual(["selfplay", true]);
    ok(await beat(a, l.lease_id, "running", { progress: { games_done: 0, games_total: 1 }, stats: { inflight: 8, concurrency: 8 } }));
    await setRate(a, "selfplay", "run:cs", 0.04); // 1.5 x 8 / 0.04 = 300 s
    const b = await enroll("b", { profile: CPU_ONLY });
    ok(await online(b));
    await setRate(b, "eval", "run:old", 1);
    await campaignRun("cmp", games, { share: null, gpl: 8 });
    return { a, l, b };
  }

  it("no yield when the other PCs would have played the whole batch before the drain ends", async () => {
    const { a, l } = await cpuFleet(64); // 64 s at 1 game/s, drain 300 s
    expect(yielded(ok(await extend(a, l.lease_id)))).toBe(false);
  });

  it("a yield when the batch outlasts the drain there", async () => {
    const { a, l } = await cpuFleet(400); // 400 s
    expect(yielded(ok(await extend(a, l.lease_id)))).toBe(true);
  });

  it("a stalled batch (nobody took it for eval_stall_s) is a yield whatever those PCs could do", async () => {
    const { a, l, b } = await cpuFleet(64);
    advance(181);
    await keep(l.lease_id);
    ok(await online(b)); // still online, still not taking it
    expect(yielded(ok(await extend(a, l.lease_id)))).toBe(true);
  });
});
