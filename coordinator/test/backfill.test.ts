// Rev 25 (PROTOCOL 16): the backfill tier. A run job with `class: "backfill"` (standing work with no deadline: a
// yardstick for a checkpoint that has none) ranks per resource (16.2): on the machine's best route (its GPU, or any job
// on a Mac) after every tier-1 job and before fuzz without a share; on a Windows PC's CPU after that fuzz and before the
// CPU self-play of a run with a GPU version. A backfill lease that has run 60 s gives its machine back to any job of a
// strictly higher tier, never to other backfill (16.3: give_back at a classic lease's beat, finish "yield" at a rolling
// lease's extension); a fuzz lease past its pace gives way to backfill on the machine's best route (16.4). Backfill is
// offered only where the priority lane applies, never makes the idle poll 60 s, and is not GPU self-play to rev 18's
// eval deferral (16.5).

import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { BACKFILL_CPU_TIER, BACKFILL_TIER, CPU_SELFPLAY_TIER, JOB_CLASSES, YIELD_MIN_S, backfillTier } from "../src/routing";
import {
  type Machine,
  PROFILE,
  admin,
  advance,
  ask,
  beat,
  call,
  clock,
  define,
  enroll,
  err,
  finish,
  lease,
  makeBuild,
  makeCampaign,
  makeRun,
  ok,
  publishCkpt,
  runBody,
} from "./helpers";

const CPU_ONLY = { ...PROFILE, gpus: [], providers: ["cpu"] };
const MAC = "macos-arm64";
const MAC_PROFILE = { os: "macOS 26.1", cpu: "Apple M4 Pro", cores: 12, threads: 12, ram_mb: 24576, disk_free_mb: 300000, gpus: [], providers: ["cpu"], platform: MAC };
const GPU_OK = { gpu: false, gpu_allowed: true };
const GPU_ON = { gpu: true, gpu_allowed: true };
const ROLLING = { inflight: 8, gpu_inflight: 32, block: 1000 }; // max_s: the default 7200 (`until` far away)
const SEATS = { settings: {}, seats: [{ net: 1 }, { bot: "random" }], paired: false };

const routing = (body: unknown) => admin("POST", "/v1/admin/routing", body);
const canary = (name: string, o: { profile?: object; platform?: string } = {}) => enroll(name, { channel: "canary", ...o });
const canaryMac = (name: string) => canary(name, { profile: MAC_PROFILE, platform: MAC });
const row = (id: string) => env.DB.prepare(`SELECT * FROM leases WHERE lease_id = ?`).bind(id).first<any>();
const progress = (games_done: number, games_total: number) => ({ progress: { games_done, games_total } });
const setRate = (m: Machine, scope: string, rate: number) =>
  env.DB.prepare(`INSERT OR REPLACE INTO rates (machine_id, kind, scope, rate, samples, t) VALUES (?, 'eval', ?, ?, 5, ?)`).bind(m.machine_id, scope, rate, clock.now).run();

// A run whose default is the Windows CPU pack, with a CUDA alternate (its GPU route on an NVIDIA PC) and a macOS CPU
// alternate; its self-play and eval batch (`eval` games, leases of 16) carry `class` (default "backfill"; null: none).
// `body(sp, ev)`: the run's PUT body with `sp` merged into its self-play and `ev` (when given) as its eval.
interface Builds {
  def: string;
  alt: string;
  mac: string;
  body: (sp?: Record<string, unknown>, ev?: unknown) => any;
}
async function bfRun(
  run: string,
  o: { selfplay?: "classic" | "rolling" | false; eval?: number; priority?: number; klass?: string | null } = {},
): Promise<Builds> {
  const cpuRt = await makeBuild({ kind: "runtime", tag: `cpu-${run}` });
  const cuda = await makeBuild({ kind: "runtime", tag: `cuda-${run}`, gpu: { vendor: "nvidia", min_driver: "527.41", providers: ["cuda"] } });
  const macRt = await makeBuild({ kind: "runtime", tag: `mac-${run}`, platform: MAC });
  const b = {
    def: await makeBuild({ tag: `w-cpu-${run}`, requires_runtime: cpuRt }),
    alt: await makeBuild({ tag: `w-cuda-${run}`, requires_runtime: cuda }),
    mac: await makeBuild({ tag: `w-mac-${run}`, requires_runtime: macRt, platform: MAC }),
  };
  const klass = o.klass === undefined ? "backfill" : o.klass;
  const sp = o.selfplay ?? false;
  const body = (spExtra: Record<string, unknown> = {}, ev?: unknown) => {
    const x: any = {
      ...runBody(b.def, { games_per_lease: "auto", quota: 100000, priority: o.priority ?? 1, selfplay: sp !== false }),
      runtime: cpuRt,
      alternates: [
        { build_id: b.alt, runtime: cuda, when: { vendor: "nvidia" } },
        { build_id: b.mac, runtime: macRt, when: { platform: MAC } },
      ],
    };
    if (x.selfplay) {
      if (klass) x.selfplay.class = klass;
      if (sp === "rolling") x.selfplay.rolling = ROLLING;
      Object.assign(x.selfplay, spExtra);
    }
    if (ev) x.eval = ev;
    return x;
  };
  ok(await admin("PUT", `/v1/admin/run/${run}`, body()));
  await publishCkpt(run, 1);
  if (o.eval) {
    await publishCkpt(run, 2);
    ok(await admin("PUT", `/v1/admin/run/${run}`, body({}, evalOf(o.eval, klass))));
  }
  await define("run", run, (d) => d.builds.push(b.alt, b.mac));
  return { ...b, body };
}

