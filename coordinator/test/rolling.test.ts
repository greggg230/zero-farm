// Rolling self-play leases (PROTOCOL 10, rev 15): a run's `selfplay.rolling` gives agents from ROLLING_AGENT that list
// the feature "rolling" a lease that reserves a block of ids from next_gid and releases them as the worker asks
// (POST /v1/lease/{id}/extend), switches generations in place (beat answers carry the new definition), is never
// refused as stale, and is renewed past `until` unless other work waits for the machine. Every other agent, and every
// run without the setting, gets classic leases exactly as before.

import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import {
  type Machine,
  type RunOpts,
  admin,
  advance,
  beat,
  call,
  checkDefinitionLease,
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
  putArtifact,
  runBody,
  sha256,
  upload,
} from "./helpers";

const ROLL = "0.2.13"; // ROLLING_AGENT's default
const ROLLING = { inflight: 8, gpu_inflight: 64, block: 1000 }; // first release: the larger in-flight count, 64

// A run with rolling self-play: makeRun (gen 1, signed), then the same configuration with `rolling`.
async function rollingRun(run: string, o: RunOpts & { rolling?: Record<string, unknown> } = {}): Promise<string> {
  const build = await makeRun(run, o);
  const body: any = runBody(build, o);
  body.selfplay.rolling = o.rolling ?? ROLLING;
  ok(await admin("PUT", `/v1/admin/run/${run}`, body));
  return build;
}

function askRolling(m: Machine, o: { agent?: string; features?: unknown; env?: Record<string, string>; now?: number } = {}) {
  return call("POST", "/v1/lease", {
    token: m.token,
    agent: o.agent ?? ROLL,
    env: o.env,
    now: o.now,
    body: {
      profile_digest: "ab".repeat(32),
      profile: null,
      caps: { threads: 6, ram_mb: 12000, gpu: false, disk_mb: 2500 },
      cached: { builds: [], ckpts: [] },
      kinds: ["fuzz", "eval", "selfplay", "diag"],
      ...(o.features === undefined ? { features: ["rolling"] } : o.features === null ? {} : { features: o.features }),
    },
  });
}

async function rollingLease(m: Machine, o: Parameters<typeof askRolling>[1] = {}): Promise<any> {
  const b = ok(await askRolling(m, o));
  expect(b.lease, JSON.stringify(b)).toBeTruthy();
  return b.lease;
}

const extend = (m: Machine, id: string, body: unknown, now?: number) => call("POST", `/v1/lease/${id}/extend`, { token: m.token, body, now });
const state = async (run: string) => ok(await admin("GET", `/v1/admin/run/${run}`));
const row = (id: string) => env.DB.prepare(`SELECT * FROM leases WHERE lease_id = ?`).bind(id).first<any>();
const pool = async (scope: string) =>
  (await env.DB.prepare(`SELECT lo, hi FROM pool WHERE scope = ? AND pool = 'selfplay' ORDER BY lo`).bind(scope).all<any>()).results.map((r) => [r.lo, r.hi]);
// `until` has passed (instead of beating for max_s seconds).
const due = (id: string) => env.DB.prepare(`UPDATE leases SET until = ? WHERE lease_id = ?`).bind(clock.now - 1, id).run();
// The worker finished every released id (as its last beat would say), so the in-flight limit (rev 21) leaves room.
const played = (id: string) =>
  env.DB.prepare(`UPDATE leases SET progress = json_object('games_done', rel_hi - lo, 'games_total', rel_hi - lo) WHERE lease_id = ?`).bind(id).run();
// The lease was issued `s` seconds ago (its rate so far, and so its reporting buffer, comes from that).
const aged = (id: string, s: number) => env.DB.prepare(`UPDATE leases SET issued_at = ? WHERE lease_id = ?`).bind(clock.now - s, id).run();
const games = (l: any) => l.job.games;

