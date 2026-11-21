// Rev 24 (PROTOCOL 15): the priority lane. Waiting eval work (an analysis campaign's batch, a run's evaluation) starts in
// minutes instead of waiting for fuzz leases and rolling self-play to end on their own: an idle machine asks again after
// 60 s while eval work is about to need it (15.2); a fuzz lease is given back at its next beat when a batch its machine
// could play waits (15.3); a rolling self-play lease finishes at its next extension when the batch would take its
// machine within its share, never more machines than the batch can use (15.4); under the lane the eval deferral counts
// a PC busy on fuzz as capacity only when the lane frees it, and a batch nobody has taken for eval_stall_s is behind
// (15.5). The owner's setting `priority_lane` (15.6): "canary" (default) preempts only canary machines, "all" every
// machine, "off" restores rev 23 exactly.

import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { fuzzAlmostDone } from "../src/friend";
import { HW_ROUTING_DEFAULT, batchMachines, hwRoutingOf, laneApplies, sortsBefore, yieldTarget } from "../src/routing";
import {
  type Machine,
  PROFILE,
  admin,
  advance,
  ask,
  beat,
  call,
  clock,
  complete,
  define,
  enroll,
  err,
  finish,
  idsOf,
  lease,
  makeBuild,
  makeCampaign,
  makeRun,
  ok,
  publishCkpt,
  runBody,
  upload,
} from "./helpers";

const CPU_ONLY = { ...PROFILE, gpus: [], providers: ["cpu"] };
const MAC = "macos-arm64";
const MAC_PROFILE = { os: "macOS 26.1", cpu: "Apple M4 Pro", cores: 12, threads: 12, ram_mb: 24576, disk_free_mb: 300000, gpus: [], providers: ["cpu"], platform: MAC };
const GPU_OK = { gpu: false, gpu_allowed: true };
const GPU_ON = { gpu: true, gpu_allowed: true }; // a CUDA probe passed (the stored caps the lane reads)
const ROLLING = { inflight: 8, gpu_inflight: 32, block: 1000 }; // max_s: the default 7200 (`until` far away)
const SEATS = { settings: {}, seats: [{ net: 1 }, { bot: "random" }], paired: false };

const routing = (body: unknown) => admin("POST", "/v1/admin/routing", body);
const canary = (name: string, o: { profile?: object } = {}) => enroll(name, { channel: "canary", ...o });
const toChannel = async (m: Machine, channel: string) => ok(await admin("POST", `/v1/admin/machine/${m.machine_id}`, { channel }));
const online = async (m: Machine) => ok(await ask(m, { kinds: [] }));
const setRate = (m: Machine, scope: string, rate: number) =>
  env.DB.prepare(`INSERT OR REPLACE INTO rates (machine_id, kind, scope, rate, samples, t) VALUES (?, 'eval', ?, ?, 5, ?)`).bind(m.machine_id, scope, rate, clock.now).run();
const row = (id: string) => env.DB.prepare(`SELECT * FROM leases WHERE lease_id = ?`).bind(id).first<any>();
const fuzzResult = (first: number, count: number, done: number) =>
  JSON.stringify({ campaign: "fz", mode: "delver", first, count, done, failures: [], build_id: "b", engine_commit: "e", final: true });
const progress = (games_done: number, games_total: number) => ({ progress: { games_done, games_total } });

// An analysis campaign's run (PROTOCOL 5.3): no self-play, one eval batch of `games` (a CPU build, unpaired, leases of
// `gpl` games), share 0.25 unless given (null: none).
async function campaignRun(run: string, games: number, o: { share?: number | null; gpl?: number; needs?: { threads: number; ram_mb: number } } = {}) {
  const share = o.share === undefined ? 0.25 : o.share;
  await makeRun(run, {
    selfplay: false,
    eval: { batch: "c1", games, checkpoints: [1], games_per_lease: o.gpl ?? 16, ...(share === null ? {} : { share }), ...(o.needs ? { needs: o.needs } : {}), job: SEATS },
  });
}