const evalOf = (games: number, klass: string | null, extra: Record<string, unknown> = {}) => ({
  batch: "bf1",
  games,
  checkpoints: [2, 1],
  games_per_lease: 16,
  ...(klass ? { class: klass } : {}),
  job: { settings: {}, seats: [{ net: 2 }, { net: 1 }], paired: false },
  ...extra,
});

// A live run's eval batch (a CPU build, no self-play, no share): tier-1 work every PC here can play.
const liveEval = (run: string, games = 16) =>
  makeRun(run, { selfplay: false, eval: { batch: "c1", games, checkpoints: [1], games_per_lease: 16, job: SEATS } });

// A fuzz campaign with a macOS alternate (so a Mac can fuzz it too).
async function macCampaign(id: string, share?: number) {
  const win = await makeBuild({ tag: `fuzz-win-${id}` });
  const mb = await makeBuild({ tag: `fuzz-mac-${id}`, platform: MAC });
  const body = { build_id: win, mode: "delver", env: { FUZZ_POOL: "delver" }, seed_base: 0, chunk: 1000, priority: 0.2, ...(share ? { share } : {}), alternates: [{ build_id: mb, when: { platform: MAC } }] };
  ok(await admin("PUT", `/v1/admin/campaign/${id}`, body));
  await define("campaign", id, (d) => d.builds.push(mb));
  return { win, mac: mb };
}

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
const extend = (m: Machine, id: string, games = 8) => call("POST", `/v1/lease/${id}/extend`, { token: m.token, body: { games } });

describe("the class (rev 25, 16.1) and the tiers as units", () => {
  it("class backfill is stored and shown; null leaves it out; any other value, or with a share, is refused", async () => {
    const b = await bfRun("bf", { selfplay: "classic", eval: 32 });
    const got = ok(await admin("GET", "/v1/admin/run/bf"));
    expect([got.selfplay.class, got.eval.class]).toEqual(["backfill", "backfill"]);
    const put = (sp: Record<string, unknown>, ev: Record<string, unknown> | null) => admin("PUT", "/v1/admin/run/bf", b.body(sp, ev && evalOf(32, "backfill", ev)));
    expect(err(await put({ share: 0.25 }, null), 400, "bad_request").message).toMatch(/selfplay.class "backfill" and selfplay.share/);
    expect(err(await put({}, { share: 0.25 }), 400, "bad_request").message).toMatch(/eval.class "backfill" and eval.share/);
    expect(err(await put({ class: "urgent" }, null), 400, "bad_request").message).toMatch(/selfplay.class must be one of backfill/);
    expect(err(await put({}, { class: 1 }), 400, "bad_request").message).toMatch(/eval.class must be one of backfill/);
    // null: a run's job as before (the key left out), and then a share is fine
    ok(await put({ class: null, share: 0.25 }, { class: null }));
    const after = ok(await admin("GET", "/v1/admin/run/bf"));
    expect(["class" in after.selfplay, "class" in after.eval, after.selfplay.share]).toEqual([false, false, 0.25]);
  });

  it("backfillTier: the best route (a GPU lease, any job on a Mac) 1.9, a Windows CPU 2.5; between tier 1, fuzz (2) and tier 3", () => {
    expect(JOB_CLASSES).toEqual(["backfill"]);
    expect([backfillTier(true, "windows-x64"), backfillTier(false, MAC), backfillTier(true, MAC), backfillTier(false, "windows-x64")]).toEqual([1.9, 1.9, 1.9, 2.5]);
    expect(1 < BACKFILL_TIER && BACKFILL_TIER < 2 && 2 < BACKFILL_CPU_TIER && BACKFILL_CPU_TIER < CPU_SELFPLAY_TIER).toBe(true);
    expect(YIELD_MIN_S).toBe(60);
  });
});

