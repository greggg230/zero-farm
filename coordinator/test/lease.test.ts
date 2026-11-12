// The lease lifecycle (issue, beat, expiry -> reissue, give-back, cancel, done) and how work is
// chosen and sized.

import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { runCron } from "../src/cron";
import {
  admin,
  advance,
  beat,
  call,
  clock,
  complete,
  enroll,
  err,
  expectTiled,
  finish,
  idsOf,
  lease,
  ask,
  makeBuild,
  makeCampaign,
  makeRun,
  ok,
  publishCkpt,
  runBody,
  upload,
} from "./helpers";

describe("lifecycle", () => {
  it("issue -> beat -> upload -> done", async () => {
    const build = await makeRun("r", { games_per_lease: 10 });
    const m = await enroll("a");
    const l = await lease(m);
    expect(l).toMatchObject({
      kind: "selfplay",
      run: "r",
      campaign: null,
      beat_s: 60,
      slack_s: 600,
      issued_at: clock.now,
      expires_at: clock.now + 180,
      build: { build_id: build, manifest: `builds/${build}/manifest.json` },
      runtime: null,
      checkpoint: { gen: 1, model: "ckpt/r/1/model.onnx", reference: "ckpt/r/1/reference.npz" },
      job: { kind: "selfplay", settings: { sims: 64 }, games: 10, gid_base: 0 },
    });
    expect(l.lease_id).toMatch(/^l_[0-9a-z]{26}$/);
    expect(Number.isInteger(l.job.seed)).toBe(true);
    advance(60);
    const b = ok(await beat(m, l.lease_id, "running", { stats: { games: 2 } }));
    expect(b).toEqual({ action: "continue", expires_at: clock.now + 180, checkpoint: null });
    ok(await upload(m, l.lease_id, 0, "games 0-4", { ids: [[0, 5]] }));
    ok(await upload(m, l.lease_id, 1, "games 5-9", { ids: [[5, 10]] }));
    expect(ok(await finish(m, l.lease_id, [0, 1]))).toEqual({ ok: true, returned: 0 });
    expect(ok(await finish(m, l.lease_id, [0, 1]))).toEqual({ ok: true, returned: 0 }); // repeat is fine
    err(await beat(m, l.lease_id), 410, "gone");
    await expectTiled("run:r", 0, 10);
    const row = await env.DB.prepare(`SELECT state, units_done FROM leases WHERE lease_id = ?`).bind(l.lease_id).first<any>();
    expect(row).toEqual({ state: "done", units_done: 10 });
  });

  it("a lease is only its machine's", async () => {
    await makeRun("r");
    const a = await enroll("a");
    const b = await enroll("b");
    const l = await lease(a);
    err(await beat(b, l.lease_id), 404, "not_found");
    err(await upload(b, l.lease_id, 0, "x", { ids: [[0, 1]] }), 404, "not_found");
    err(await finish(b, l.lease_id, []), 404, "not_found");
    err(await beat(a, "l_00000000000000000000000000"), 404, "not_found");
    err(await beat(a, "nonsense"), 404, "not_found");
    err(await call("POST", `/v1/lease/${l.lease_id}/beat`, { token: a.token, body: { state: "sleeping" } }), 400, "bad_request");
  });

  it("a lease silent for 3 beats expires; its unfinished game ids go to the next machine", async () => {
    await makeRun("r", { games_per_lease: 10 });
    const a = await enroll("a");
    const b = await enroll("b");
    const la = await lease(a);
    ok(await upload(a, la.lease_id, 0, "games 0,1,2,7", { ids: [[0, 3], [7, 8]] }));
    advance(150);
    ok(await beat(a, la.lease_id)); // extends to now + 180
    advance(179);
    ok(await beat(a, la.lease_id)); // still alive
    advance(181); // silent past 3 beats
    err(await beat(a, la.lease_id), 410, "gone");
    // The six unfinished ids come back, lowest range first, one contiguous range per lease.
    const l1 = await lease(b);
    expect(idsOf(l1)).toEqual([3, 7]);
    const l2 = await lease(b);
    expect(idsOf(l2)).toEqual([8, 10]);
    const l3 = await lease(b);
    expect(idsOf(l3)).toEqual([10, 20]); // then fresh ids
    for (const l of [l1, l2, l3]) await complete(b, l);
    await expectTiled("run:r", 0, 20);
  });

  it("the cron trigger expires silent leases too", async () => {
    await makeCampaign("fz");
    const a = await enroll("a");
    const l = await lease(a);
    expect(await runCron(env, clock.now + 100)).toEqual({ swept: 0, deleted: 0 });
    expect((await runCron(env, clock.now + 181)).swept).toBe(1);
    const row = await env.DB.prepare(`SELECT state, close_reason FROM leases WHERE lease_id = ?`).bind(l.lease_id).first<any>();
    expect(row).toEqual({ state: "expired", close_reason: "silent" });
    expect((await env.DB.prepare(`SELECT lo, hi FROM pool`).first<any>())).toEqual({ lo: l.job.first, hi: l.job.first + l.job.count });
  });

  it("paused past the slack: the coordinator answers give_back; done(given_back) returns the rest", async () => {
    await makeRun("r", { games_per_lease: 10 });
    const a = await enroll("a");
    const l = await lease(a);
    ok(await upload(a, l.lease_id, 0, "4 games", { ids: [[0, 4]] }));
    advance(60);
    expect(ok(await beat(a, l.lease_id, "paused")).action).toBe("continue");
    for (let t = 0; t < 10; t++) {
      advance(60);
      expect(ok(await beat(a, l.lease_id, "paused")).action).toBe("continue");
    }
    advance(60); // paused 660 s > slack 600 s
    expect(ok(await beat(a, l.lease_id, "paused")).action).toBe("give_back");
    expect(ok(await beat(a, l.lease_id, "running")).action).toBe("give_back"); // sticks
    ok(await upload(a, l.lease_id, 1, "1 more", { ids: [[4, 5]] }));
    const d = ok(await finish(a, l.lease_id, [0, 1], { given_back: true, unfinished: { gids: [[5, 10]] } }));
    expect(d.returned).toBe(5);
    const row = await env.DB.prepare(`SELECT state, paused_s FROM leases WHERE lease_id = ?`).bind(l.lease_id).first<any>();
    expect(row.state).toBe("given_back");
    expect(row.paused_s).toBe(660);
    expect(idsOf(await lease(a))).toEqual([5, 10]);
  });

  it("an agent can give back on its own; unfinished outside the lease is refused", async () => {
    await makeCampaign("fz", { seed_base: 100, chunk: 50 });
    const a = await enroll("a");
    const l = await lease(a);
    expect(idsOf(l)).toEqual([100, 150]);
    err(await finish(a, l.lease_id, [], { given_back: true, unfinished: { seeds: [[90, 150]] } }), 400, "bad_request");
    ok(await upload(a, l.lease_id, 0, JSON.stringify({ campaign: "fz", first: 100, count: 50, done: 20, failures: [] }), { ids: [[100, 120]] }));
    expect(ok(await finish(a, l.lease_id, [0], { given_back: true, unfinished: { seeds: [[120, 150]] } })).returned).toBe(30);
    expect(idsOf(await lease(a))).toEqual([120, 150]);
  });

  it("pausing a run cancels its leases at the next beat; their ids go back", async () => {
    const build = await makeRun("r", { games_per_lease: 10 });
    const a = await enroll("a");
    const l = await lease(a);
    ok(await upload(a, l.lease_id, 0, "3 games", { ids: [[0, 3]] }));
    ok(await admin("PUT", "/v1/admin/run/r", runBody(build, { games_per_lease: 10, state: "paused" })));
    expect(ok(await beat(a, l.lease_id))).toEqual({ action: "cancel", expires_at: clock.now, checkpoint: null });
    err(await beat(a, l.lease_id), 410, "gone");
    err(await upload(a, l.lease_id, 1, "late", { ids: [[3, 4]] }), 410, "gone");
    ok(await upload(a, l.lease_id, 0, "3 games", { ids: [[0, 3]] })); // an exact repeat is still fine
    err(await finish(a, l.lease_id, [0]), 410, "gone");
    expect(ok(await ask(a)).reason).toBe("no_work"); // paused run: nothing to do
    ok(await admin("PUT", "/v1/admin/run/r", runBody(build, { games_per_lease: 10 })));
    expect(idsOf(await lease(a))).toEqual([3, 10]);
  });

  it("pausing a campaign cancels its leases", async () => {
    await makeCampaign("fz");
    const a = await enroll("a");
    const l = await lease(a);
    await makeCampaign("fz", { state: "paused" });
    expect(ok(await beat(a, l.lease_id)).action).toBe("cancel");
  });

  it("a newer checkpoint reaches self-play leases through the beat answer", async () => {
    await makeRun("r");
    const a = await enroll("a");
    const l = await lease(a);
    expect(ok(await beat(a, l.lease_id)).checkpoint).toBeNull();
    await publishCkpt("r", 2);
    const b = ok(await beat(a, l.lease_id));
    expect(b.action).toBe("continue");
    expect(b.checkpoint).toMatchObject({ gen: 2, model: "ckpt/r/2/model.onnx", reference: "ckpt/r/2/reference.npz" });
    expect(b.checkpoint.sha256).toMatch(/^[0-9a-f]{64}$/);
    expect(ok(await beat(a, l.lease_id)).checkpoint.gen).toBe(2); // repeated until the lease ends
    // New leases start on gen 2.
    expect((await lease(a)).checkpoint.gen).toBe(2);
  });

  it("done with a result the coordinator never got -> 409 with the missing seqs", async () => {
    await makeRun("r");
    const a = await enroll("a");
    const l = await lease(a);
    ok(await upload(a, l.lease_id, 0, "x", { ids: [[0, 2]] }));
    expect(err(await finish(a, l.lease_id, [0, 1, 2]), 409, "conflict").missing).toEqual([1, 2]);
    ok(await upload(a, l.lease_id, 1, "y", { ids: [[2, 4]] }));
    ok(await upload(a, l.lease_id, 2, "z", { ids: [[4, 10]] }));
    ok(await finish(a, l.lease_id, [0, 1, 2]));
  });

  it("done on an expired lease keeps its stats and returns nothing more", async () => {
    await makeRun("r", { games_per_lease: 10 });
    const a = await enroll("a");
    const l = await lease(a);
    advance(200);
    ok(await upload(a, l.lease_id, 0, "late games", { ids: [[0, 4]] }));
    expect(ok(await finish(a, l.lease_id, [0], { stats: { games: 4, cpu_pct: 300 } }))).toEqual({ ok: true, returned: 0 });
    const row = await env.DB.prepare(`SELECT state, stats FROM leases WHERE lease_id = ?`).bind(l.lease_id).first<any>();
    expect(row.state).toBe("expired");
    expect(JSON.parse(row.stats)).toEqual({ games: 4, cpu_pct: 300 });
  });
});