// A self-play run whose default is the CPU pack, with the CUDA alternate (a GPU version, PROTOCOL 12.1); optionally its
// own eval batch and rolling self-play.
async function gpuRun(run: string, o: { eval?: unknown; rolling?: unknown } = {}) {
  const cpuRt = await makeBuild({ kind: "runtime", tag: `cpu-${run}` });
  const cuda = await makeBuild({ kind: "runtime", tag: `cuda-${run}`, gpu: { vendor: "nvidia", min_driver: "527.41", providers: ["cuda"] } });
  const def = await makeBuild({ tag: `w-cpu-${run}`, requires_runtime: cpuRt });
  const alt = await makeBuild({ tag: `w-cuda-${run}`, requires_runtime: cuda });
  const body = (extra: Record<string, unknown> = {}) => {
    const b: any = { ...runBody(def, { games_per_lease: "auto", quota: 100000 }), runtime: cpuRt, alternates: [{ build_id: alt, runtime: cuda, when: { vendor: "nvidia" } }], ...extra };
    if (o.rolling) b.selfplay.rolling = o.rolling;
    return b;
  };
  ok(await admin("PUT", `/v1/admin/run/${run}`, body()));
  await publishCkpt(run, 1);
  if (o.eval) {
    await publishCkpt(run, 2);
    ok(await admin("PUT", `/v1/admin/run/${run}`, body({ eval: o.eval })));
  }
  await define("run", run, (d) => d.builds.push(alt));
}
const EVAL400 = { batch: "b1", games: 400, checkpoints: [2, 1], games_per_lease: "auto", job: { settings: { sims: 32 }, seats: [{ net: 2 }, { net: 1 }], paired: true } };

function askRolling(m: Machine, caps: Record<string, unknown> = GPU_ON) {
  return call("POST", "/v1/lease", {
    token: m.token,
    agent: "0.2.13",
    body: {
      profile_digest: "ab".repeat(32),
      profile: null,
      caps: { threads: 6, ram_mb: 12000, gpu: false, disk_mb: 2500, ...caps },
      cached: { builds: [], ckpts: [] },
      kinds: ["fuzz", "eval", "selfplay", "diag"],
      features: ["rolling"],
    },
  });
}
async function rollingLease(m: Machine, caps?: Record<string, unknown>) {
  const b = ok(await askRolling(m, caps));
  expect(b.lease, JSON.stringify(b)).toBeTruthy();
  expect(b.lease.rolling, "a rolling lease").toBeTruthy();
  return b.lease;
}
const extend = (m: Machine, id: string, games = 8) => call("POST", `/v1/lease/${id}/extend`, { token: m.token, body: { games } });