describe("rolling leases: the run's setting (10.1)", () => {
  it("checks it, fills in the defaults, and GET and the stats show it; a PUT without it removes it", async () => {
    const build = await makeRun("cfg");
    const put = (rolling: unknown) => {
      const body: any = runBody(build);
      if (rolling !== undefined) body.selfplay.rolling = rolling;
      return admin("PUT", "/v1/admin/run/cfg", body);
    };
    for (const bad of [
      { what: 1 },
      { inflight: 0 },
      { inflight: 4097 },
      { inflight: "lots" },
      { gpu_inflight: 1.5 },
      { block: 0 },
      { block: 1_000_001 },
      { max_s: 599 },
      { max_s: 86401 },
      { flush_s: 9 },
      { flush_s: 3601 },
      "yes",
      [1],
    ]) {
      err(await put(bad), 400, "bad_request");
    }
    ok(await put({}));
    const want = { inflight: "auto", gpu_inflight: "auto", block: 65536, max_s: 7200, flush_s: 120 };
    expect((await state("cfg")).selfplay.rolling).toEqual(want);
    expect(ok(await admin("GET", "/v1/admin/stats")).runs.find((r: any) => r.run === "cfg").rolling).toEqual(want);
    ok(await put({ inflight: 12, gpu_inflight: "auto", block: 5000, max_s: 600, flush_s: 10 }));
    expect((await state("cfg")).selfplay.rolling).toEqual({ inflight: 12, gpu_inflight: "auto", block: 5000, max_s: 600, flush_s: 10 });
    ok(await put(undefined));
    expect("rolling" in (await state("cfg")).selfplay).toBe(false);
    ok(await put(null));
    expect("rolling" in (await state("cfg")).selfplay).toBe(false);
    expect(ok(await admin("GET", "/v1/admin/stats")).runs.find((r: any) => r.run === "cfg").rolling).toBe(null);
  });
});

describe("rolling leases: who gets one and the grant (10.2, 10.3)", () => {
  it("an agent from ROLLING_AGENT listing the feature gets a block from next_gid with the first ids released", async () => {
    await rollingRun("roll", { quota: 100, slack: 0.1 }); // cap 110
    const m = await enroll("r1");
    const l = await rollingLease(m);
    await checkDefinitionLease(m, l); // the job is still {kind, gid_base, games, seed}
    expect(l.kind).toBe("selfplay");
    expect(l.rolling).toEqual({ block: [0, 1000], inflight: { cpu: 8, gpu: 64 }, until: clock.now + 7200, max_s: 7200, flush_s: 120 });
    expect(l.job.gid_base).toBe(0);
    expect(games(l)).toBe(64); // min(headroom 110, block 1000, the larger in-flight count 64; rev 21: once, not twice)
    const st = await state("roll");
    expect(st.next_gid).toBe(1000);
    expect(st.selfplay_state.leased).toBe(64); // only released ids count
    expect(st.selfplay_state.headroom).toBe(110 - 64);
    expect(st.rolling_leases).toEqual([
      { lease_id: l.lease_id, machine_id: m.machine_id, name: "r1", block: [0, 1000], released: 64, claimed: 0, until: clock.now + 7200, gen: 1, cur_gen: 1, issued_at: clock.now },
    ]);
    const r = await row(l.lease_id);
    expect([r.lo, r.hi, r.rel_hi, r.until, r.cur_gen]).toEqual([0, 1000, 64, clock.now + 7200, null]);
    // the admin page has a table for them
    const page = await call("GET", "/admin");
    expect(page.body).toContain("Rolling leases");
  });

  it("the first release is capped by the headroom; 'auto' counts 256 games in flight (released once, rev 21)", async () => {
    await rollingRun("auto", { quota: 1000, slack: 0, rolling: { inflight: "auto", gpu_inflight: 4 } });
    expect(games(await rollingLease(await enroll("a1")))).toBe(256);
    await rollingRun("small", { quota: 20, slack: 0, priority: 2 });
    const l = await rollingLease(await enroll("a2"));
    expect(l.run).toBe("small");
    expect(games(l)).toBe(20);
    expect(l.rolling.block).toEqual([0, 1000]);
  });

  it("older agents, agents without the feature and runs without the setting get classic leases, as before", async () => {
    await rollingRun("mixed", { quota: 1000 });
    const classic = (l: any) => {
      expect(l.rolling).toBeUndefined();
      expect(games(l)).toBe(10); // games_per_lease
    };
    classic(await rollingLease(await enroll("old"), { agent: "0.2.12" }));
    classic(await rollingLease(await enroll("nofeat"), { features: null }));
    classic(await rollingLease(await enroll("other"), { features: ["something-else"] }));
    classic(await lease(await enroll("plain"))); // helpers' agent 0.3.1, no features field
    classic(await rollingLease(await enroll("var"), { env: { ROLLING_AGENT: "0.4.0" } })); // a Worker var
    await makeRun("classic", { quota: 1000, priority: 5 });
    const l = await rollingLease(await enroll("c1"));
    expect(l.run).toBe("classic");
    classic(l);
    // a bad features list is refused like any bad field
    err(await askRolling(await enroll("bad"), { features: "rolling" }), 400, "bad_request");
  });

  it("a block comes from next_gid, never from the pool; classic leases still take the pool first", async () => {
    await rollingRun("blk", { quota: 1000 });
    const old = await enroll("old");
    const c = await rollingLease(old, { agent: "0.2.12" }); // classic [0, 10)
    ok(await finish(old, c.lease_id, [], { given_back: true }));
    expect(await pool("run:blk")).toEqual([[0, 10]]);
    const l = await rollingLease(await enroll("new"));
    expect(l.rolling.block).toEqual([10, 1010]);
    expect(l.job.gid_base).toBe(10);
    const c2 = await rollingLease(await enroll("old2"), { agent: "0.2.12" });
    expect(c2.job.gid_base).toBe(0); // the pool
    expect((await state("blk")).next_gid).toBe(1010);
  });
});