describe("where backfill ranks, per resource (16.2)", () => {
  it("on a GPU route before fuzz without a share; on a Windows CPU after it; a live run's work before both", async () => {
    const b = await bfRun("bf", { eval: 64 });
    await makeCampaign("fz", { seed_base: 0, chunk: 1000 });
    // an NVIDIA PC that may use its GPU: the CUDA alternate, rank 4 - before the fuzz past its pace
    const nv = await canary("nv");
    const l = await lease(nv, { caps: GPU_OK });
    expect([l.kind, l.run, l.build.build_id]).toEqual(["eval", "bf", b.alt]);
    // the same PC without GPU work allowed would play it on its CPU (rank 6): the fuzz first
    expect((await lease(await canary("nv-cpu"), { caps: { gpu: false, gpu_allowed: false } })).kind).toBe("fuzz");
    // a CPU-only Windows PC: the fuzz first
    expect((await lease(await canary("cpu", { profile: CPU_ONLY }))).kind).toBe("fuzz");
    // a live run's evaluation (tier 1) goes before backfill on any route
    await liveEval("live");
    expect((await lease(await canary("nv2"), { caps: GPU_OK })).run).toBe("live");
  });

  it("fuzz behind its pace (a campaign with a share) goes before backfill on every route", async () => {
    await bfRun("bf", { eval: 64 });
    await makeCampaign("fz-s", { seed_base: 0, chunk: 1000, share: 0.5 });
    expect((await lease(await canary("nv"), { caps: GPU_OK })).kind).toBe("fuzz");
    expect((await lease(await canaryMac("mac"))).kind).toBe("eval"); // no macOS build of that campaign: backfill
  });

  it("a Mac takes backfill (its CPU is its best route, rank 4) before fuzz past its pace; a Windows CPU PC the fuzz", async () => {
    const b = await bfRun("bf", { eval: 64 });
    const fz = await macCampaign("fz");
    const mac = await canaryMac("mac");
    const l = await lease(mac);
    expect([l.kind, l.run, l.build.build_id]).toEqual(["eval", "bf", b.mac]);
    const w = await lease(await canary("cpu", { profile: CPU_ONLY }));
    expect([w.kind, w.build.build_id]).toEqual(["fuzz", fz.win]);
  });

  it("on a Windows CPU before the CPU self-play of a run with a GPU version (tier 3)", async () => {
    await bfRun("sp", { selfplay: "classic", klass: null }); // a live run with a CUDA alternate
    await bfRun("bf", { eval: 64 });
    const cpu = await canary("cpu", { profile: CPU_ONLY });
    expect((await lease(cpu)).run).toBe("bf");
    // on its GPU the live run's self-play goes first
    expect((await lease(await canary("nv"), { caps: GPU_OK })).run).toBe("sp");
  });
});

describe("gating by the priority lane (16.5)", () => {
  it("off: no backfill anywhere; canary: only canary machines; all: every machine", async () => {
    await bfRun("bf", { eval: 64 });
    await bfRun("bfs", { selfplay: "classic" });
    const st = await enroll("stable");
    const ca = await canary("canary");
    ok(await routing({ priority_lane: "off" }));
    expect(ok(await ask(ca, { caps: GPU_OK }))).toEqual({ wait_s: 300, reason: "no_work" });
    ok(await routing({ priority_lane: "canary" }));
    expect(ok(await ask(st, { caps: GPU_OK }))).toEqual({ wait_s: 300, reason: "no_work" });
    expect(["bf", "bfs"]).toContain((await lease(ca, { caps: GPU_OK })).run);
    ok(await routing({ priority_lane: "all" }));
    expect(["bf", "bfs"]).toContain((await lease(st, { caps: GPU_OK })).run);
  });
});