describe("the owner's setting and the rules as units (rev 24, 15.6)", () => {
  it("priority_lane defaults to canary, eval_stall_s to 180; stored values are checked; POST validates", async () => {
    expect(HW_ROUTING_DEFAULT).toMatchObject({ priority_lane: "canary", eval_stall_s: 180 });
    expect(hwRoutingOf(null)).toMatchObject({ priority_lane: "canary", eval_stall_s: 180 });
    expect(hwRoutingOf(JSON.stringify({ priority_lane: "all", eval_stall_s: 0 }))).toMatchObject({ priority_lane: "all", eval_stall_s: 0 });
    expect(hwRoutingOf(JSON.stringify({ priority_lane: "maybe", eval_stall_s: -5 }))).toMatchObject({ priority_lane: "canary", eval_stall_s: 180 });
    // a setting stored before rev 24 (no such keys) reads as the defaults
    expect(hwRoutingOf(JSON.stringify({ on: true, eval_horizon_s: 900, eval_games_per_thread: 2 }))).toMatchObject({ priority_lane: "canary", eval_horizon_s: 900 });
    expect(ok(await routing({ priority_lane: "off" }))).toMatchObject({ ok: true, priority_lane: "off", eval_stall_s: 180, eval_horizon_s: 1800 });
    expect(ok(await routing({ eval_stall_s: 0 }))).toMatchObject({ priority_lane: "off", eval_stall_s: 0 });
    expect(ok(await admin("GET", "/v1/admin/routing"))).toMatchObject({ priority_lane: "off", eval_stall_s: 0, stored: true });
    for (const bad of [{ priority_lane: "maybe" }, { priority_lane: 1 }, { eval_stall_s: -1 }, { eval_stall_s: 1.5 }, { eval_stall_s: 86401 }]) {
      err(await routing(bad), 400, "bad_request");
    }
  });

  it("laneApplies, sortsBefore, batchMachines, yieldTarget, fuzzAlmostDone", () => {
    const hw = (priority_lane: "off" | "canary" | "all") => ({ ...HW_ROUTING_DEFAULT, priority_lane });
    expect([laneApplies(hw("off"), "canary"), laneApplies(hw("canary"), "canary"), laneApplies(hw("canary"), "stable"), laneApplies(hw("all"), "stable")]).toEqual([
      false,
      true,
      false,
      true,
    ]);
    expect(sortsBefore({ tier: 1, rank: 2, score: 0 }, { tier: 2, rank: 0, score: 9 })).toBe(true); // tier first
    expect(sortsBefore({ tier: 1, rank: 0, score: 0 }, { tier: 1, rank: 1, score: 9 })).toBe(true); // then rank
    expect(sortsBefore({ tier: 1, rank: 1.5, score: 9 }, { tier: 1, rank: 1, score: 0 })).toBe(false); // a deferred batch
    expect(sortsBefore({ tier: 1, rank: 0, score: 2 }, { tier: 1, rank: 0, score: 1 })).toBe(true); // then score
    expect(sortsBefore({ tier: 1, rank: 0, score: 1 }, { tier: 1, rank: 0, score: 1 })).toBe(false);
    expect([batchMachines(64, 16), batchMachines(65, 16), batchMachines(64, 128), batchMachines(1, 0)]).toEqual([4, 5, 1, 1]);
    expect([yieldTarget("yield:run:cmp-x"), yieldTarget("give_back"), yieldTarget(null)]).toEqual(["run:cmp-x", null, null]);
    const l = { issued_at: 1000, paused_s: 0, progress: null };
    expect(fuzzAlmostDone(l, { games_done: 100, games_total: 1000 }, 1060)).toBe(false); // 540 s left
    expect(fuzzAlmostDone(l, { games_done: 990, games_total: 1000 }, 1060)).toBe(true); // under a second left
    expect(fuzzAlmostDone(l, { games_done: 500, games_total: 1000 }, 1100)).toBe(true); // 100 s left: within FUZZ_TAIL_S
    expect(fuzzAlmostDone(l, { games_done: 0, games_total: 1000 }, 1060)).toBe(false); // no pace yet
    expect(fuzzAlmostDone(l, undefined, 1060)).toBe(false); // no progress: not known
    expect(fuzzAlmostDone({ ...l, progress: JSON.stringify({ games_done: 999, games_total: 1000 }) }, undefined, 1060)).toBe(true); // the last beat's
    expect(fuzzAlmostDone({ ...l, paused_s: 50 }, { games_done: 100, games_total: 1000 }, 1060)).toBe(true); // 10 s running: 90 s left
  });
});

describe("idle poll (15.2)", () => {
  it("no_work says 60 s while a batch the machine could play has leases out, 300 s once it is done", async () => {
    await campaignRun("cmp", 4, { gpl: 4 });
    const a = await enroll("a");
    const la = await lease(a);
    expect([la.kind, la.job.games]).toEqual(["eval", 4]);
    const b = await enroll("b");
    expect(ok(await ask(b))).toEqual({ wait_s: 60, reason: "no_work" });
    // a machine that couldn't play the batch (an agent not offering eval) waits as before
    expect(ok(await ask(await enroll("c"), { kinds: ["fuzz", "selfplay"] }))).toEqual({ wait_s: 300, reason: "no_work" });
    await complete(a, la);
    expect(ok(await ask(b))).toEqual({ wait_s: 300, reason: "no_work" });
  });

  it("60 s while a run without self-play or a batch was put in the last 10 minutes (a campaign registering), for a machine of its platform", async () => {
    await makeRun("cmp-new", { selfplay: false }); // its batch comes once its checkpoints are registered
    const pc = await enroll("pc");
    expect(ok(await ask(pc))).toEqual({ wait_s: 60, reason: "no_work" });
    const mac = await enroll("mac", { profile: MAC_PROFILE, platform: MAC });
    expect(ok(await ask(mac)).wait_s).toBe(300); // no macOS build: nothing for it there
    advance(601);
    expect(ok(await ask(pc)).wait_s).toBe(300);
  });

  it("priority_lane off: 300 s as before", async () => {
    ok(await routing({ priority_lane: "off" }));
    await campaignRun("cmp", 4, { gpl: 4 });
    await lease(await enroll("a"));
    expect(ok(await ask(await enroll("b")))).toEqual({ wait_s: 300, reason: "no_work" });
  });
});