describe("rolling leases: extend (10.4)", () => {
  it("releases ids up to the headroom, then answers wait_s until the trainer's counts make room", async () => {
    await rollingRun("q", { quota: 100, slack: 0.1 }); // cap 110
    const m = await enroll("m");
    const l = await rollingLease(m); // 64 released
    const cm = await enroll("classic");
    const c = await lease(cm, { agent: "0.2.12" }); // 10 classic ids [1000, 1010)
    expect(c.job.gid_base).toBe(1000);
    // 20 games done on the rolling lease
    ok(await upload(m, l.lease_id, 0, "chunk0", { ids: [[0, 20]], gen: 1 }));
    let st = (await state("q")).selfplay_state;
    expect([st.leased, st.pending, st.headroom]).toEqual([44 + 10, 20, 110 - 54 - 20]);
    // asked 100, got the headroom (36)
    expect(ok(await extend(m, l.lease_id, { games: 100 }))).toEqual({ games: 100 });
    expect((await row(l.lease_id)).rel_hi).toBe(100);
    st = (await state("q")).selfplay_state;
    expect(st.headroom).toBe(0);
    expect(ok(await extend(m, l.lease_id, { games: 50 }))).toEqual({ games: 100, wait_s: 60, reason: "quota" });
    // accepting and acking the 20 games changes nothing (accepted + in flight); the classic lease given back does
    ok(await admin("POST", "/v1/admin/run/q/accept", { gen: 1, games: 20 }));
    const { cursor } = ok(await admin("GET", "/v1/admin/run/q/results"));
    ok(await admin("POST", "/v1/admin/run/q/ack", { cursor }));
    expect(ok(await extend(m, l.lease_id, { games: 50 }))).toEqual({ games: 100, wait_s: 60, reason: "quota" });
    ok(await finish(cm, c.lease_id, [], { given_back: true }));
    // headroom again, but 80 unfinished ids are past this lease's limit (25 in flight + 40 of buffer): rev 21
    expect(ok(await extend(m, l.lease_id, { games: 50 }))).toEqual({ games: 100, wait_s: 20, reason: "inflight" });
    await played(l.lease_id);
    expect(ok(await extend(m, l.lease_id, { games: 50 }))).toEqual({ games: 110 });
    // x-ids may now reach the newly released ids
    ok(await upload(m, l.lease_id, 1, "chunk1", { ids: [[100, 110]], gen: 1 }));
    err(await upload(m, l.lease_id, 2, "chunk2", { ids: [[110, 111]], gen: 1 }), 400, "bad_request");
  });

  it("finishes when the block is used up", async () => {
    await rollingRun("b", { quota: 10000, rolling: { inflight: 8, gpu_inflight: 64, block: 70 } });
    const m = await enroll("m");
    const l = await rollingLease(m);
    expect(games(l)).toBe(64);
    await played(l.lease_id);
    expect(ok(await extend(m, l.lease_id, { games: 100 }))).toEqual({ games: 70 });
    expect(ok(await extend(m, l.lease_id, { games: 100 }))).toEqual({ games: 70, finish: true, reason: "block" });
  });

  it("renews past `until` when nothing else waits for the machine", async () => {
    await rollingRun("renew", { quota: 10000 });
    await makeRun("lower", { quota: 10000, priority: 0.5 }); // lower priority: doesn't count
    await makeCampaign("fz"); // the backfill: doesn't count
    const m = await enroll("m");
    const l = await rollingLease(m);
    expect(l.run).toBe("renew");
    await due(l.lease_id);
    await played(l.lease_id);
    expect(ok(await extend(m, l.lease_id, { games: 16 }))).toEqual({ games: 80 });
    expect((await row(l.lease_id)).until).toBe(clock.now - 1 + 7200);
    // before `until` nothing is looked at
    expect(ok(await extend(m, l.lease_id, { games: 16 }))).toEqual({ games: 96 });
  });

  it("finishes past `until` when an eval batch waits", async () => {
    await rollingRun("sp", { quota: 10000 });
    const m = await enroll("m");
    const l = await rollingLease(m);
    await makeRun("ev", {
      selfplay: false,
      priority: 0.1,
      eval: { batch: "b1", games: 10, checkpoints: [1], games_per_lease: 4, job: { settings: {}, seats: [{ net: 1 }, { bot: "random" }], paired: false } },
    });
    // before `until` the lease keeps going whatever waits
    await played(l.lease_id);
    expect(ok(await extend(m, l.lease_id, { games: 8 }))).toEqual({ games: 72 });
    await due(l.lease_id);
    expect(ok(await extend(m, l.lease_id, { games: 8 }))).toEqual({ games: 72, finish: true, reason: "max_s" });
    // the machine's next lease is that work
    ok(await finish(m, l.lease_id, []));
    expect((await rollingLease(m)).kind).toBe("eval");
  });

  it("finishes past `until` when a fuzz campaign with a share is below it", async () => {
    await rollingRun("sp", { quota: 10000 });
    const m = await enroll("m");
    const l = await rollingLease(m);
    await makeCampaign("fz", { share: 0.5 }); // 0 + 1 <= 0.5 x (1 run lease + 0 + 1)
    await due(l.lease_id);
    expect(ok(await extend(m, l.lease_id, { games: 8 }))).toEqual({ games: 64, finish: true, reason: "max_s" });
  });

  it("a fuzz share that is not behind doesn't count", async () => {
    await rollingRun("sp", { quota: 10000 });
    const m = await enroll("m");
    const l = await rollingLease(m);
    await makeCampaign("fz", { share: 0.1 }); // 1 > 0.1 x 2
    await due(l.lease_id);
    await played(l.lease_id);
    expect(ok(await extend(m, l.lease_id, { games: 8 }))).toEqual({ games: 72 });
  });

  it("finishes past `until` when another run with a higher priority has headroom (not without headroom)", async () => {
    await rollingRun("sp", { quota: 10000 });
    const m = await enroll("m");
    const l = await rollingLease(m);
    await makeRun("full", { quota: 0, priority: 5 }); // no headroom: doesn't count
    await due(l.lease_id);
    await played(l.lease_id);
    expect(ok(await extend(m, l.lease_id, { games: 8 }))).toEqual({ games: 72 });
    await makeRun("hi", { quota: 100, priority: 5 });
    await due(l.lease_id);
    expect(ok(await extend(m, l.lease_id, { games: 8 }))).toEqual({ games: 72, finish: true, reason: "max_s" });
  });

  it("finishes with state when the run stops rolling, is paused, or the machine or farm is paused", async () => {
    const build = await rollingRun("st", { quota: 10000 });
    const m = await enroll("m");
    const l = await rollingLease(m);
    const fin = { games: 64, finish: true, reason: "state" };
    ok(await admin("POST", `/v1/admin/machine/${m.machine_id}`, { paused: true }));
    expect(ok(await extend(m, l.lease_id, { games: 8 }))).toEqual(fin);
    ok(await admin("POST", `/v1/admin/machine/${m.machine_id}`, { paused: false }));
    ok(await admin("POST", "/v1/admin/pause", { paused: true }));
    expect(ok(await extend(m, l.lease_id, { games: 8 }))).toEqual(fin);
    ok(await admin("POST", "/v1/admin/pause", { paused: false }));
    await played(l.lease_id);
    expect(ok(await extend(m, l.lease_id, { games: 8 }))).toEqual({ games: 72 });
    ok(await admin("PUT", "/v1/admin/run/st", runBody(build, { quota: 10000 }))); // rolling removed
    expect(ok(await extend(m, l.lease_id, { games: 8 }))).toEqual({ ...fin, games: 72 });
    const body: any = runBody(build, { quota: 10000, state: "paused" });
    body.selfplay.rolling = ROLLING;
    ok(await admin("PUT", "/v1/admin/run/st", body)); // paused: the lease is cancelled at its next beat
    expect(ok(await extend(m, l.lease_id, { games: 8 }))).toEqual({ ...fin, games: 72 });
    expect(ok(await beat(m, l.lease_id)).action).toBe("cancel");
  });

  it("finishes with state once the run's current definition no longer lists the lease's build (the run was re-pinned)", async () => {
    await rollingRun("rp", { quota: 10000 });
    const m = await enroll("m");
    const l = await rollingLease(m);
    await played(l.lease_id);
    expect(ok(await extend(m, l.lease_id, { games: 8 }))).toEqual({ games: 72 });
    const nb = await makeBuild({ tag: "rp-repinned" });
    const body: any = runBody(nb, { quota: 10000 });
    body.selfplay.rolling = ROLLING;
    ok(await admin("PUT", "/v1/admin/run/rp", body));
    await define("run", "rp"); // the next version lists only the new build
    expect(ok(await extend(m, l.lease_id, { games: 8 }))).toEqual({ games: 72, finish: true, reason: "state" });
    // a fresh lease carries the new build
    const m2 = await enroll("m2");
    expect((await rollingLease(m2)).build.build_id).toBe(nb);
  });

  it("400 for a classic lease or bad games, 404 for another machine's lease, 410 for a closed one; the beat bucket", async () => {
    await rollingRun("e", { quota: 10000 });
    const m = await enroll("m");
    const l = await rollingLease(m);
    const c = await rollingLease(m, { features: null });
    expect(c.rolling).toBeUndefined();
    err(await extend(m, c.lease_id, { games: 8 }), 400, "bad_request");
    for (const g of [0, -1, 1.5, "8", null]) err(await extend(m, l.lease_id, { games: g }), 400, "bad_request");
    err(await extend(m, l.lease_id, {}), 400, "bad_request");
    const other = await enroll("other");
    err(await extend(other, l.lease_id, { games: 8 }), 404, "not_found");
    err(await call("POST", `/v1/lease/l_nope/extend`, { token: m.token, body: { games: 1 } }), 404, "not_found");
    ok(await finish(m, l.lease_id, []));
    err(await extend(m, l.lease_id, { games: 8 }), 410, "gone");
    // rate limit: the beat bucket (burst 12), shared with beats
    const n = await enroll("n");
    const l2 = await rollingLease(n);
    let limited = 0;
    for (let i = 0; i < 14; i++) if ((await extend(n, l2.lease_id, { games: 1 })).status === 429) limited++;
    expect(limited).toBeGreaterThan(0);
  });
});

