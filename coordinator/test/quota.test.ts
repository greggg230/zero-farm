// The per-generation self-play quota: accepted + in-flight against quota x (1 + slack).

import { describe, expect, it } from "vitest";
import { admin, ask, complete, enroll, finish, idsOf, lease, makeCampaign, makeRun, ok, publishCkpt, upload } from "./helpers";

const state = async (run: string) => ok(await admin("GET", `/v1/admin/run/${run}`)).selfplay_state;

describe("quota and slack", () => {
  it("stops issuing at quota x (1 + slack); the last lease is trimmed to fit", async () => {
    await makeRun("r", { quota: 20, slack: 0.1, games_per_lease: 8 });
    const m = await enroll("a");
    const sizes = [];
    for (let i = 0; i < 3; i++) sizes.push((await lease(m)).job.games);
    expect(sizes).toEqual([8, 8, 6]); // cap 22
    expect(ok(await ask(m)).reason).toBe("no_work");
    expect(await state("r")).toMatchObject({ gen: 1, quota: 20, cap: 22, accepted: 0, leased: 22, pending: 0, in_flight: 22, headroom: 0 });
  });

  it("in-flight = unclaimed leased ids + uploaded games the trainer hasn't acked", async () => {
    await makeRun("r", { quota: 10, slack: 0, games_per_lease: 10 });
    const m = await enroll("a");
    const l = await lease(m);
    ok(await upload(m, l.lease_id, 0, "4", { ids: [[0, 4]] }));
    expect(await state("r")).toMatchObject({ leased: 6, pending: 4, in_flight: 10, headroom: 0 });
    ok(await upload(m, l.lease_id, 1, "6", { ids: [[4, 10]] }));
    ok(await finish(m, l.lease_id, [0, 1]));
    expect(await state("r")).toMatchObject({ leased: 0, pending: 10, headroom: 0 });
    // The trainer accepts 7 of them and rejects 3 (replay failure), then acks: 3 games free up.
    const res = ok(await admin("GET", "/v1/admin/run/r/results"));
    ok(await admin("POST", "/v1/admin/run/r/accept", { gen: 1, games: 7 }));
    ok(await admin("POST", "/v1/admin/run/r/ack", { cursor: res.cursor }));
    expect(await state("r")).toMatchObject({ accepted: 7, pending: 0, in_flight: 0, headroom: 3 });
    expect((await lease(m)).job.games).toBe(3);
  });

  it("dropped games come back without exceeding the cap", async () => {
    await makeRun("r", { quota: 10, slack: 0, games_per_lease: 10 });
    const a = await enroll("a");
    const b = await enroll("b");
    const l = await lease(a);
    ok(await upload(a, l.lease_id, 0, "6", { ids: [[0, 6]] }));
    ok(await finish(a, l.lease_id, [0], { given_back: true }));
    expect(await state("r")).toMatchObject({ leased: 0, pending: 6, headroom: 4 });
    const l2 = await lease(b);
    expect(idsOf(l2)).toEqual([6, 10]); // the given-back ids, not new ones
    expect(ok(await ask(b)).reason).toBe("no_work");
  });

  it("a new checkpoint resets the generation's count; extra games carry over as in-flight", async () => {
    await makeRun("r", { quota: 10, slack: 0.2, games_per_lease: 6 });
    const m = await enroll("a");
    const l1 = await lease(m);
    const l2 = await lease(m);
    await complete(m, l1);
    await complete(m, l2); // 12 games = the cap
    const res = ok(await admin("GET", "/v1/admin/run/r/results"));
    ok(await admin("POST", "/v1/admin/run/r/accept", { gen: 1, games: 10 }));
    ok(await admin("POST", "/v1/admin/run/r/ack", { cursor: res.cursor }));
    expect(await state("r")).toMatchObject({ accepted: 10, headroom: 2 });
    // The trainer had 2 extra games; it moves on and counts them toward gen 2.
    await publishCkpt("r", 2);
    expect(await state("r")).toMatchObject({ gen: 2, accepted: 0, headroom: 12 });
    ok(await admin("POST", "/v1/admin/run/r/accept", { gen: 2, games: 2 }));
    expect(await state("r")).toMatchObject({ gen: 2, accepted: 2, headroom: 10 });
    const l3 = await lease(m);
    expect(l3.checkpoint.gen).toBe(2);
    expect(l3.job.games).toBe(6);
    // accept is the trainer's total, not an increment: repeating it changes nothing.
    ok(await admin("POST", "/v1/admin/run/r/accept", { gen: 2, games: 2 }));
    expect((await state("r")).accepted).toBe(2);
  });

  it("when the quota is full, machines get fuzz", async () => {
    await makeRun("r", { quota: 5, slack: 0, games_per_lease: 5 });
    await makeCampaign("fz");
    const m = await enroll("a");
    expect((await lease(m)).kind).toBe("selfplay");
    expect((await lease(m)).kind).toBe("fuzz");
  });

  it("an expired lease's games stop counting as in flight", async () => {
    await makeRun("r", { quota: 10, slack: 0, games_per_lease: 10 });
    const a = await enroll("a");
    const l = await lease(a);
    ok(await upload(a, l.lease_id, 0, "2", { ids: [[0, 2]] }));
    expect((await state("r")).headroom).toBe(0);
    const later = 1_800_000_000 + 1000;
    expect(ok(await ask(a, { now: later })).lease.job).toMatchObject({ gid_base: 2, games: 8 });
  });
});