describe("fuzz leases give way to waiting eval work (15.3)", () => {
  it("a canary machine's fuzz lease answers give_back at its next beat; finished seeds stay credited, the rest go back; then it gets the batch", async () => {
    await makeCampaign("fz", { seed_base: 0, chunk: 1000 });
    const m = await canary("m", { profile: CPU_ONLY });
    const fl = await lease(m);
    expect(fl.kind).toBe("fuzz");
    expect(ok(await beat(m, fl.lease_id, "running", progress(50, 1000))).action).toBe("continue"); // nothing waits yet
    await campaignRun("cmp", 16);
    advance(60);
    expect(ok(await beat(m, fl.lease_id, "running", progress(100, 1000))).action).toBe("give_back");
    expect((await row(fl.lease_id)).pending).toBe("yield:run:cmp");
    // a lost answer: the next beat says it again
    expect(ok(await beat(m, fl.lease_id, "running", progress(110, 1000))).action).toBe("give_back");
    // the agent (any version: lease.rs `give_back` -> quit, then the final fuzz result with the seeds it covered, then done)
    const [first] = idsOf(fl);
    ok(await upload(m, fl.lease_id, 0, fuzzResult(first, 1000, 110), { ids: [[first, first + 110]] }));
    expect(ok(await finish(m, fl.lease_id, [0], { given_back: true })).returned).toBe(890);
    const claims = (await env.DB.prepare(`SELECT lo, hi FROM claims WHERE scope = 'campaign:fz'`).all<any>()).results;
    expect(claims).toEqual([{ lo: first, hi: first + 110 }]);
    const pool = (await env.DB.prepare(`SELECT lo, hi FROM pool WHERE scope = 'campaign:fz'`).all<any>()).results;
    expect(pool).toEqual([{ lo: first + 110, hi: first + 1000 }]);
    const l = await lease(m);
    expect([l.kind, l.run, l.job.games]).toEqual(["eval", "cmp", 16]);
  });

  it("only when its machine could play the batch (the caps it last asked with)", async () => {
    await makeCampaign("fz", { seed_base: 0, chunk: 1000 });
    const small = await canary("small", { profile: CPU_ONLY });
    const big = await canary("big", { profile: CPU_ONLY });
    const ls = await lease(small, { caps: { threads: 6 } });
    const lb = await lease(big, { caps: { threads: 16 } });
    await campaignRun("cmp", 16, { needs: { threads: 16, ram_mb: 1000 } });
    advance(60);
    expect(ok(await beat(small, ls.lease_id, "running", progress(100, 1000))).action).toBe("continue");
    expect(ok(await beat(big, lb.lease_id, "running", progress(100, 1000))).action).toBe("give_back");
  });

  it("never more machines than the batch can use: its unissued games over the lease size, machines on their way counted", async () => {
    await makeCampaign("fz", { seed_base: 0, chunk: 1000 });
    const a = await canary("a", { profile: CPU_ONLY });
    const b = await canary("b", { profile: CPU_ONLY });
    const la = await lease(a);
    const lb = await lease(b);
    await campaignRun("cmp", 16); // one lease of 16
    advance(60);
    expect(ok(await beat(a, la.lease_id, "running", progress(100, 1000))).action).toBe("give_back");
    expect(ok(await beat(b, lb.lease_id, "running", progress(100, 1000))).action).toBe("continue");
    // a takes the batch: nothing unissued, nothing waits
    ok(await finish(a, la.lease_id, [], { given_back: true }));
    expect((await lease(a)).run).toBe("cmp");
    advance(30);
    expect(ok(await beat(b, lb.lease_id, "running", progress(150, 1000))).action).toBe("continue");
  });

  it("a fuzz lease about to finish anyway is left alone; a paused one too", async () => {
    await makeCampaign("fz", { seed_base: 0, chunk: 1000 });
    const m = await canary("m", { profile: CPU_ONLY });
    const fl = await lease(m);
    await campaignRun("cmp", 16);
    advance(60);
    expect(ok(await beat(m, fl.lease_id, "running", progress(990, 1000))).action).toBe("continue");
    expect(ok(await beat(m, fl.lease_id, "paused", progress(100, 1000))).action).toBe("continue");
  });

  it("canary: a stable machine's fuzz lease is not given back; all: it is; off: no machine's", async () => {
    await makeCampaign("fz", { seed_base: 0, chunk: 1000 });
    const st = await enroll("stable", { profile: CPU_ONLY });
    const ca = await canary("canary", { profile: CPU_ONLY });
    const ls = await lease(st);
    const lc = await lease(ca);
    await campaignRun("cmp", 64);
    ok(await routing({ priority_lane: "off" }));
    advance(30);
    expect(ok(await beat(ca, lc.lease_id, "running", progress(100, 1000))).action).toBe("continue");
    ok(await routing({ priority_lane: "canary" }));
    advance(30);
    expect(ok(await beat(st, ls.lease_id, "running", progress(100, 1000))).action).toBe("continue");
    expect(ok(await beat(ca, lc.lease_id, "running", progress(110, 1000))).action).toBe("give_back");
    ok(await routing({ priority_lane: "all" }));
    advance(30);
    expect(ok(await beat(st, ls.lease_id, "running", progress(120, 1000))).action).toBe("give_back");
  });

  it("not for work the lane doesn't cover: another run's self-play with headroom ahead of the batch keeps the fuzz lease", async () => {
    await makeCampaign("fz", { seed_base: 0, chunk: 1000 });
    const m = await canary("m", { profile: CPU_ONLY });
    const fl = await lease(m, { kinds: ["fuzz"] });
    await makeRun("live", { quota: 100000, games_per_lease: 10, priority: 5 });
    await campaignRun("cmp", 16, { share: 0.25 }); // past its share with no run lease out (rank 2): after the self-play
    advance(60);
    expect(ok(await beat(m, fl.lease_id, "running", progress(100, 1000))).action).toBe("continue");
  });
});

