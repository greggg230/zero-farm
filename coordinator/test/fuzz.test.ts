// Fuzz campaigns: the never-rewinding seed cursor, the pool, failures.

import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { admin, advance, enroll, err, expectTiled, finish, idsOf, lease, makeBuild, makeCampaign, ok, upload } from "./helpers";

const fuzzResult = (first: number, count: number, done: number, failures: object[] = []) =>
  JSON.stringify({ campaign: "fz", mode: "delver", first, count, done, failures, build_id: "b", engine_commit: "e" });

describe("fuzz campaigns", () => {
  it("hands out consecutive seed chunks from seed_base", async () => {
    await makeCampaign("fz", { seed_base: 10_000_000, chunk: 20000 });
    const m = await enroll("a");
    const l1 = await lease(m);
    const l2 = await lease(m);
    expect(l1.job).toEqual({ kind: "fuzz", mode: "delver", env: { FUZZ_POOL: "delver" }, first: 10_000_000, count: 20000 });
    expect(l2.job.first).toBe(10_020_000);
    expect(l1).toMatchObject({ kind: "fuzz", run: null, campaign: "fz", checkpoint: null, runtime: null });
  });

  it("the cursor never rewinds; a higher seed_base moves it up", async () => {
    const build = await makeBuild();
    const put = (seed_base: number) =>
      admin("PUT", "/v1/admin/campaign/fz", { build_id: build, mode: "delver", env: {}, seed_base, chunk: 100 });
    expect(ok(await put(1000)).cursor).toBe(1000);
    const m = await enroll("a");
    await lease(m);
    await lease(m); // cursor 1200
    expect(ok(await put(0)).cursor).toBe(1200); // lower seed_base: ignored
    expect(ok(await put(1100)).cursor).toBe(1200);
    expect(idsOf(await lease(m))).toEqual([1200, 1300]);
    expect(ok(await put(5000)).cursor).toBe(5000); // jump forward
    expect(idsOf(await lease(m))).toEqual([5000, 5100]);
    // A new build for the same campaign keeps the cursor.
    const build2 = await makeBuild({ tag: "v2" });
    expect(ok(await admin("PUT", "/v1/admin/campaign/fz", { build_id: build2, mode: "delver", env: {}, seed_base: 1000, chunk: 100 })).cursor).toBe(5100);
    expect((await lease(m)).build.build_id).toBe(build2);
  });

  it("reissues pooled seeds before moving the cursor", async () => {
    await makeCampaign("fz", { seed_base: 0, chunk: 100 });
    const a = await enroll("a");
    const b = await enroll("b");
    await lease(a); // [0, 100)
    await lease(b); // [100, 200)
    advance(200); // both silent
    const l1 = await lease(b);
    const l2 = await lease(b);
    const l3 = await lease(b);
    expect([idsOf(l1), idsOf(l2), idsOf(l3)]).toEqual([[0, 100], [100, 200], [200, 300]]); // merged in the pool, reissued in chunks
    for (const l of [l1, l2, l3]) {
      const [lo, hi] = idsOf(l);
      ok(await upload(b, l.lease_id, 0, fuzzResult(lo, hi - lo, hi - lo), { ids: [[lo, hi]] }));
      ok(await finish(b, l.lease_id, [0]));
    }
    await expectTiled("campaign:fz", 0, 300);
  });

  it("parses failures: deduped by (seed, site), first of each site flagged, listed with a cursor", async () => {
    await makeCampaign("fz", { seed_base: 0, chunk: 100 });
    const m = await enroll("a");
    const l = await lease(m);
    const f1 = { seed: 12, panic: "index out of bounds", site: "engine/src/stack.rs:88", repro: "(sha, env, 12)", output_tail: "..." };
    const f2 = { seed: 40, panic: "index out of bounds", site: "engine/src/stack.rs:88", repro: "(sha, env, 40)" };
    const f3 = { seed: 77, panic: "unreachable", site: "engine/src/combat.rs:301", repro: "(sha, env, 77)" };
    ok(await upload(m, l.lease_id, 0, fuzzResult(0, 100, 13, [f1]))); // per-failure result, no ids
    ok(await upload(m, l.lease_id, 1, fuzzResult(0, 100, 100, [f1, f2, f3]), { ids: [[0, 100]] })); // the final one repeats f1
    ok(await finish(m, l.lease_id, [0, 1]));
    const page1 = ok(await admin("GET", "/v1/admin/campaign/fz/failures?limit=2"));
    expect(page1.failures.map((f: any) => [f.seed, f.first_of_site])).toEqual([[12, true], [40, false]]);
    expect(page1.failures[0]).toMatchObject({ repro: "(sha, env, 12)", machine_id: m.machine_id, lease_id: l.lease_id, mode: "delver" });
    const page2 = ok(await admin("GET", `/v1/admin/campaign/fz/failures?after=${page1.cursor}`));
    expect(page2.failures.map((f: any) => [f.seed, f.first_of_site])).toEqual([[77, true]]);
    expect(ok(await admin("GET", "/v1/admin/campaign/fz"))).toMatchObject({ failures: 3, sites: 2, seeds_done: 100 });
  });

  it("rejects malformed fuzz results", async () => {
    await makeCampaign("fz", { seed_base: 0, chunk: 100 });
    const m = await enroll("a");
    const l = await lease(m);
    err(await upload(m, l.lease_id, 0, "not json", { ids: [[0, 100]] }), 400, "bad_request");
    err(await upload(m, l.lease_id, 0, JSON.stringify({ failures: [{ seed: "x", site: 1 }] })), 400, "bad_request");
    err(await upload(m, l.lease_id, 0, JSON.stringify({ nofailures: true })), 400, "bad_request");
    // Nothing was stored, so seq 0 is still free.
    ok(await upload(m, l.lease_id, 0, fuzzResult(0, 100, 100), { ids: [[0, 100]] }));
    expect((await env.DB.prepare(`SELECT COUNT(*) AS n FROM results`).first<any>()).n).toBe(1);
  });

  it("campaign results are listed and acked like a run's", async () => {
    await makeCampaign("fz", { seed_base: 0, chunk: 100 });
    const m = await enroll("a");
    const l = await lease(m);
    ok(await upload(m, l.lease_id, 0, fuzzResult(0, 100, 100), { ids: [[0, 100]] }));
    const r = ok(await admin("GET", "/v1/admin/campaign/fz/results"));
    expect(r.results[0]).toMatchObject({ kind: "fuzz", games: 100, ids: [[0, 100]], dup_ids: [], });
    expect(r.results[0].key).toMatch(new RegExp(`^results/fz/${l.lease_id}/0-[0-9a-f]{16}$`));
    expect(ok(await admin("POST", "/v1/admin/campaign/fz/ack", { cursor: r.cursor })).acked).toBe(1);
  });
});