describe("rolling leases: the in-flight limit (10.4, rev 21)", () => {
  it("a single worker holds its games in flight plus the buffer; finished games (results or its last beat) make room", async () => {
    await rollingRun("lim", { quota: 10000 });
    const m = await enroll("m");
    const l = await rollingLease(m); // 64 released
    await aged(l.lease_id, 3600); // a slow rate so far: the buffer is its floor
    ok(await beat(m, l.lease_id, "running", { stats: { concurrency: 64 }, progress: { games_done: 0, games_total: 64 } }));
    // it asks for 2 x 64: 64 in flight + a buffer of max(16, 64 / 8) = 80 unfinished at most
    expect(ok(await extend(m, l.lease_id, { games: 128 }))).toEqual({ games: 80 });
    expect(ok(await extend(m, l.lease_id, { games: 128 }))).toEqual({ games: 80, wait_s: 20, reason: "inflight" });
    // 30 games credited by a result: 50 unfinished, room for 30
    ok(await upload(m, l.lease_id, 0, "rows", { ids: [[0, 30]], gen: 1 }));
    expect(ok(await extend(m, l.lease_id, { games: 128 }))).toEqual({ games: 110 });
    // the beat's own count, higher than the results': 50 finished, room for 20
    ok(await beat(m, l.lease_id, "running", { stats: { concurrency: 64 }, progress: { games_done: 50, games_total: 110 } }));
    expect(ok(await extend(m, l.lease_id, { games: 128 }))).toEqual({ games: 130 });
  });

  it("the buffer is the games it plays in ROLLING_BUFFER_S (120 s) at its rate so far", async () => {
    await rollingRun("buf", { quota: 10000 });
    const m = await enroll("m");
    const l = await rollingLease(m); // 64 released
    await aged(l.lease_id, 120);
    await played(l.lease_id); // 64 games in 120 s: 64 more in the next 120 s
    // 64 in flight (it asks for 128) + a buffer of 64
    expect(ok(await extend(m, l.lease_id, { games: 128 }))).toEqual({ games: 192 });
    expect(ok(await extend(m, l.lease_id, { games: 128 }))).toEqual({ games: 192, wait_s: 20, reason: "inflight" });
  });

  it("the machine's measured rate for the run counts when it is higher (a lease's first minutes finish little)", async () => {
    await rollingRun("known", { quota: 10000 });
    const m = await enroll("m");
    const l = await rollingLease(m); // 64 released, nothing finished yet
    await env.DB.prepare(`INSERT INTO rates (machine_id, kind, scope, rate, samples, t) VALUES (?, 'selfplay', 'run:known', 1.0, 3, ?)`)
      .bind(m.machine_id, clock.now)
      .run();
    // 64 in flight + a buffer of 120 games (1 a second for 120 s): 184 unfinished at most
    expect(ok(await extend(m, l.lease_id, { games: 128 }))).toEqual({ games: 184 });
  });

  it("a crew holds twice its games in flight: more workers in its last beat, or more games asked for than its main worker plays", async () => {
    await rollingRun("crew", { quota: 10000 });
    const m = await enroll("m");
    const l = await rollingLease(m); // 64 released
    await aged(l.lease_id, 3600);
    ok(await beat(m, l.lease_id, "running", { stats: { concurrency: 64 }, progress: { games_done: 0, games_total: 64 } }));
    // 2 workers of 64: it asks for 2 x 128, holds at most 2 x 128 + 16
    expect(ok(await extend(m, l.lease_id, { games: 256 }))).toEqual({ games: 272 });
    const m2 = await enroll("m2");
    const l2 = await rollingLease(m2); // 64 released
    await aged(l2.lease_id, 3600);
    ok(await beat(m2, l2.lease_id, "running", { stats: { concurrency: 64, inflight: 64, workers: 2 }, progress: { games_done: 0, games_total: 64 } }));
    // its beat says 2 workers (rev 19 agents): 2 x 64 + 16
    expect(ok(await extend(m2, l2.lease_id, { games: 128 }))).toEqual({ games: 144 });
    // the crew's numbers are kept with the lease's stats
    const s = JSON.parse((await row(l2.lease_id)).stats);
    expect([s.inflight, s.workers]).toEqual([64, 2]);
  });

  it("never under the games its main worker plays: a small ask still gets ids", async () => {
    await rollingRun("big", { quota: 10000 });
    const m = await enroll("m");
    const l = await rollingLease(m); // 64 released
    await aged(l.lease_id, 3600);
    ok(await beat(m, l.lease_id, "running", { stats: { concurrency: 200 }, progress: { games_done: 0, games_total: 64 } }));
    // half of 16 is 8, but the worker plays 200: at most 200 + 25 unfinished
    expect(ok(await extend(m, l.lease_id, { games: 16 }))).toEqual({ games: 80 });
  });
});