describe("rolling self-play yields at its next extension (15.4)", () => {
  // Four canary NVIDIA PCs on the live run's rolling self-play (on their GPUs); no CPU PC, so a batch is never deferred.
  async function fleet(names = ["n1", "n2", "n3", "n4"], channel = "canary") {
    await gpuRun("sp", { rolling: ROLLING });
    const out: { m: Machine; l: any }[] = [];
    for (const n of names) {
      const m = await enroll(n, { channel });
      out.push({ m, l: await rollingLease(m) });
    }
    return out;
  }
  const answers = async (f: { m: Machine; l: any }[]) => {
    const got: any[] = [];
    for (const { m, l } of f) got.push(ok(await extend(m, l.lease_id)));
    return got;
  };
  const yielded = (got: any[]) => got.filter((a) => a.finish && a.reason === "yield").length;

  it("finishes (reason yield) before `until`; later extensions say state, beats go on; the machine then gets the batch", async () => {
    const f = await fleet();
    await campaignRun("cmp", 16, { share: null });
    const { m, l } = f[0];
    expect(ok(await extend(m, l.lease_id))).toEqual({ games: 32, finish: true, reason: "yield" });
    expect((await row(l.lease_id)).pending).toBe("yield:run:cmp");
    expect(ok(await extend(m, l.lease_id))).toMatchObject({ finish: true, reason: "state" });
    expect(ok(await beat(m, l.lease_id)).action).toBe("continue");
    // the others: the batch (16 games, one lease) has its machine
    expect(yielded(await answers(f.slice(1)))).toBe(0);
    ok(await finish(m, l.lease_id, []));
    const e = ok(await askRolling(m)).lease;
    expect([e.kind, e.run]).toEqual(["eval", "cmp"]);
  });

  it("within the batch's share: share 0.25 of four run leases takes one machine", async () => {
    const f = await fleet();
    await campaignRun("cmp", 64, { share: 0.25 });
    expect(yielded(await answers(f))).toBe(1);
  });

  it("only as many as the batch can use: 32 games in leases of 16 take two machines, whatever the share", async () => {
    const f = await fleet();
    await campaignRun("cmp", 32, { share: 1 });
    expect(yielded(await answers(f))).toBe(2);
  });

  it("several campaigns together take no more of the live run than one share would", async () => {
    const f = await fleet();
    // another campaign already holds a lease (on a CPU PC, which takes it before its tier-3 self-play): 1 of 5 run leases
    await campaignRun("cmp-b", 64, { share: 0.25 });
    expect((await lease(await enroll("cpu", { profile: CPU_ONLY }))).run).toBe("cmp-b");
    // cmp-a alone would be within its own share (1 of 5 <= 0.25 x 5), but the two together would hold 2 of 5
    await campaignRun("cmp-a", 64, { share: 0.25 });
    expect(yielded(await answers(f))).toBe(0);
  });

  it("a deferred batch (CPU PCs can finish it in the horizon) makes no GPU self-play PC yield", async () => {
    const f = await fleet(["n1", "n2"]);
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    await online(cpu);
    await setRate(cpu, "run:cmp", 1); // 1 game/s x 1800 s >= 64
    await campaignRun("cmp", 64, { share: null });
    expect(yielded(await answers(f))).toBe(0);
  });

  it("canary: a stable machine's rolling lease is not yielded; all: it is; off: renewal only, as before", async () => {
    const f = await fleet(["s1", "s2"], "stable");
    await campaignRun("cmp", 64, { share: null });
    expect(yielded(await answers(f))).toBe(0);
    ok(await routing({ priority_lane: "off" }));
    await toChannel(f[0].m, "canary");
    const off = ok(await extend(f[0].m, f[0].l.lease_id));
    expect(off.finish).toBeUndefined();
    ok(await routing({ priority_lane: "all" }));
    expect(ok(await extend(f[1].m, f[1].l.lease_id))).toMatchObject({ finish: true, reason: "yield" });
  });
});

