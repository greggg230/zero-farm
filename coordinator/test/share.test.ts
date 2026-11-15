// Background eval batches (PROTOCOL 5.3, rev 13: analysis campaigns): an eval batch with a `share` goes before other
// runs' games only while it keeps that share of the runs' active leases, and gets every machine the other runs have
// nothing for; without `share` evaluation still goes first (rev 7); the setting's checks; GET shows it.

import { describe, expect, it } from "vitest";
import { jobRank } from "../src/routing";
import { admin, define, enroll, err, lease, makeBuild, makeRun, ok, publishCkpt, runBody } from "./helpers";

const SEATS = [{ net: 2 }, { net: 1 }];

function evalCfg(o: Record<string, unknown> = {}) {
  return { batch: "b1", games: 40, checkpoints: [2, 1], games_per_lease: 2, job: { settings: { eval_sims: 64, eval_trace: 1 }, seats: SEATS, paired: true, seed: 0 }, ...o };
}

// An analysis campaign: a run of its own (no self-play), two gens, a batch, a signed definition.
async function campaign(run: string, ev: Record<string, unknown>) {
  const build = await makeBuild({ tag: run });
  const body = (e: unknown) => ({ ...runBody(build, { selfplay: false }), eval: e });
  ok(await admin("PUT", `/v1/admin/run/${run}`, body(null)));
  await publishCkpt(run, 2);
  await publishCkpt(run, 1);
  const put = await admin("PUT", `/v1/admin/run/${run}`, body(ev));
  if (put.status === 200) await define("run", run);
  return { build, put, body };
}

describe("background eval batches (rev 13)", () => {
  it("jobRank: evaluation first, a background batch only within its share", () => {
    expect(jobRank("eval", undefined, 5, 5)).toBe(0);
    expect(jobRank("selfplay", undefined, 0, 0)).toBe(1);
    expect(jobRank("fuzz", undefined, 0, 0)).toBe(1);
    // 1 of 4 run leases at share 0.25
    expect(jobRank("eval", 0.25, 0, 0)).toBe(2);
    expect(jobRank("eval", 0.25, 0, 3)).toBe(0);
    expect(jobRank("eval", 0.25, 1, 4)).toBe(2);
    expect(jobRank("eval", 0.25, 1, 7)).toBe(0);
    expect(jobRank("eval", 1, 9, 9)).toBe(0);
  });

  it("takes its share of the machines beside a live run's self-play, and the rest go to the self-play", async () => {
    await makeRun("live", { quota: 100000, games_per_lease: 10 });
    await campaign("cmp-bg", evalCfg({ share: 0.25 }));
    const got: string[] = [];
    for (let i = 0; i < 8; i++) got.push((await lease(await enroll(`pc${i}`))).run);
    // the share counts this lease: machine 4 (1 of 4) and machine 8 (2 of 8)
    expect(got).toEqual(["live", "live", "live", "cmp-bg", "live", "live", "live", "cmp-bg"]);
  });

  it("gets every machine the other runs have nothing for (a quota filled, a paused run)", async () => {
    await makeRun("full", { quota: 10, slack: 0, games_per_lease: 10 });
    await campaign("cmp-bg2", evalCfg({ share: 0.1 }));
    expect((await lease(await enroll("a"))).run).toBe("full"); // the whole quota in one lease
    for (const n of ["b", "c", "d"]) expect((await lease(await enroll(n))).run).toBe("cmp-bg2");
  });

  it("without share an eval batch still goes first (rev 7)", async () => {
    await makeRun("live2", { quota: 100000, games_per_lease: 10 });
    await campaign("cmp-fg", evalCfg());
    expect((await lease(await enroll("x"))).run).toBe("cmp-fg");
    expect((await lease(await enroll("y"))).run).toBe("cmp-fg");
  });

  it("checks share and keeps it out of a config that has none", async () => {
    for (const [i, bad] of [0, -0.1, 1.5, "0.3", true].entries()) {
      const { put } = await campaign(`cmp-bad-${i}`, evalCfg({ share: bad }));
      err(put, 400, "bad_request");
    }
    const { body } = await campaign("cmp-ok", evalCfg({ share: 0.3 }));
    expect(ok(await admin("GET", "/v1/admin/run/cmp-ok")).eval.share).toBe(0.3);
    // the batch is the same batch with another share (share is not part of a batch's identity)
    ok(await admin("PUT", "/v1/admin/run/cmp-ok", body(evalCfg({ share: 0.5 }))));
    expect(ok(await admin("GET", "/v1/admin/run/cmp-ok")).eval.share).toBe(0.5);
    ok(await admin("PUT", "/v1/admin/run/cmp-ok", body(evalCfg({ share: null }))));
    expect("share" in ok(await admin("GET", "/v1/admin/run/cmp-ok")).eval).toBe(false);
  });

  it("a background self-play (a self-play campaign) takes its share the same way; share checked there too", async () => {
    await makeRun("live3", { quota: 100000, games_per_lease: 10 });
    const build = await makeBuild({ tag: "cmp-sp" });
    const body = runBody(build, { quota: 40, slack: 0, games_per_lease: 4 }) as any;
    ok(await admin("PUT", "/v1/admin/run/cmp-sp", { ...body, selfplay: { ...body.selfplay, share: 0.25 } }));
    await publishCkpt("cmp-sp", 1);
    await define("run", "cmp-sp");
    expect(ok(await admin("GET", "/v1/admin/run/cmp-sp")).selfplay.share).toBe(0.25);
    const got: string[] = [];
    for (let i = 0; i < 8; i++) got.push((await lease(await enroll(`sp${i}`))).run);
    expect(got).toEqual(["live3", "live3", "live3", "cmp-sp", "live3", "live3", "live3", "cmp-sp"]);
    err(await admin("PUT", "/v1/admin/run/cmp-sp", { ...body, selfplay: { ...body.selfplay, share: 2 } }), 400, "bad_request");
  });
});
