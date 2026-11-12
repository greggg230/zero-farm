// Eval batches and head-to-heads (PROTOCOL 5.2, rev 5): leases that carry every network the seats
// name, the batch checks, whole paired leases, pair crediting within one lease (held games and
// `also`), dropouts, alignment, max_stale, agent gating, disk, progress.

import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { runCron } from "../src/cron";
import { normalize, total, type Range } from "../src/ranges";
import {
  admin,
  advance,
  ask,
  beat,
  checkDefinitionLease,
  clock,
  define,
  enroll,
  err,
  expectTiled,
  finish,
  lease,
  makeBuild,
  makeRun,
  ok,
  publishCkpt,
  runBody,
  upload,
  type Machine,
} from "./helpers";

const SEATS = [{ net: 80 }, { net: 70 }];

function evalCfg(o: Record<string, unknown> = {}) {
  return { batch: "b1", games: 10, checkpoints: [80, 70], games_per_lease: 4, job: { settings: { sims: 32 }, seats: SEATS, paired: true }, ...o };
}

// An ad hoc head-to-head: a run of its own, no self-play, its gens just labels (registered in any
// order), then the batch, then (rev 6) the signed definition with both networks and the seats.
async function h2h(run = "h2h-a-80v70", ev: Record<string, unknown> = evalCfg(), o: { define?: boolean } = {}) {
  const build = await makeBuild({ tag: run });
  const body = (e: unknown) => ({ ...runBody(build, { selfplay: false }), eval: e });
  ok(await admin("PUT", `/v1/admin/run/${run}`, body(null)));
  await publishCkpt(run, 80);
  await publishCkpt(run, 70); // older than 80: registered, not current
  ok(await admin("PUT", `/v1/admin/run/${run}`, body(ev)));
  if (o.define !== false) await define("run", run);
  return { build, put: (e: unknown) => admin("PUT", `/v1/admin/run/${run}`, body(e)) };
}

// One eval row per game, as a worker writes them.
const rows = (gids: number[]) => gids.map((g) => JSON.stringify({ gid: g, pair: Math.floor(g / 2), result: [1, 0] })).join("\n");
const gidsIn = (rs: Range[]) => rs.flatMap(([a, b]) => Array.from({ length: b - a }, (_, i) => a + i));
const toRanges = (ids: number[]): Range[] => normalize(ids.map((i) => [i, i + 1] as Range));

async function put(m: Machine, l: any, seq: number, gids: number[]) {
  return ok(await upload(m, l.lease_id, seq, rows(gids), { ids: toRanges(gids) }));
}

// Reads a batch the way PROTOCOL 5.2 says: rows of `key` for gids in `ids`, plus rows of each
// `also` key for its ids. Returns gid -> row; fails on any gid read twice or missing.
async function readBatch(run: string, batch: string): Promise<Map<number, any>> {
  const out = new Map<number, any>();
  const fetchRows = async (key: string) =>
    new Map<number, any>(
      String((await admin("GET", `/v1/admin/artifact/${key}`)).body)
        .split("\n")
        .filter(Boolean)
        .map((l) => JSON.parse(l))
        .map((r) => [r.gid, r]),
    );
  const take = async (key: string, ids: Range[]) => {
    const have = await fetchRows(key);
    for (const g of gidsIn(ids)) {
      expect(out.has(g), `gid ${g} read twice`).toBe(false);
      expect(have.has(g), `gid ${g} not in ${key}`).toBe(true);
      out.set(g, have.get(g));
    }
  };
  for (const r of ok(await admin("GET", `/v1/admin/run/${run}/results?limit=1000`)).results) {
    if (r.kind !== "eval" || r.batch !== batch) continue;
    expect(r.games).toBe(total(r.ids) + r.also.reduce((n: number, a: any) => n + total(a.ids), 0));
    await take(r.key, r.ids);
    for (const a of r.also) await take(a.key, a.ids);
  }
  return out;
}

