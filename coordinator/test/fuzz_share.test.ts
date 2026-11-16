// A fuzz campaign's share (PROTOCOL 5.4, rev 14: the desktop's fuzz steward while a build's daily fuzz is behind its
// pace): with `share` the campaign goes before a live run's self-play only while it keeps that share of all active
// leases (the runs' and its own), and after every other run job otherwise - it never idles a PC; without `share` it is
// the backfill, as before; the setting's checks; GET and the stats show it.

import { describe, expect, it } from "vitest";
import { jobRank } from "../src/routing";
import { admin, enroll, err, lease, makeBuild, makeCampaign, makeRun, ok } from "./helpers";

const who = (l: any) => l.run ?? l.campaign;

describe("fuzz campaigns with a share (rev 14)", () => {
  it("jobRank: a fuzz share counts its own leases in the total", () => {
    expect(jobRank("fuzz", undefined, 3, 3)).toBe(1); // the backfill's rank, as before
    expect(jobRank("fuzz", 0.25, 0, 0)).toBe(2); // 1 of 1
    expect(jobRank("fuzz", 0.25, 0, 3)).toBe(0); // 1 of 4
    expect(jobRank("fuzz", 0.25, 1, 3)).toBe(2); // 2 of 5
    expect(jobRank("fuzz", 0.25, 1, 6)).toBe(0); // 2 of 8
    expect(jobRank("fuzz", 0.5, 0, 1)).toBe(0); // 1 of 2
    expect(jobRank("fuzz", 0.5, 1, 1)).toBe(2); // 2 of 3
    expect(jobRank("fuzz", 1, 9, 0)).toBe(0);
  });

  it("without share a campaign gets nothing while a live run's self-play has games", async () => {
    await makeRun("live", { quota: 100000, games_per_lease: 10 });
    await makeCampaign("fz-bg");
    for (let i = 0; i < 6; i++) expect(who(await lease(await enroll(`b${i}`)))).toBe("live");
  });

  it("with share 0.25 it takes 1 machine in 4 beside the live run, the rest go to the self-play", async () => {
    await makeRun("live2", { quota: 100000, games_per_lease: 10 });
    await makeCampaign("fz-25", { share: 0.25 });
    const got: string[] = [];
    for (let i = 0; i < 8; i++) got.push(who(await lease(await enroll(`q${i}`))));
    expect(got).toEqual(["live2", "live2", "live2", "fz-25", "live2", "live2", "live2", "fz-25"]);
  });

  it("with share 0.5 it takes every other machine (the steward's catch-up)", async () => {
    await makeRun("live3", { quota: 100000, games_per_lease: 10 });
    await makeCampaign("fz-50", { share: 0.5 });
    const got: string[] = [];
    for (let i = 0; i < 6; i++) got.push(who(await lease(await enroll(`h${i}`))));
    expect(got).toEqual(["live3", "fz-50", "live3", "fz-50", "live3", "fz-50"]);
  });

  it("beyond its share it still gets every machine the runs have nothing for (a quota filled)", async () => {
    await makeRun("full", { quota: 10, slack: 0, games_per_lease: 10 });
    await makeCampaign("fz-small", { share: 0.1 });
    expect(who(await lease(await enroll("f0")))).toBe("full"); // the whole quota in one lease
    for (const n of ["f1", "f2", "f3"]) expect(who(await lease(await enroll(n)))).toBe("fz-small");
  });

  it("a campaign with a share goes before a backfill campaign; evaluation still first", async () => {
    await makeCampaign("fz-plain", { priority: 5 });
    await makeCampaign("fz-share", { share: 0.1, priority: 0.1 });
    // no run at all: the share campaign is in tier 1 (rank 2 beyond its share) - before the tier-2 backfill
    expect(who(await lease(await enroll("p0")))).toBe("fz-share");
  });

  it("checks share, stores it, shows it in GET and the stats, and a put without it removes it", async () => {
    const build = await makeBuild({ tag: "fz-chk" });
    const body = (share: unknown) => ({ build_id: build, mode: "delver", env: {}, seed_base: 1000, chunk: 100, ...(share === undefined ? {} : { share }) });
    for (const bad of [0, -0.1, 1.5, "0.3", true]) err(await admin("PUT", "/v1/admin/campaign/fz-chk", body(bad)), 400, "bad_request");
    ok(await admin("PUT", "/v1/admin/campaign/fz-chk", body(0.3)));
    expect(ok(await admin("GET", "/v1/admin/campaign/fz-chk")).share).toBe(0.3);
    const st = ok(await admin("GET", "/v1/admin/stats"));
    expect(st.campaigns.find((c: any) => c.campaign === "fz-chk").share).toBe(0.3);
    // a PUT is the whole configuration: the cursor stays, the share goes
    ok(await admin("PUT", "/v1/admin/campaign/fz-chk", body(undefined)));
    expect(ok(await admin("GET", "/v1/admin/campaign/fz-chk")).share).toBe(null);
    ok(await admin("PUT", "/v1/admin/campaign/fz-chk", body(null)));
    expect(ok(await admin("GET", "/v1/admin/campaign/fz-chk")).share).toBe(null);
  });
});
