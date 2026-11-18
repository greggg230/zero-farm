// Rev 18 (PROTOCOL 12): hardware routing. A PC that would play a run's self-play on its CPU while the run has a GPU
// version takes that self-play last (after fuzz); a PC that plays a run's self-play on its GPU takes an eval batch after
// that self-play unless the CPU PCs online couldn't finish the batch within the horizon; a CPU eval lease of `auto` size
// gets at least eval_games_per_thread x caps.threads games. GET/POST /v1/admin/routing; `on` false: the order of before.

import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { type Machine, PROFILE, admin, advance, ask, call, clock, define, enroll, err, finish, lease, makeBuild, makeCampaign, makeRun, ok, publishCkpt, runBody } from "./helpers";

const CPU_ONLY = { ...PROFILE, gpus: [], providers: ["cpu"] };
const GPU_OK = { gpu: false, gpu_allowed: true };
const EVAL = (games = 400) => ({
  batch: "b1",
  games,
  checkpoints: [2, 1],
  games_per_lease: "auto",
  job: { settings: { sims: 32 }, seats: [{ net: 2 }, { net: 1 }], paired: true },
});

interface Setup {
  cpuRt: string;
  cuda: string;
  def: string;
  alt: string;
}

// A self-play run whose default is the CPU pack, with the CUDA alternate (a GPU version); optionally an eval batch
// and rolling self-play. Its definition lists both builds.
async function gpuRun(run: string, o: { eval?: unknown; rolling?: unknown; quota?: number } = {}): Promise<Setup> {
  const cpuRt = await makeBuild({ kind: "runtime", tag: `cpu-${run}` });
  const cuda = await makeBuild({ kind: "runtime", tag: `cuda-${run}`, gpu: { vendor: "nvidia", min_driver: "527.41", providers: ["cuda"] } });
  const def = await makeBuild({ tag: `w-cpu-${run}`, requires_runtime: cpuRt });
  const alt = await makeBuild({ tag: `w-cuda-${run}`, requires_runtime: cuda });
  const body = (extra: Record<string, unknown> = {}) => {
    const b: any = { ...runBody(def, { games_per_lease: "auto", quota: o.quota ?? 100000 }), runtime: cpuRt, alternates: [{ build_id: alt, runtime: cuda, when: { vendor: "nvidia" } }], ...extra };
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
  return { cpuRt, cuda, def, alt };
}

const routing = (body: unknown) => admin("POST", "/v1/admin/routing", body);
// The PC is online (its last contact now), and works on nothing.
const online = async (m: Machine) => ok(await ask(m, { kinds: [] }));
const setRate = (m: Machine, scope: string, rate: number) =>
  env.DB.prepare(`INSERT OR REPLACE INTO rates (machine_id, kind, scope, rate, samples, t) VALUES (?, 'eval', ?, ?, 5, ?)`).bind(m.machine_id, scope, rate, clock.now).run();
const gpuPlay = async (m: Machine) => (await env.DB.prepare(`SELECT gpu_play FROM machines WHERE machine_id = ?`).bind(m.machine_id).first<any>()).gpu_play;

describe("CPU self-play is the last resort while the run has a GPU version (rev 18)", () => {
  it("a CPU-only PC takes fuzz first; an NVIDIA PC takes the self-play on its GPU", async () => {
    const s = await gpuRun("sp");
    await makeCampaign("fz");
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    expect((await lease(cpu)).kind).toBe("fuzz");
    const nv = await enroll("nv");
    const l = await lease(nv, { caps: GPU_OK });
    expect([l.kind, l.build.build_id]).toEqual(["selfplay", s.alt]);
    expect(await gpuPlay(nv)).toBe(1);
    expect(await gpuPlay(cpu)).toBe(0);
  });

  it("with nothing else to do the CPU-only PC still plays the self-play (default build)", async () => {
    const s = await gpuRun("sp");
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    const l = await lease(cpu);
    expect([l.kind, l.build.build_id]).toEqual(["selfplay", s.def]);
  });

  it("a run without a GPU version: its self-play still goes before fuzz everywhere", async () => {
    await makeRun("plain", { quota: 10000 });
    await makeCampaign("fz");
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    expect((await lease(cpu)).kind).toBe("selfplay");
  });

  it("an NVIDIA PC whose bench says no GPU pays for the run (gpu_games 0), or that may not use its GPU, counts as a CPU PC", async () => {
    await gpuRun("sp");
    await makeCampaign("fz");
    const nv = await enroll("nv");
    expect((await lease(nv, { caps: { ...GPU_OK, gpu_games: { sp: 0 } } })).kind).toBe("fuzz");
    const nv2 = await enroll("nv2");
    expect((await lease(nv2, { caps: { gpu: false, gpu_allowed: false } })).kind).toBe("fuzz");
    expect(await gpuPlay(nv)).toBe(0);
  });

  it("routing off: CPU self-play goes before fuzz, as before rev 18", async () => {
    ok(await routing({ on: false }));
    await gpuRun("sp");
    await makeCampaign("fz");
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    expect((await lease(cpu)).kind).toBe("selfplay");
  });
});

describe("eval batches on PCs that play self-play on their GPU (rev 18)", () => {
  it("after the self-play while the CPU PCs online can finish the batch within the horizon; first once they can't", async () => {
    await gpuRun("sp", { eval: EVAL(400) });
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    await online(cpu);
    await setRate(cpu, "run:sp", 1); // 1 game/s x 1800 s >= 400 unissued: not behind
    const nv = await enroll("nv");
    expect((await lease(nv, { caps: GPU_OK })).kind).toBe("selfplay");
    // a slow CPU PC: behind, so the GPU PC helps
    await setRate(cpu, "run:sp", 0.01);
    const nv2 = await enroll("nv2");
    expect((await lease(nv2, { caps: GPU_OK })).kind).toBe("eval");
    // the CPU PC itself takes the batch first (its lease sized from that rate: the floor, 2 x 6 threads)
    const l = await lease(cpu);
    expect([l.kind, l.job.games]).toEqual(["eval", 12]);
  });

  it("a CPU PC counts its average eval rate when it has none for this batch; unknown counts as nothing", async () => {
    await gpuRun("sp", { eval: EVAL(400) });
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    await online(cpu);
    const nv = await enroll("nv");
    expect((await lease(nv, { caps: GPU_OK })).kind).toBe("eval"); // no rate at all: behind
    await setRate(cpu, "run:other-a", 0.5);
    await setRate(cpu, "run:other-b", 1.5); // average 1 game/s
    const nv2 = await enroll("nv2");
    expect((await lease(nv2, { caps: GPU_OK })).kind).toBe("selfplay");
  });

  it("CPU PCs offline, in use (fresh presence), paused or playing on a GPU don't count", async () => {
    await gpuRun("sp", { eval: EVAL(400) });
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    await online(cpu);
    await setRate(cpu, "run:sp", 1);
    const probe = async (name: string) => (await lease(await enroll(name), { caps: GPU_OK })).kind;
    expect(await probe("nv1")).toBe("selfplay");
    ok(await call("POST", "/v1/presence", { token: cpu.token, body: { reason: "user_active" } }));
    expect(await probe("nv2")).toBe("eval");
    await online(cpu);
    advance(31); // its next contact moves last_seen past the presence
    await online(cpu);
    expect(await probe("nv3")).toBe("selfplay");
    await env.DB.prepare(`UPDATE machines SET paused = 1 WHERE machine_id = ?`).bind(cpu.machine_id).run();
    expect(await probe("nv4")).toBe("eval");
    await env.DB.prepare(`UPDATE machines SET paused = 0, gpu_play = 1 WHERE machine_id = ?`).bind(cpu.machine_id).run();
    expect(await probe("nv5")).toBe("eval");
    await env.DB.prepare(`UPDATE machines SET gpu_play = 0 WHERE machine_id = ?`).bind(cpu.machine_id).run();
    expect(await probe("nv6")).toBe("selfplay");
    advance(301);
    expect(await probe("nv7")).toBe("eval"); // offline: not seen for 5 minutes
  });

  it("eval_horizon_s 0 or routing off: eval before self-play everywhere, as before", async () => {
    await gpuRun("sp", { eval: EVAL(400) });
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    await online(cpu);
    await setRate(cpu, "run:sp", 1);
    ok(await routing({ eval_horizon_s: 0 }));
    expect((await lease(await enroll("nv1"), { caps: GPU_OK })).kind).toBe("eval");
    ok(await routing({ eval_horizon_s: 1800, on: false }));
    expect((await lease(await enroll("nv2"), { caps: GPU_OK })).kind).toBe("eval");
  });

  it("a deferred batch still goes before fuzz", async () => {
    await gpuRun("sp", { eval: EVAL(400), quota: 1 });
    await makeCampaign("fz", { priority: 5 });
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    await online(cpu);
    await setRate(cpu, "run:sp", 1);
    const nv = await enroll("nv");
    expect((await lease(nv, { caps: GPU_OK, kinds: ["selfplay"] })).kind).toBe("selfplay"); // fills the quota
    expect((await lease(nv, { caps: GPU_OK })).kind).toBe("eval"); // no self-play left: the deferred batch, not fuzz
  });
});

describe("CPU eval lease floor (rev 18)", () => {
  const evalRun = () =>
    makeRun("ev", {
      selfplay: false,
      eval: { batch: "b1", games: 1000, checkpoints: [1], games_per_lease: "auto", job: { settings: {}, seats: [{ net: 1 }, { bot: "random" }], paired: false } },
    });
  const games = (l: any) => l.job.games as number;

  it("at least 2 games per thread, no more than the agent's RAM holds; 0 turns it off; a set size is kept", async () => {
    await evalRun();
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    expect(games(await lease(cpu, { caps: { threads: 8, ram_mb: 12000 } }))).toBe(16);
    const small = await enroll("small", { profile: CPU_ONLY });
    expect(games(await lease(small, { caps: { threads: 16, ram_mb: 1500 } }))).toBe(20); // (1500 - 600) / 45
    ok(await routing({ eval_games_per_thread: 0 }));
    const off = await enroll("off", { profile: CPU_ONLY });
    expect(games(await lease(off, { caps: { threads: 8, ram_mb: 12000 } }))).toBe(10); // SIZING.eval.def
    ok(await routing({ eval_games_per_thread: 3 }));
    const three = await enroll("three", { profile: CPU_ONLY });
    expect(games(await lease(three, { caps: { threads: 8, ram_mb: 12000 } }))).toBe(24);
  });

  it("a configured games_per_lease is exact", async () => {
    await makeRun("ev", {
      selfplay: false,
      eval: { batch: "b1", games: 1000, checkpoints: [1], games_per_lease: 4, job: { settings: {}, seats: [{ net: 1 }, { bot: "random" }], paired: false } },
    });
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    expect(games(await lease(cpu, { caps: { threads: 8, ram_mb: 12000 } }))).toBe(4);
  });
});

describe("rolling self-play and hardware routing (rev 18)", () => {
  const ROLLING = { inflight: 8, gpu_inflight: 32, block: 1000 };
  const askRolling = (m: Machine, caps: Record<string, unknown> = {}) =>
    call("POST", "/v1/lease", {
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
  const rollingLease = async (m: Machine, caps: Record<string, unknown> = {}) => {
    const b = ok(await askRolling(m, caps));
    expect(b.lease, JSON.stringify(b)).toBeTruthy();
    return b.lease;
  };
  const extend = (m: Machine, id: string, games: number) => call("POST", `/v1/lease/${id}/extend`, { token: m.token, body: { games } });
  const due = (id: string) => env.DB.prepare(`UPDATE leases SET until = ? WHERE lease_id = ?`).bind(clock.now - 1, id).run();

  it("a CPU PC's rolling self-play of a run with a GPU version gives way to fuzz at renewal", async () => {
    await gpuRun("sp", { rolling: ROLLING });
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    const l = await rollingLease(cpu);
    expect(l.kind).toBe("selfplay");
    await makeCampaign("fz");
    await due(l.lease_id);
    expect(ok(await extend(cpu, l.lease_id, 8))).toMatchObject({ finish: true, reason: "max_s" });
    ok(await finish(cpu, l.lease_id, []));
    expect((await rollingLease(cpu)).kind).toBe("fuzz");
  });

  // (an NVIDIA PC whose CUDA probe passed: caps.gpu, which the renewal reads from the stored caps)
  const GPU_ON = { gpu: true, gpu_allowed: true };
  const gpuRolling = async (run: string, quota: number) => {
    await gpuRun(run, { rolling: ROLLING, eval: EVAL(400), quota });
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    await online(cpu);
    await setRate(cpu, `run:${run}`, 1); // the batch is not behind
    const nv = await enroll("nv");
    const l = await rollingLease(nv, GPU_ON);
    expect([l.kind, l.run]).toEqual(["selfplay", run]);
    await due(l.lease_id);
    return ok(await extend(nv, l.lease_id, 8));
  };

  it("a GPU PC's rolling self-play is renewed while a deferred batch waits and the generation has headroom", async () => {
    expect((await gpuRolling("roomy", 100000)).finish).toBeUndefined();
  });

  it("the renewal reads gpu_allowed and gpu_games as the lease request sent them (an agent with gpu false, gpu_allowed true)", async () => {
    await gpuRun("sp", { rolling: ROLLING, eval: EVAL(400) });
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    await online(cpu);
    await setRate(cpu, "run:sp", 1);
    const nv = await enroll("nv");
    const l = await rollingLease(nv, GPU_OK);
    expect(l.kind).toBe("selfplay");
    expect(await gpuPlay(nv)).toBe(1);
    await due(l.lease_id);
    expect(ok(await extend(nv, l.lease_id, 8)).finish).toBeUndefined(); // still its GPU self-play: the batch waits
    expect(await gpuPlay(nv)).toBe(1);
    // its next request says its bench found no GPU that pays for this run: a CPU PC now, so the batch ends the lease
    ok(await ask(nv, { kinds: [], caps: { ...GPU_OK, gpu_games: { sp: 0 } } }));
    expect(await gpuPlay(nv)).toBe(0);
    await due(l.lease_id);
    expect(ok(await extend(nv, l.lease_id, 8))).toMatchObject({ finish: true, reason: "max_s" });
  });

  it("a deferred batch ends it at renewal once the generation is full", async () => {
    expect(await gpuRolling("full", 1)).toMatchObject({ finish: true, reason: "max_s" });
  });
});

describe("GET/POST /v1/admin/routing (rev 18)", () => {
  it("defaults, partial updates, validation", async () => {
    expect(ok(await admin("GET", "/v1/admin/routing"))).toEqual({ on: true, eval_horizon_s: 1800, eval_games_per_thread: 2, stored: false });
    expect(ok(await routing({ eval_horizon_s: 900 }))).toEqual({ ok: true, on: true, eval_horizon_s: 900, eval_games_per_thread: 2 });
    expect(ok(await routing({ on: false }))).toEqual({ ok: true, on: false, eval_horizon_s: 900, eval_games_per_thread: 2 });
    expect(ok(await admin("GET", "/v1/admin/routing"))).toEqual({ on: false, eval_horizon_s: 900, eval_games_per_thread: 2, stored: true });
    for (const bad of [{ on: "yes" }, { eval_horizon_s: -1 }, { eval_horizon_s: 1.5 }, { eval_horizon_s: 8 * 86400 }, { eval_games_per_thread: 17 }, { other: 1 }]) {
      err(await routing(bad), 400, "bad_request");
    }
    err(await call("GET", "/v1/admin/routing"), 401, "unauthorized");
  });
});