describe("eval batches (rev 5)", () => {
  it("a head-to-head run without self-play: leases carry both networks and whole pairs", async () => {
    await h2h();
    const run = ok(await admin("GET", "/v1/admin/run/h2h-a-80v70"));
    expect(run.current_gen).toBe(80);
    expect(run.checkpoints).toEqual([70, 80]);
    expect(run.selfplay_state).toBeNull();
    expect(run.eval_state).toMatchObject({ batch: "b1", games: 10, paired: true, checkpoints: [80, 70], lo: 0, hi: 10, done: 0, leased: 0, unissued: 10 });

    const m = await enroll("a");
    const l = await lease(m);
    // Rev 6: the seats, pairing and settings come from the signed definition; the job only says
    // which games.
    expect(l).toMatchObject({ kind: "eval", run: "h2h-a-80v70", checkpoint: null, job: { kind: "eval", games: 4, gid_base: 0 } });
    expect(typeof l.job.seed).toBe("number");
    const d = await checkDefinitionLease(m, l);
    expect(d.jobs.eval).toEqual({ settings: { sims: 32 }, seats: SEATS, paired: true });
    expect(l.checkpoints.map((c: any) => c.gen)).toEqual([80, 70]); // the batch's order
    for (const c of l.checkpoints) {
      expect(c).toEqual({
        gen: c.gen,
        model: `ckpt/h2h-a-80v70/${c.gen}/model.onnx`,
        sha256: expect.stringMatching(/^[0-9a-f]{64}$/),
        reference: `ckpt/h2h-a-80v70/${c.gen}/reference.npz`,
        reference_sha256: expect.stringMatching(/^[0-9a-f]{64}$/),
      });
    }
    expect(ok(await admin("GET", "/v1/admin/run/h2h-a-80v70")).eval_state).toMatchObject({ leased: 4, unissued: 6 });
    // The remaining 6 games: 4 then 2, every lease whole pairs.
    const l2 = await lease(m);
    const l3 = await lease(m);
    expect([l2.job.gid_base, l2.job.games, l3.job.gid_base, l3.job.games]).toEqual([4, 4, 8, 2]);
    expect(ok(await ask(m)).reason).toBe("no_work");
  });

  it("leases an even number of games even when games_per_lease is odd", async () => {
    await h2h("h2h-odd", evalCfg({ games_per_lease: 3 }));
    const m = await enroll("a");
    expect((await lease(m)).job).toMatchObject({ gid_base: 0, games: 2 });
    expect((await lease(m)).job).toMatchObject({ gid_base: 2, games: 2 });
  });

  it("checks batches: registered gens, seats = checkpoints, no ext: bots, even paired games, no gen", async () => {
    const { put } = await h2h();
    const job = (o: Record<string, unknown>) => ({ settings: {}, seats: SEATS, paired: true, ...o });
    const bad = async (e: unknown, what: RegExp) => expect(err(await put(e), 400, "bad_request").message).toMatch(what);
    await bad(evalCfg({ batch: "x1", gen: 80 }), /replaced by eval.checkpoints/);
    await bad({ ...evalCfg({ batch: "x1" }), checkpoints: undefined }, /checkpoints must be a list/);
    await bad(evalCfg({ batch: "x1", checkpoints: [80, 80] }), /twice/);
    await bad(evalCfg({ batch: "x1", checkpoints: [80], job: job({}) }), /exactly the gens the seats name/);
    await bad(evalCfg({ batch: "x1", checkpoints: [80, 70, 60], job: job({}) }), /exactly the gens the seats name/);
    await bad(evalCfg({ batch: "x1", checkpoints: [80, 60], job: job({ seats: [{ net: 80 }, { net: 60 }] }) }), /60 not registered/);
    await bad(evalCfg({ batch: "x1", checkpoints: [80], job: job({ seats: [{ net: 80 }, { bot: "ext:node-bot" }] }) }), /ext: bots/);
    await bad(evalCfg({ batch: "x1", checkpoints: [80], job: job({ seats: [{ net: 80, bot: "heuristic" }, { bot: "random" }] }) }), /exactly one of net and bot/);
    await bad(evalCfg({ batch: "x1", checkpoints: [80], job: job({ seats: [{ net: 80 }] }) }), /2 to 16 seats/);
    await bad(evalCfg({ batch: "x1", games: 9 }), /even/);
    await bad(evalCfg({ batch: "x1", job: job({ paired: "yes" }) }), /paired/);
    // A new run can't name checkpoints yet (they need the run): it's put with eval null first.
    const w = await makeBuild({ tag: "new" });
    expect(err(await admin("PUT", "/v1/admin/run/fresh", { ...runBody(w, { selfplay: false }), eval: evalCfg() }), 400, "bad_request").message).toMatch(
      /not registered.*"eval": null/,
    );
    // ...unless its seats are bots only.
    ok(await admin("PUT", "/v1/admin/run/bots", { ...runBody(w, { selfplay: false }), eval: evalCfg({ checkpoints: [], job: job({ seats: [{ bot: "heuristic" }, { bot: "random" }] }) }) }));
    await define("run", "bots");
    const m = await enroll("a");
    const bl = await lease(m);
    expect([bl.run, bl.checkpoints, bl.checkpoint]).toEqual(["bots", [], null]);
  });

  it("a batch can't change and its name can't come back; re-putting it unchanged is fine", async () => {
    const { put } = await h2h();
    const before = ok(await admin("GET", "/v1/admin/run/h2h-a-80v70")).next_gid;
    ok(await put(evalCfg({ games_per_lease: 2, gpu: "preferred" }))); // not part of the batch
    expect(ok(await admin("GET", "/v1/admin/run/h2h-a-80v70")).next_gid).toBe(before); // no new reservation
    err(await put(evalCfg({ games: 12 })), 409, "conflict");
    err(await put(evalCfg({ job: { settings: { sims: 64 }, seats: SEATS, paired: true } })), 409, "conflict");
    err(await put(evalCfg({ checkpoints: [70, 80] })), 409, "conflict"); // order is part of it
    ok(await put(evalCfg({ batch: "b2", games: 6 })));
    err(await put(evalCfg()), 409, "conflict"); // b1 was used before
    const r = ok(await admin("GET", "/v1/admin/run/h2h-a-80v70"));
    expect(r.eval_state).toMatchObject({ batch: "b2", lo: 10, hi: 16 });
    expect(r.eval_batches.map((b: any) => [b.batch, b.replaced_at !== null])).toEqual([
      ["b2", false],
      ["b1", true],
    ]);
    ok(await put(null)); // dropping eval replaces b2 too
    expect(ok(await admin("GET", "/v1/admin/run/h2h-a-80v70")).eval_state).toBeNull();
  });

  it("credits a pair only when both games are in: held games, completed by a later chunk via `also`", async () => {
    await h2h();
    const m = await enroll("a");
    const l = await lease(m); // [0, 4)
    expect(await put(m, l, 0, [0])).toEqual({ ok: true, duplicate: false, credited: 0, already_credited: 0, held: 1 });
    expect(await put(m, l, 1, [1, 2])).toEqual({ ok: true, duplicate: false, credited: 2, already_credited: 0, held: 1 });
    // A retry answers from what was stored.
    expect(await put(m, l, 1, [1, 2])).toEqual({ ok: true, duplicate: true, credited: 2, already_credited: 0, held: 1 });
    expect(await put(m, l, 2, [3])).toEqual({ ok: true, duplicate: false, credited: 2, already_credited: 0, held: 0 });
    ok(await finish(m, l.lease_id, [0, 1, 2]));

    const listed = ok(await admin("GET", "/v1/admin/run/h2h-a-80v70/results")).results;
    const k = (seq: number) => listed.find((r: any) => r.seq === seq).key;
    expect(listed.map((r: any) => ({ seq: r.seq, kind: r.kind, batch: r.batch, gen: r.gen, games: r.games, ids: r.ids, held: r.held, also: r.also }))).toEqual([
      { seq: 0, kind: "eval", batch: "b1", gen: null, games: 0, ids: [], held: [[0, 1]], also: [] },
      { seq: 1, kind: "eval", batch: "b1", gen: null, games: 2, ids: [[1, 2]], held: [[2, 3]], also: [{ seq: 0, key: k(0), ids: [[0, 1]] }] },
      { seq: 2, kind: "eval", batch: "b1", gen: null, games: 2, ids: [[3, 4]], held: [], also: [{ seq: 1, key: k(1), ids: [[2, 3]] }] },
    ]);
    const got = await readBatch("h2h-a-80v70", "b1");
    expect([...got.keys()].sort((a, b) => a - b)).toEqual([0, 1, 2, 3]);
    expect(ok(await admin("GET", "/v1/admin/run/h2h-a-80v70")).eval_state).toMatchObject({ done: 4, leased: 0, unissued: 6 });
  });

  it("a lease that ends with a held game gives the whole pair back; it is played again in full", async () => {
    await h2h();
    const [a, b] = [await enroll("a"), await enroll("b")];
    const la = await lease(a); // [0, 4)
    await put(a, la, 0, [0, 1, 2]); // pair 0 in, game 2 held
    ok(await finish(a, la.lease_id, [0], { given_back: true }));
    // The held game's pair went back whole (merged with the unissued rest).
    expect((await env.DB.prepare(`SELECT lo, hi FROM pool WHERE pool = 'eval:b1' ORDER BY lo`).all<any>()).results).toEqual([{ lo: 2, hi: 10 }]);
    const lb = await lease(b);
    expect(lb.job).toMatchObject({ gid_base: 2, games: 4 }); // the pair (2, 3) and the next one
    // A's late chunk completes its own held game first: that pair is A's, B's copy is a duplicate.
    expect(await put(a, la, 1, [3])).toMatchObject({ credited: 2, held: 0 });
    expect(await put(b, lb, 0, [2, 3, 4, 5])).toMatchObject({ credited: 2, already_credited: 2, held: 0 });
    ok(await finish(b, lb.lease_id, [0]));

    // Expiry: C reports one game of a pair and goes silent; the pair is reissued whole.
    const c = await enroll("c");
    const lc = await lease(c); // [6, 10)
    await put(c, lc, 0, [7]);
    advance(200);
    await runCron(env, clock.now); // the minute cron expires C's lease
    const ld = await lease(b);
    expect(ld.job).toMatchObject({ gid_base: 6, games: 4 });
    await put(b, ld, 0, [6, 7, 8, 9]);
    ok(await finish(b, ld.lease_id, [0]));
    // C's late copy of 6 can't complete anything: the pair is B's now.
    expect(await put(c, lc, 1, [6])).toMatchObject({ credited: 0, already_credited: 1, held: 0 });

    await expectTiled("run:h2h-a-80v70", 0, 10);
    const got = await readBatch("h2h-a-80v70", "b1");
    expect(got.size).toBe(10);
    expect(ok(await admin("GET", "/v1/admin/run/h2h-a-80v70")).eval_state).toMatchObject({ done: 10, leased: 0, unissued: 0 });
  });

  it("a paired batch starts on an even id; the skipped id goes to self-play", async () => {
    const w = await makeRun("r", { games_per_lease: 3 });
    const m = await enroll("a");
    expect((await lease(m, { kinds: ["selfplay"] })).job).toMatchObject({ gid_base: 0, games: 3 });
    ok(await admin("PUT", "/v1/admin/run/r", runBody(w, { games_per_lease: 3, eval: evalCfg({ checkpoints: [1], games: 4, job: { seats: [{ net: 1 }, { bot: "random" }], paired: true } }) })));
    await define("run", "r");
    expect(ok(await admin("GET", "/v1/admin/run/r")).eval_state).toMatchObject({ lo: 4, hi: 8 });
    expect((await lease(m, { kinds: ["eval"] })).job).toMatchObject({ gid_base: 4, games: 4 });
    expect((await lease(m, { kinds: ["selfplay"] })).job).toMatchObject({ gid_base: 3, games: 1 });
  });

  it("eval chunks ignore max_stale and x-gen", async () => {
    const w = await makeRun("r", { max_stale: 0 });
    ok(await admin("PUT", "/v1/admin/run/r", runBody(w, { max_stale: 0, eval: evalCfg({ checkpoints: [1], games: 4, job: { seats: [{ net: 1 }, { bot: "random" }], paired: true } }) })));
    await define("run", "r");
    const m = await enroll("a");
    const sp = await lease(m, { kinds: ["selfplay"] });
    const ev = await lease(m, { kinds: ["eval"] });
    await publishCkpt("r", 2);
    await publishCkpt("r", 3);
    err(await upload(m, sp.lease_id, 0, "records", { ids: [[sp.job.gid_base, sp.job.gid_base + 1]], gen: 1 }), 410, "gone"); // self-play: stale
    expect(ok(await upload(m, ev.lease_id, 0, rows([0, 1]), { ids: [[0, 2]], gen: 1 }))).toMatchObject({ credited: 2 });
    const r = ok(await admin("GET", "/v1/admin/run/r/results")).results.find((x: any) => x.kind === "eval");
    expect(r.gen).toBeNull();
  });

  it("eval leases need agent 0.2.3: older agents update first, or get other work", async () => {
    await h2h(undefined, undefined, { define: false }); // legacy agents: a run without a definition
    const m = await enroll("pc");
    const id = await makeBuild({ kind: "agent", version: "0.2.2" });
    ok(await admin("POST", "/v1/admin/agent/release", { version: "0.2.2", build_id: id, channel: "stable", min_agent: "0.2.0" }));
    expect(ok(await ask(m, { agent: "0.2.2" })).reason).toBe("no_work"); // the channel has nothing new enough
    const id3 = await makeBuild({ kind: "agent", version: "0.2.3" });
    ok(await admin("POST", "/v1/admin/agent/release", { version: "0.2.3", build_id: id3, channel: "stable", min_agent: "0.2.0" }));
    expect(ok(await ask(m, { agent: "0.2.2" }))).toEqual({ update: { min_agent: "0.2.3" } });
    expect((await lease(m, { agent: "0.2.3" })).kind).toBe("eval");
  });

  it("uncached checkpoints count against the disk cap", async () => {
    const { build } = await h2h();
    const m = await enroll("a");
    const cachedBuild = { builds: [build] };
    expect(ok(await ask(m, { caps: { disk_mb: 0 }, cached: cachedBuild })).reason).toBe("no_work");
    expect(ok(await ask(m, { caps: { disk_mb: 0 }, cached: { ...cachedBuild, ckpts: ["h2h-a-80v70/80"] } })).reason).toBe("no_work");
    const l = await lease(m, { caps: { disk_mb: 0 }, cached: { ...cachedBuild, ckpts: ["h2h-a-80v70/80", "h2h-a-80v70/70"] } });
    expect(l.kind).toBe("eval");
  });

  it("the dashboard shows eval batches and their progress; stats keep per-gen selftests", async () => {
    await h2h();
    const m = await enroll("a");
    const l = await lease(m);
    await put(m, l, 0, [0, 1]);
    ok(
      await beat(m, l.lease_id, "running", {
        stats: { games: 2, selftest_max_abs: 4e-6, selftest: { "80": { provider: "cpu", max_abs: 4e-6, ok: true }, "70": { provider: "cpu", max_abs: 1e-6, ok: true, junk: "x" }, bad: {} } },
      }),
    );
    const s = ok(await admin("GET", "/v1/admin/stats"));
    expect(s.runs.find((r: any) => r.run === "h2h-a-80v70").eval).toMatchObject({ batch: "b1", games: 10, done: 2, leased: 2, unissued: 6 });
    expect(s.eval_batches).toEqual([expect.objectContaining({ run: "h2h-a-80v70", batch: "b1", done: 2, paired: true, checkpoints: [80, 70] })]);
    const st = JSON.parse((await env.DB.prepare(`SELECT stats FROM leases WHERE lease_id = ?`).bind(l.lease_id).first<any>()).stats);
    expect(st.selftest).toEqual({ "80": { provider: "cpu", max_abs: 4e-6, ok: true }, "70": { provider: "cpu", max_abs: 1e-6, ok: true } });
    const page = (await admin("GET", "/admin")).body as string;
    expect(page).toContain("Eval batches");
  });
});