describe("a backfill lease gives its machine back (16.3)", () => {
  it("a classic lease: give_back at a beat once it has run 60 s and a tier-1 job waits; repeated; then the machine gets that job", async () => {
    await bfRun("bf", { eval: 16 }); // one lease: every id issued, the batch leaves the offer
    const nv = await canary("nv");
    const l = await lease(nv, { caps: GPU_OK });
    expect(l.run).toBe("bf");
    await liveEval("live");
    advance(30);
    expect(ok(await beat(nv, l.lease_id)).action).toBe("continue"); // under 60 s
    advance(31);
    expect(ok(await beat(nv, l.lease_id)).action).toBe("give_back");
    expect((await row(l.lease_id)).pending).toBe("yield:run:live");
    expect(ok(await beat(nv, l.lease_id)).action).toBe("give_back"); // a lost answer is repeated
    expect(ok(await finish(nv, l.lease_id, [], { given_back: true })).returned).toBe(16);
    expect((await lease(nv, { caps: GPU_OK })).run).toBe("live");
  });

  it("only to a strictly higher tier: not to fuzz past its pace (on a GPU route), never to other backfill; to fuzz behind its pace", async () => {
    await bfRun("bf", { eval: 16 });
    const nv = await canary("nv");
    const l = await lease(nv, { caps: GPU_OK });
    advance(61);
    await makeCampaign("fz", { seed_base: 0, chunk: 1000 });
    expect(ok(await beat(nv, l.lease_id)).action).toBe("continue");
    await bfRun("bf2", { eval: 64, priority: 9 }); // backfill with a higher priority
    expect(ok(await beat(nv, l.lease_id)).action).toBe("continue");
    await makeCampaign("fz-s", { seed_base: 10_000_000, chunk: 1000, share: 0.5 });
    expect(ok(await beat(nv, l.lease_id)).action).toBe("give_back");
    expect((await row(l.lease_id)).pending).toBe("yield:campaign:fz-s");
  });

  it("on a Windows CPU (rank 6) it gives way to fuzz past its pace (rank 5)", async () => {
    await bfRun("bf", { eval: 64 });
    const cpu = await canary("cpu", { profile: CPU_ONLY });
    const l = await lease(cpu);
    expect(l.run).toBe("bf");
    await makeCampaign("fz", { seed_base: 0, chunk: 1000 });
    expect(ok(await beat(cpu, l.lease_id)).action).toBe("continue"); // under 60 s
    advance(61);
    expect(ok(await beat(cpu, l.lease_id)).action).toBe("give_back");
    expect((await row(l.lease_id)).pending).toBe("yield:campaign:fz");
  });

  it("whatever the lane's setting now (the lease was granted under it)", async () => {
    await bfRun("bf", { eval: 16 });
    const nv = await canary("nv");
    const l = await lease(nv, { caps: GPU_OK });
    await bfRun("bfs", { selfplay: "rolling" });
    const nv2 = await canary("nv2");
    const r = ok(await askRolling(nv2)).lease;
    expect([r.run, !!r.rolling]).toEqual(["bfs", true]);
    ok(await routing({ priority_lane: "off" }));
    await liveEval("live", 32);
    advance(61);
    expect(ok(await beat(nv, l.lease_id)).action).toBe("give_back");
    expect(ok(await extend(nv2, r.lease_id))).toMatchObject({ finish: true, reason: "yield" });
  });

  it("a rolling self-play: finish with reason yield at an extension once it has run 60 s; to a live run's self-play too; never to backfill", async () => {
    await bfRun("bf", { selfplay: "rolling" });
    const nv = await canary("nv");
    const b = ok(await askRolling(nv)).lease;
    expect([b.run, !!b.rolling]).toEqual(["bf", true]);
    await bfRun("bf2", { selfplay: "rolling", priority: 9 });
    advance(61);
    expect(ok(await extend(nv, b.lease_id)).finish).toBeUndefined(); // other backfill: no
    await makeRun("live", { quota: 100000, games_per_lease: 10, priority: 5 }); // a live run's self-play with headroom
    expect(ok(await extend(nv, b.lease_id))).toMatchObject({ finish: true, reason: "yield" });
    expect((await row(b.lease_id)).pending).toBe("yield:run:live");
    expect(ok(await extend(nv, b.lease_id))).toMatchObject({ finish: true, reason: "state" });
  });

  it("a rolling self-play is not asked in its first 60 s; then a waiting batch ends it", async () => {
    await bfRun("bf", { selfplay: "rolling" });
    const nv = await canary("nv");
    const b = ok(await askRolling(nv)).lease;
    await liveEval("live");
    advance(30);
    expect(ok(await extend(nv, b.lease_id)).finish).toBeUndefined();
    advance(31);
    expect(ok(await extend(nv, b.lease_id))).toMatchObject({ finish: true, reason: "yield" });
    expect((await row(b.lease_id)).pending).toBe("yield:run:live");
  });
});