describe("eval deferral under the lane (15.5)", () => {
  it("a CPU PC busy on fuzz counts only when the lane gives its fuzz back", async () => {
    await gpuRun("sp", { eval: EVAL400 });
    await makeCampaign("fz", { seed_base: 0, chunk: 1000 });
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    expect((await lease(cpu, { kinds: ["fuzz"] })).kind).toBe("fuzz");
    await setRate(cpu, "run:sp", 1); // 1 game/s x 1800 s >= 400: not behind, by rev 18's rule
    // a stable GPU PC (the lane isn't its): rev 23's rule, the busy CPU PC counts
    expect((await lease(await enroll("nv-stable"), { caps: GPU_OK })).kind).toBe("selfplay");
    // a canary GPU PC: the CPU PC's fuzz lease (stable, so never given back) holds it: no capacity, behind
    expect((await lease(await canary("nv1"), { caps: GPU_OK })).kind).toBe("eval");
    // the CPU PC on canary: the lane gives its fuzz back, so it counts
    await toChannel(cpu, "canary");
    expect((await lease(await canary("nv2"), { caps: GPU_OK })).kind).toBe("selfplay");
    // off: rev 23 exactly
    await toChannel(cpu, "stable");
    ok(await routing({ priority_lane: "off" }));
    expect((await lease(await canary("nv3"), { caps: GPU_OK })).kind).toBe("selfplay");
  });

  it("a batch nobody took or worked on for eval_stall_s is behind; one with a lease out is not; 0 turns the rule off", async () => {
    await gpuRun("sp", { eval: EVAL400 });
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    await online(cpu);
    await setRate(cpu, "run:sp", 1);
    expect((await lease(await canary("nv1"), { caps: GPU_OK })).kind).toBe("selfplay");
    advance(181);
    await online(cpu); // still online and idle: counted, yet it never took the batch
    ok(await routing({ eval_stall_s: 0 }));
    expect((await lease(await canary("nv2"), { caps: GPU_OK })).kind).toBe("selfplay");
    ok(await routing({ eval_stall_s: 180 }));
    expect((await lease(await enroll("nv-stable"), { caps: GPU_OK })).kind).toBe("selfplay"); // not a lane machine
    const nv3 = await canary("nv3");
    expect((await lease(nv3, { caps: GPU_OK })).kind).toBe("eval");
    // a lease of the batch is out: it is being worked on, the capacity rule decides again
    expect((await lease(await canary("nv4"), { caps: GPU_OK })).kind).toBe("selfplay");
  });
});