describe("rolling leases: beats switch generations in place (10.6)", () => {
  it("offers the current gen with the current definition once it signs it; classic beats are unchanged", async () => {
    await rollingRun("g", { quota: 10000 });
    const m = await enroll("m");
    const l = await rollingLease(m);
    const c = await rollingLease(m, { features: null });
    expect(ok(await beat(m, l.lease_id, "running", { gen: 1 }))).toEqual({ action: "continue", expires_at: clock.now + 180, checkpoint: null });
    // gen 2 registered, but no definition signs it yet: nothing to switch to
    const model = "onnx g 2";
    const ref = "reference g 2";
    await putArtifact("ckpt/g/2/model.onnx", model);
    await putArtifact("ckpt/g/2/reference.npz", ref);
    ok(
      await admin("PUT", "/v1/admin/run/g/checkpoint/2", {
        model: "ckpt/g/2/model.onnx",
        sha256: await sha256(model),
        reference: "ckpt/g/2/reference.npz",
        reference_sha256: await sha256(ref),
      }),
    );
    expect(ok(await beat(m, l.lease_id, "running", { gen: 1 })).checkpoint).toBe(null);
    const d = await define("run", "g"); // a new version signs gen 2
    const ans = ok(await beat(m, l.lease_id, "running", { gen: 1 }));
    expect(ans.checkpoint.gen).toBe(2);
    expect(ans.checkpoint.sha256).toBe(await sha256(model));
    expect(ans.definition).toEqual({ key: d.key, sha256: d.sha256 });
    expect(ans.definition.key).not.toBe(l.definition.key);
    // repeated until the agent reports the new gen
    expect(ok(await beat(m, l.lease_id)).checkpoint.gen).toBe(2);
    const after = ok(await beat(m, l.lease_id, "running", { gen: 2 }));
    expect(after).toEqual({ action: "continue", expires_at: clock.now + 180, checkpoint: null });
    expect((await row(l.lease_id)).cur_gen).toBe(2);
    expect((await state("g")).rolling_leases[0].cur_gen).toBe(2);
    expect(ok(await beat(m, l.lease_id)).checkpoint).toBe(null); // cur_gen is kept
    // a classic lease's beat: the keys of before, no definition, and (definition v1 doesn't sign gen 2) no switch
    const cb = ok(await beat(m, c.lease_id, "running", { gen: 1 }));
    expect(Object.keys(cb).sort()).toEqual(["action", "checkpoint", "expires_at"]);
    expect(cb.checkpoint).toBe(null);
    expect((await row(c.lease_id)).cur_gen).toBe(null);
    err(await beat(m, l.lease_id, "running", { gen: "2" }), 400, "bad_request");
  });
});