describe("fuzz past its pace gives way to backfill on the machine's best route (16.4)", () => {
  it("a GPU PC's and a Mac's fuzz lease answer give_back; a Windows CPU PC's and a stable PC's (canary lane) don't", async () => {
    await makeCampaign("fz", { seed_base: 0, chunk: 1000 });
    await macCampaign("fz-mac");
    const nv = await canary("nv");
    const cpu = await canary("cpu", { profile: CPU_ONLY });
    const st = await enroll("stable");
    const mac = await canaryMac("mac");
    const fl = { nv: await lease(nv, { caps: GPU_OK }), cpu: await lease(cpu), st: await lease(st, { caps: GPU_OK }), mac: await lease(mac) };
    for (const x of Object.values(fl)) expect(x.kind).toBe("fuzz");
    await bfRun("bf", { eval: 64 });
    advance(60);
    expect(ok(await beat(nv, fl.nv.lease_id, "running", progress(100, 1000))).action).toBe("give_back");
    expect((await row(fl.nv.lease_id)).pending).toBe("yield:run:bf");
    expect(ok(await beat(mac, fl.mac.lease_id, "running", progress(100, 1000))).action).toBe("give_back");
    expect(ok(await beat(cpu, fl.cpu.lease_id, "running", progress(100, 1000))).action).toBe("continue");
    expect(ok(await beat(st, fl.st.lease_id, "running", progress(100, 1000))).action).toBe("continue");
    ok(await finish(nv, fl.nv.lease_id, [], { given_back: true }));
    expect((await lease(nv, { caps: GPU_OK })).run).toBe("bf");
  });

  it("backfill takes no busy machine from anything but fuzz: a rolling CPU self-play below it (tier 3) runs to its renewal", async () => {
    await bfRun("sp", { selfplay: "rolling", klass: null }); // a live run with a GPU version
    const cpu = await canary("cpu", { profile: CPU_ONLY });
    const r = ok(await askRolling(cpu, { gpu: false, gpu_allowed: false })).lease;
    expect([r.run, !!r.rolling]).toEqual(["sp", true]);
    await bfRun("bf", { eval: 64 });
    advance(61);
    expect(ok(await extend(cpu, r.lease_id)).finish).toBeUndefined();
  });

  it("fuzz behind its pace (a share) keeps its machine", async () => {
    await makeCampaign("fz-s", { seed_base: 0, chunk: 1000, share: 0.5 });
    const nv = await canary("nv");
    const fl = await lease(nv, { caps: GPU_OK });
    await bfRun("bf", { eval: 64 });
    advance(60);
    expect(ok(await beat(nv, fl.lease_id, "running", progress(100, 1000))).action).toBe("continue");
  });
});

describe("the idle poll and rev 18's bookkeeping (16.5)", () => {
  it("no_work stays 300 s while only a backfill batch has leases out", async () => {
    await bfRun("bf", { eval: 16 });
    expect((await lease(await canary("a"), { caps: GPU_OK })).run).toBe("bf");
    expect(ok(await ask(await canary("b"), { caps: GPU_OK }))).toEqual({ wait_s: 300, reason: "no_work" });
  });

  it("a backfill self-play on a PC's GPU doesn't set gpu_play: that PC counts as CPU eval capacity, its own batches aren't deferred", async () => {
    await bfRun("bf", { selfplay: "classic" });
    const bfpc = await canary("bfpc");
    expect((await lease(bfpc, { caps: GPU_OK })).run).toBe("bf");
    expect((await env.DB.prepare(`SELECT gpu_play FROM machines WHERE machine_id = ?`).bind(bfpc.machine_id).first<any>()).gpu_play).toBe(0);
    // a live run with a CUDA alternate and a 400-game batch; the backfill PC plays 1 game/s: 1800 s of it covers 400
    await bfRun("sp", { selfplay: "classic", klass: null, eval: 400 });
    await setRate(bfpc, "run:sp", 1);
    // another GPU PC on the live run's self-play: the batch is not behind (the backfill PC counts), so after its self-play
    expect((await lease(await canary("nv2"), { caps: GPU_OK })).kind).toBe("selfplay");
  });
});
