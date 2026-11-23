// Rev 30 (PROTOCOL 22): single games. A result's `games` counts credited ids (a game or a whole match each); the
// trainer's ack may say how many single games those ids held (`single_games`), stored per result and listed with it.
// The owner's own machines report the same split (POST /v1/admin/external `single_games`, `uncounted`).

import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { admin, clock, enroll, err, lease, makeCampaign, makeRun, ok, upload } from "./helpers";

const rows = async () =>
  (await env.DB.prepare(`SELECT id, games, single_games, acked_at FROM results ORDER BY id`).all<any>()).results;

describe("single games (rev 30)", () => {
  it("an ack stores the trainer's count per result; results list it; an ack without it is as before", async () => {
    await makeRun("r", { games_per_lease: 10 });
    const m = await enroll("a");
    const l = await lease(m);
    ok(await upload(m, l.lease_id, 0, "records A", { ids: [[0, 4]] }));
    ok(await upload(m, l.lease_id, 1, "records B", { ids: [[4, 6]] }));
    ok(await upload(m, l.lease_id, 2, "records C", { ids: [[6, 9]] }));
    const res = ok(await admin("GET", "/v1/admin/run/r/results"));
    expect(res.results.map((r: any) => [r.games, r.single_games])).toEqual([[4, null], [2, null], [3, null]]);
    const [a, b, c] = res.results.map((r: any) => r.id);
    // an ack without counts (a trainer before rev 30): the answer and the rows of before
    expect(ok(await admin("POST", "/v1/admin/run/r/ack", { cursor: a }))).toEqual({ ok: true, acked: 1 });
    expect((await rows())[0]).toMatchObject({ single_games: null });
    // counts with the ack: 4 matches of 2-3 games, 2 single games
    const ans = ok(await admin("POST", "/v1/admin/run/r/ack", { cursor: b, single_games: [[a, 10], [b, 2]] }));
    expect(ans).toEqual({ ok: true, acked: 1, single_games: { stored: 2, ignored: 0 } });
    expect((await rows()).map((r: any) => r.single_games)).toEqual([10, 2, null]);
    const again = ok(await admin("GET", "/v1/admin/run/r/results"));
    expect(again.results.map((r: any) => r.single_games)).toEqual([10, 2, null]);
    // the same ack again: nothing changes, the entries count as stored
    expect(ok(await admin("POST", "/v1/admin/run/r/ack", { cursor: b, single_games: [[a, 10], [b, 2]] }))).toEqual({
      ok: true,
      acked: 0,
      single_games: { stored: 2, ignored: 0 },
    });
    void c;
  });

  it("entries that cannot be right are left out and never fail the ack", async () => {
    await makeRun("r", { games_per_lease: 10 });
    const m = await enroll("a");
    const l = await lease(m);
    ok(await upload(m, l.lease_id, 0, "records A", { ids: [[0, 4]] }));
    ok(await upload(m, l.lease_id, 1, "records B", { ids: [[4, 6]] }));
    const [a, b] = ok(await admin("GET", "/v1/admin/run/r/results")).results.map((r: any) => r.id);
    const ans = ok(
      await admin("POST", "/v1/admin/run/r/ack", {
        cursor: a,
        single_games: [
          [a, 3], // fewer single games than ids
          [b, 5], // beyond the cursor
          [9999, 1], // no such result
        ],
      }),
    );
    expect(ans).toEqual({ ok: true, acked: 1, single_games: { stored: 0, ignored: 3 } });
    expect((await rows()).map((r: any) => [r.single_games, r.acked_at !== null])).toEqual([[null, true], [null, false]]);
    await makeRun("other", { games_per_lease: 10 });
    // more than 64 single games an id; another run's ack; a count once stored is never replaced by another
    expect(ok(await admin("POST", "/v1/admin/run/r/ack", { cursor: b, single_games: [[a, 4 * 64 + 1]] })).single_games).toEqual({ stored: 0, ignored: 1 });
    expect(ok(await admin("POST", "/v1/admin/run/other/ack", { cursor: b, single_games: [[a, 9]] })).single_games).toEqual({ stored: 0, ignored: 1 });
    expect(ok(await admin("POST", "/v1/admin/run/r/ack", { cursor: b, single_games: [[a, 9], [b, 2]] })).single_games).toEqual({ stored: 2, ignored: 0 });
    expect(ok(await admin("POST", "/v1/admin/run/r/ack", { cursor: b, single_games: [[a, 8]] })).single_games).toEqual({ stored: 0, ignored: 1 });
    expect((await rows()).map((r: any) => r.single_games)).toEqual([9, 2]);
  });

  it("the body's shape is checked", async () => {
    await makeRun("r");
    for (const single_games of [5, "x", [[1]], [[1, 2, 3]], [[0, 1]], [[1, -1]], [[1.5, 1]], [["1", 1]], Array.from({ length: 1001 }, (_, i) => [i + 1, 1])]) {
      err(await admin("POST", "/v1/admin/run/r/ack", { cursor: 1, single_games }), 400, "bad_request");
    }
    expect(ok(await admin("POST", "/v1/admin/run/r/ack", { cursor: 1, single_games: [] }))).toEqual({ ok: true, acked: 0, single_games: { stored: 0, ignored: 0 } });
    expect(ok(await admin("POST", "/v1/admin/run/r/ack", { cursor: 1, single_games: null }))).toEqual({ ok: true, acked: 0 });
  });

  it("fuzz results never take a count (their ids are seeds)", async () => {
    await makeCampaign("fz", { seed_base: 0, chunk: 100 });
    const m = await enroll("a");
    const l = await lease(m, { kinds: ["fuzz"] });
    expect(l.kind).toBe("fuzz");
    const body = JSON.stringify({ campaign: "fz", mode: "delver", first: 0, count: 100, done: 5, failures: [], build_id: "b", engine_commit: "e" });
    ok(await upload(m, l.lease_id, 0, body, { ids: [[0, 5]] }));
    const [a] = ok(await admin("GET", "/v1/admin/campaign/fz/results")).results.map((r: any) => r.id);
    expect(ok(await admin("POST", "/v1/admin/campaign/fz/ack", { cursor: a, single_games: [[a, 5]] })).single_games).toEqual({ stored: 0, ignored: 1 });
    expect((await rows())[0].single_games).toBeNull();
  });

  it("an external report's single games and uncounted ids are stored; a report without them stores NULL", async () => {
    const src = (o: Record<string, unknown>) => ({
      id: "local",
      name: "Desktop",
      kind: "local",
      working_on: [],
      selfplay_games: { "1h": 100, "24h": 2000 },
      eval_games: { "1h": 10, "24h": 40 },
      ...o,
    });
    const post = (s: object) => admin("POST", "/v1/admin/external", { reporter: "desktop", t: clock.now, sources: [s] });
    const row = () => env.DB.prepare(`SELECT single_1h, single_24h, uncounted_1h, uncounted_24h FROM external_sources`).first<any>();
    ok(await post(src({})));
    expect(await row()).toEqual({ single_1h: null, single_24h: null, uncounted_1h: null, uncounted_24h: null });
    ok(await post(src({ single_games: { "1h": 250, "24h": 5000 }, uncounted: { "1h": 10, "24h": 40 } })));
    expect(await row()).toEqual({ single_1h: 250, single_24h: 5000, uncounted_1h: 10, uncounted_24h: 40 });
    err(await post(src({ single_games: { "1h": -1 } })), 400, "bad_request");
    err(await post(src({ uncounted: { "2h": 1 } })), 400, "bad_request");
  });
});