describe("choosing work", () => {
  it("gives each lease a disjoint game-id block", async () => {
    await makeRun("r", { quota: 1000, games_per_lease: 7 });
    const ms = await Promise.all(["a", "b", "c", "d"].map((n) => enroll(n)));
    const ranges = [];
    for (let i = 0; i < 3; i++) for (const m of ms) ranges.push(idsOf(await lease(m)));
    ranges.sort((x, y) => x[0] - y[0]);
    for (let i = 0; i < ranges.length; i++) expect(ranges[i]).toEqual([i * 7, i * 7 + 7]);
  });

  it("splits work between runs by priority (active leases / priority)", async () => {
    const w = await makeBuild();
    await makeRun("hi", { build: w, priority: 3, quota: 10000 });
    await makeRun("lo", { build: w, priority: 1, quota: 10000 });
    const m = await enroll("a");
    const counts: Record<string, number> = { hi: 0, lo: 0 };
    for (let i = 0; i < 16; i++) {
      counts[(await lease(m)).run]++;
      advance(5); // a machine may ask once per 5 s after its burst
    }
    expect(counts).toEqual({ hi: 12, lo: 4 });
  });

  it("self-play first, then eval, then fuzz as the backfill", async () => {
    await makeCampaign("fz");
    await makeRun("r", { quota: 10, slack: 0, games_per_lease: 10, eval: { batch: "it1", games: 4, games_per_lease: 2, job: { seats: ["a", "b"], paired: true } } });
    const m = await enroll("a");
    expect((await lease(m)).kind).toBe("selfplay");
    const e1 = await lease(m);
    expect(e1).toMatchObject({ kind: "eval", job: { kind: "eval", games: 2, gid_base: 0, seats: ["a", "b"], paired: true } });
    expect((await lease(m)).kind).toBe("eval");
    expect((await lease(m)).kind).toBe("fuzz"); // quota full, eval all issued
  });

  it("routes by GPU: required skips CPU-only PCs, preferred makes them backfill", async () => {
    const w = await makeBuild();
    await makeRun("gpu", { build: w, gpu: "required" });
    const cpu = await enroll("cpu");
    const gpu = await enroll("gpu");
    expect(ok(await ask(cpu)).reason).toBe("no_work");
    expect((await lease(gpu, { caps: { gpu: true } })).run).toBe("gpu");

    ok(await admin("PUT", "/v1/admin/run/gpu", runBody(w, { gpu: "preferred" })));
    await makeCampaign("fz", { priority: 1 });
    expect((await lease(cpu)).kind).toBe("fuzz"); // fuzz (1.0) beats preferred-GPU self-play on a CPU (1.0 x 0.25)
    expect((await lease(gpu, { caps: { gpu: true } })).kind).toBe("selfplay");
  });

  it("respects needs (threads, RAM) and disk", async () => {
    await makeRun("big", { needs: { threads: 8, ram_mb: 16000 } });
    const m = await enroll("a");
    expect(ok(await ask(m, { caps: { threads: 6 } })).reason).toBe("no_work");
    expect(ok(await ask(m, { caps: { threads: 8, ram_mb: 8000 } })).reason).toBe("no_work");
    expect(ok(await ask(m, { caps: { threads: 8, ram_mb: 16000, disk_mb: 0 } })).reason).toBe("no_work"); // no room for the build
    const l = await lease(m, { caps: { threads: 8, ram_mb: 16000, disk_mb: 0 }, cached: { builds: [(await admin("GET", "/v1/admin/run/big")).body.build_id] } });
    expect(l.run).toBe("big");
  });

  it("sizes leases to ~7.5 min of the machine's measured rate (default before history)", async () => {
    await makeRun("r", { quota: 100000, games_per_lease: "auto" });
    const a = await enroll("a");
    const l1 = await lease(a);
    expect(l1.job.games).toBe(16); // no history yet
    advance(160); // 16 games in 160 s = 0.1 games/s
    ok(await beat(a, l1.lease_id));
    await complete(a, l1);
    const l2 = await lease(a);
    expect(l2.job.games).toBe(45); // 0.1 * 450
    // Another machine of the same hardware class starts from the class's rate.
    const b = await enroll("b");
    expect((await lease(b)).job.games).toBe(45);
    // A different class starts from the default.
    const c = await enroll("c", { profile: { cpu: "Intel i5-8400", threads: 6 }, digest: "cd".repeat(32) });
    expect((await lease(c, { digest: "cd".repeat(32) })).job.games).toBe(16);
  });

  it("time spent paused doesn't count against the rate", async () => {
    await makeCampaign("fz", { chunk: "auto" });
    const a = await enroll("a");
    const l = await lease(a);
    expect(l.job.count).toBe(20000);
    advance(60);
    ok(await beat(a, l.lease_id, "paused")); // paused from t=60 (paused agents keep beating)
    advance(120);
    ok(await beat(a, l.lease_id, "paused"));
    advance(120);
    ok(await beat(a, l.lease_id, "paused"));
    advance(60);
    ok(await beat(a, l.lease_id, "running")); // running again at t=360
    advance(40); // 400 s since issue, 300 of them paused -> 100 s running
    ok(await upload(a, l.lease_id, 0, JSON.stringify({ failures: [] }), { ids: [idsOf(l)] }));
    ok(await finish(a, l.lease_id, [0]));
    expect((await lease(a)).job.count).toBe(Math.round((20000 / 100) * 450)); // 90000
  });

  it("time the worker was starved of CPU (Stats starved_ms) doesn't count against the rate", async () => {
    await makeCampaign("fz", { chunk: "auto" });
    const a = await enroll("a");
    const l = await lease(a);
    advance(150);
    ok(await beat(a, l.lease_id, "running", { stats: { starved_ms: 100_000 } }));
    advance(150);
    ok(await beat(a, l.lease_id, "running", { stats: { starved_ms: 200_000 } }));
    advance(100); // 400 s since issue, 300 of them starved (the final Stats count) -> 100 s running
    ok(await upload(a, l.lease_id, 0, JSON.stringify({ failures: [] }), { ids: [idsOf(l)] }));
    ok(await finish(a, l.lease_id, [0], { stats: { starved_ms: 300_000 } }));
    expect((await lease(a)).job.count).toBe(90000);
  });
});