describe("rolling leases: results and closing (10.7)", () => {
  it("rolling chunks are never stale; classic chunks still are; x-gen and x-ids are checked", async () => {
    await rollingRun("s", { quota: 10000, max_stale: 1 });
    const m = await enroll("m");
    const l = await rollingLease(m);
    const c = await rollingLease(m, { features: null });
    await publishCkpt("s", 2);
    await publishCkpt("s", 3); // current 3: gen 1 is past max_stale
    ok(await upload(m, l.lease_id, 0, "old rows", { ids: [[0, 4]], gen: 1 }));
    err(await upload(m, c.lease_id, 0, "old rows", { ids: [[c.job.gid_base, c.job.gid_base + 4]], gen: 1 }), 410, "gone");
    err(await upload(m, l.lease_id, 1, "future", { ids: [[4, 5]], gen: 4 }), 400, "bad_request");
    err(await upload(m, l.lease_id, 1, "outside", { ids: [[60, 70]], gen: 3 }), 400, "bad_request"); // past the released 64
    err(await upload(m, l.lease_id, 1, "outside", { ids: [[1000, 1001]], gen: 3 }), 400, "bad_request");
    ok(await upload(m, l.lease_id, 1, "inside", { ids: [[60, 64]], gen: 3 }));
    // done's unfinished must lie in the released ids
    err(await finish(m, l.lease_id, [0, 1], { unfinished: { gids: [[64, 70]] } }), 400, "bad_request");
    ok(await finish(m, l.lease_id, [0, 1], { unfinished: { gids: [[4, 60]] } }));
  });

  it("done: released unclaimed ids go to the pool, the unreleased tail rewinds next_gid", async () => {
    await rollingRun("d", { quota: 10000 });
    const m = await enroll("m");
    const l = await rollingLease(m);
    ok(await upload(m, l.lease_id, 0, "rows", { ids: [[0, 20]], gen: 1 }));
    expect(ok(await finish(m, l.lease_id, [0]))).toEqual({ ok: true, returned: 44 });
    expect(await pool("run:d")).toEqual([[20, 64]]);
    const st = await state("d");
    expect(st.next_gid).toBe(64);
    expect(st.selfplay_state.leased).toBe(0);
    expect(st.rolling_leases).toEqual([]);
    // the next block starts where the tail went back
    expect((await rollingLease(m)).rolling.block).toEqual([64, 1064]);
  });

  it("closing with ids taken after the block sends the tail to the pool", async () => {
    await rollingRun("t", { quota: 10000 });
    const m = await enroll("m");
    const a = await rollingLease(m); // [0, 1000), 64 released
    const b = await rollingLease(await enroll("n")); // [1000, 2000)
    expect(b.rolling.block).toEqual([1000, 2000]);
    ok(await upload(m, a.lease_id, 0, "rows", { ids: [[0, 10]], gen: 1 }));
    expect(ok(await finish(m, a.lease_id, [0], { given_back: true }))).toEqual({ ok: true, returned: 54 });
    expect(await pool("run:t")).toEqual([[10, 1000]]);
    expect((await state("t")).next_gid).toBe(2000);
    // a classic lease takes the pool
    expect((await lease(await enroll("o"), { agent: "0.2.12" })).job.gid_base).toBe(10);
  });

  it("expiry returns the ids the same way; a late chunk still claims its released ids", async () => {
    await rollingRun("x", { quota: 10000 });
    const m = await enroll("m");
    const l = await rollingLease(m);
    advance(181);
    err(await beat(m, l.lease_id), 410, "gone"); // swept: expired
    expect((await row(l.lease_id)).state).toBe("expired");
    expect(await pool("run:x")).toEqual([[0, 64]]);
    expect((await state("x")).next_gid).toBe(64);
    // a late chunk of the expired lease claims its ids back from the pool
    ok(await upload(m, l.lease_id, 0, "late", { ids: [[0, 8]], gen: 1 }));
    expect(await pool("run:x")).toEqual([[8, 64]]);
    err(await extend(m, l.lease_id, { games: 8 }), 410, "gone");
  });

  it("cancel (revoke) returns the ids, and a new eval batch after the block keeps the tail in the pool", async () => {
    const build = await rollingRun("v", { quota: 10000 });
    const m = await enroll("m");
    const l = await rollingLease(m);
    const body: any = runBody(build, {
      quota: 10000,
      eval: { batch: "b1", games: 10, checkpoints: [1], games_per_lease: 4, job: { settings: {}, seats: [{ net: 1 }, { bot: "random" }], paired: false } },
    });
    body.selfplay.rolling = ROLLING;
    ok(await admin("PUT", "/v1/admin/run/v", body)); // the batch reserves [1000, 1010)
    expect((await state("v")).next_gid).toBe(1010);
    ok(await admin("POST", `/v1/admin/machine/${m.machine_id}/revoke`));
    expect((await row(l.lease_id)).state).toBe("cancelled");
    expect(await pool("run:v")).toEqual([[0, 1000]]);
    expect((await state("v")).next_gid).toBe(1010);
  });
});
