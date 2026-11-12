// Result uploads: idempotent, size-limited, first copy wins, late copies, staleness, the trainer's
// cursor, and deletion after ack.

import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { runCron } from "../src/cron";
import {
  admin,
  advance,
  call,
  clock,
  complete,
  enroll,
  err,
  expectTiled,
  finish,
  idsOf,
  lease,
  makeRun,
  ok,
  publishCkpt,
  sha256,
  upload,
} from "./helpers";

describe("result uploads", () => {
  it("are idempotent: same seq + same hash -> 200; a different hash -> 409", async () => {
    await makeRun("r");
    const m = await enroll("a");
    const l = await lease(m);
    const first = ok(await upload(m, l.lease_id, 0, "records A", { ids: [[0, 3]] }));
    expect(first).toEqual({ ok: true, duplicate: false, credited: 3, already_credited: 0 });
    expect(ok(await upload(m, l.lease_id, 0, "records A", { ids: [[0, 3]] }))).toEqual({ ok: true, duplicate: true, credited: 3, already_credited: 0 });
    err(await upload(m, l.lease_id, 0, "records B", { ids: [[0, 3]] }), 409, "conflict");
    const listed = (await env.BUCKET.list({ prefix: `results/r/${l.lease_id}/` })).objects.map((o) => o.key);
    expect(listed).toEqual([`results/r/${l.lease_id}/0-${(await sha256("records A")).slice(0, 16)}`]); // B's bytes were removed
    expect(await (await env.BUCKET.get(listed[0]))!.text()).toBe("records A");
    expect((await env.DB.prepare(`SELECT COUNT(*) AS n FROM claims`).first<any>()).n).toBe(1);
  });

  it("check the body's hash, size, seq and ids", async () => {
    await makeRun("r");
    const m = await enroll("a");
    const l = await lease(m);
    err(await upload(m, l.lease_id, 0, "x", { sha: "0".repeat(64) }), 400, "bad_request");
    err(await upload(m, l.lease_id, 0, "x", { sha: "nothex" }), 400, "bad_request");
    err(await call("PUT", `/v1/lease/${l.lease_id}/result/0`, { token: m.token, raw: "x" }), 400, "bad_request"); // no x-sha256
    err(await upload(m, l.lease_id, -1 as number, "x"), 400, "bad_request");
    err(await upload(m, l.lease_id, 0, "x", { ids: [[0, 11]] }), 400, "bad_request"); // outside [0, 10)
    err(await upload(m, l.lease_id, 0, "x", { ids: [[2, 1]] }), 400, "bad_request");
    err(
      await call("PUT", `/v1/lease/${l.lease_id}/result/0`, { token: m.token, raw: "x", headers: { "x-sha256": await sha256("x"), "x-ids": "[[0," } }),
      400,
      "bad_request",
    );
    // > 32 MB
    const big = new Uint8Array(32 * 1024 * 1024 + 1);
    err(
      await call("PUT", `/v1/lease/${l.lease_id}/result/1`, { token: m.token, raw: big, headers: { "x-sha256": await sha256(big) } }),
      413,
      "too_large",
    );
    // Exactly 32 MB is fine.
    const max = big.subarray(0, 32 * 1024 * 1024);
    ok(await call("PUT", `/v1/lease/${l.lease_id}/result/1`, { token: m.token, raw: max, headers: { "x-sha256": await sha256(max) } }));
  });

  it("a late copy from an expired lease is accepted; the first copy of each game id wins", async () => {
    await makeRun("r", { games_per_lease: 10 });
    const a = await enroll("a");
    const b = await enroll("b");
    const la = await lease(a);
    advance(200); // a goes silent; its ids [0, 10) return to the pool
    const lb = await lease(b);
    expect(idsOf(lb)).toEqual([0, 10]);
    // a comes back with games 0-5 (first copies: credited, and b now owes only 6-9).
    expect(ok(await upload(a, la.lease_id, 0, "a: 0-5", { ids: [[0, 6]] }))).toMatchObject({ credited: 6, already_credited: 0 });
    // b uploads all ten: 0-5 are duplicates.
    expect(ok(await upload(b, lb.lease_id, 0, "b: 0-9", { ids: [[0, 10]] }))).toMatchObject({ credited: 4, already_credited: 6 });
    ok(await finish(b, lb.lease_id, [0]));
    const res = ok(await admin("GET", "/v1/admin/run/r/results")).results;
    expect(res.map((r: any) => [r.machine_id, r.games, r.ids, r.dup_ids])).toEqual([
      [a.machine_id, 6, [[0, 6]], []],
      [b.machine_id, 4, [[6, 10]], [[0, 6]]],
    ]);
    await expectTiled("run:r", 0, 10);
  });

  it("a late copy of ids still in the pool takes them out of it", async () => {
    await makeRun("r", { games_per_lease: 10 });
    const a = await enroll("a");
    const la = await lease(a);
    advance(200);
    await call("POST", "/v1/lease", { token: a.token, body: {} }); // (any request; expiry also happens lazily)
    await runCron(env, clock.now);
    ok(await upload(a, la.lease_id, 0, "a: 0-3", { ids: [[0, 4]] }));
    await expectTiled("run:r", 0, 10);
    const pool = (await env.DB.prepare(`SELECT lo, hi FROM pool`).all<any>()).results;
    expect(pool).toEqual([{ lo: 4, hi: 10 }]);
  });

  it("late copies are taken after the trainer acked the lease's earlier chunks (it reads by cursor)", async () => {
    await makeRun("r", { games_per_lease: 10 });
    const a = await enroll("a");
    const l1 = await lease(a);
    ok(await upload(a, l1.lease_id, 0, "0-1", { ids: [[0, 2]] }));
    const cur = ok(await admin("GET", "/v1/admin/run/r/results")).cursor;
    ok(await admin("POST", "/v1/admin/run/r/ack", { cursor: cur }));
    advance(200); // l1 expires
    expect(ok(await upload(a, l1.lease_id, 1, "2-3", { ids: [[2, 4]] })).credited).toBe(2);
    const next = ok(await admin("GET", `/v1/admin/run/r/results?after=${cur}`)).results;
    expect(next.map((r: any) => r.ids)).toEqual([[[2, 4]]]);
    ok(await upload(a, l1.lease_id, 0, "0-1", { ids: [[0, 2]] })); // an exact repeat is fine
  });

  it("self-play results older than max_stale generations are refused (their ids stay unclaimed)", async () => {
    await makeRun("r", { games_per_lease: 10, max_stale: 1, quota: 1000 });
    const a = await enroll("a");
    const l = await lease(a); // gen 1
    await publishCkpt("r", 2);
    ok(await upload(a, l.lease_id, 0, "gen 1 games", { ids: [[0, 2]], gen: 1 })); // 1 >= 2 - 1
    await publishCkpt("r", 3);
    err(await upload(a, l.lease_id, 1, "gen 1 games", { ids: [[2, 4]], gen: 1 }), 410, "gone"); // 1 < 3 - 1
    err(await upload(a, l.lease_id, 1, "gen 1 games", { ids: [[2, 4]] }), 410, "gone"); // x-gen defaults to the lease's
    ok(await upload(a, l.lease_id, 1, "gen 3 games", { ids: [[2, 4]], gen: 3 }));
    err(await upload(a, l.lease_id, 2, "?", { ids: [[4, 5]], gen: 9 }), 400, "bad_request"); // newer than current
    ok(await finish(a, l.lease_id, [0, 1]));
    expect(idsOf(await lease(a))).toEqual([4, 10]); // the refused/unplayed ids are reissued
  });

  it("the trainer pages through results with a cursor, per run", async () => {
    await makeRun("r", { games_per_lease: 4, quota: 1000 });
    await makeRun("s", { games_per_lease: 4, quota: 1000 });
    const a = await enroll("a");
    const ls = [];
    for (let i = 0; i < 6; i++) ls.push(await lease(a)); // shared 3/3 by active leases
    for (const l of ls) await complete(a, l);
    const p1 = ok(await admin("GET", "/v1/admin/run/r/results?limit=2"));
    expect(p1.results).toHaveLength(2);
    const p2 = ok(await admin("GET", `/v1/admin/run/r/results?after=${p1.cursor}`));
    expect(p2.results).toHaveLength(1);
    expect(ok(await admin("GET", `/v1/admin/run/r/results?after=${p2.cursor}`))).toEqual({ results: [], cursor: p2.cursor });
    for (const r of [...p1.results, ...p2.results]) {
      expect(r).toMatchObject({ kind: "selfplay", gen: 1, games: 4, dup_ids: [], machine_id: a.machine_id, seq: 0 });
      expect(r.key).toBe(`results/r/${r.lease_id}/0-${r.sha256.slice(0, 16)}`);
      expect((await admin("GET", `/v1/admin/artifact/${r.key}`)).status).toBe(200);
    }
    expect(ok(await admin("GET", "/v1/admin/run/s/results")).results).toHaveLength(3);
  });

  it("acked result objects are deleted from R2 after 7 days", async () => {
    await makeRun("r");
    const a = await enroll("a");
    const l = await lease(a);
    await complete(a, l);
    const key = ok(await admin("GET", "/v1/admin/run/r/results")).results[0].key;
    const cur = ok(await admin("GET", "/v1/admin/run/r/results")).cursor;
    ok(await admin("POST", "/v1/admin/run/r/ack", { cursor: cur }));
    expect((await runCron(env, clock.now + 6 * 86400)).deleted).toBe(0);
    expect(await env.BUCKET.head(key)).not.toBeNull();
    expect((await runCron(env, clock.now + 7 * 86400 + 1)).deleted).toBe(1);
    expect(await env.BUCKET.head(key)).toBeNull();
    expect((await runCron(env, clock.now + 8 * 86400)).deleted).toBe(0);
  });
});
