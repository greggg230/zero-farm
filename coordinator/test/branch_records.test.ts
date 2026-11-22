// Branch games in self-play records (records/2, PROTOCOL.md section 16; zero-core docs/az.md 72.7): a branch game is one
// more line of its game's records file under its game's id, so the coordinator - which credits ids, never reads a
// records file - credits the two once, as one id; another lease's copy of that id is a duplicate whole (its branch with
// it); the trainer's `accept` total counts every accepted game, branches included, and the quota stops on it.

import { describe, expect, it } from "vitest";
import { admin, advance, ask, enroll, finish, lease, makeRun, ok, upload } from "./helpers";

const state = async (run: string) => ok(await admin("GET", `/v1/admin/run/${run}`)).selfplay_state;

// A records/2 file's lines as the worker writes them: each game, and right after it (same file) its branch, if any.
function records2(games: number[], branched: Set<number>): string {
  const lines = [JSON.stringify({ records: 2, games: games.length + [...branched].length })];
  for (const g of games) {
    lines.push(JSON.stringify({ gid: g }));
    if (branched.has(g)) lines.push(JSON.stringify({ gid: g, branch: { from: 7, merged: null } }));
  }
  return lines.join("\n");
}

describe("branch games in records/2", () => {
  it("ride on their game's id: credited once with it, a later copy a duplicate whole", async () => {
    await makeRun("r", { games_per_lease: 6 });
    const a = await enroll("a");
    const b = await enroll("b");
    const la = await lease(a);
    advance(200); // a goes silent; its ids [0, 6) go back to the pool and b gets them
    const lb = await lease(b);
    expect(lb.job.gid_base).toBe(0);
    // a comes back: games 0-5, four with a branch line beside them - six ids credited, not ten
    expect(ok(await upload(a, la.lease_id, 0, records2([0, 1, 2, 3, 4, 5], new Set([0, 2, 3, 5])), { ids: [[0, 6]] }))).toEqual({
      ok: true,
      duplicate: false,
      credited: 6,
      already_credited: 0,
    });
    // b's copies of games 0-5 and their branches: duplicates, game and branch alike (the trainer skips their lines)
    expect(ok(await upload(b, lb.lease_id, 0, records2([0, 1, 2, 3, 4, 5], new Set([1, 2])), { ids: [[0, 6]] }))).toMatchObject({
      credited: 0,
      already_credited: 6,
    });
    ok(await finish(b, lb.lease_id, [0]));
    const res = ok(await admin("GET", "/v1/admin/run/r/results")).results;
    expect(res.map((r: any) => [r.machine_id, r.games, r.ids, r.dup_ids])).toEqual([
      [a.machine_id, 6, [[0, 6]], []],
      [b.machine_id, 0, [], [[0, 6]]],
    ]);
  });

  it("the quota counts the trainer's accepted games, branches included", async () => {
    await makeRun("r", { quota: 10, slack: 0, games_per_lease: 10 });
    const m = await enroll("a");
    const l = await lease(m);
    ok(await upload(m, l.lease_id, 0, records2([0, 1, 2, 3, 4, 5, 6, 7, 8, 9], new Set([1, 4, 6, 8])), { ids: [[0, 10]] }));
    ok(await finish(m, l.lease_id, [0]));
    expect(await state("r")).toMatchObject({ pending: 10, in_flight: 10, headroom: 0 });
    // the pool replays ten games and four branches, and accepts all fourteen: more than the quota's ten
    const res = ok(await admin("GET", "/v1/admin/run/r/results"));
    ok(await admin("POST", "/v1/admin/run/r/accept", { gen: 1, games: 14 }));
    ok(await admin("POST", "/v1/admin/run/r/ack", { cursor: res.cursor }));
    // (headroom goes below 0: the cap is reached by games, not ids - no more leases)
    expect(await state("r")).toMatchObject({ accepted: 14, pending: 0, in_flight: 0, headroom: -4 });
    expect(ok(await ask(m)).reason).toBe("no_work");
  });
});
