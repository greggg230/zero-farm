// Rev 9: a run's or campaign's own `min_agent` - its leases go only to agents at or above it; an
// older agent updates first when its channel offers a new enough release, else it gets other work.

import { describe, expect, it } from "vitest";
import { admin, ask, enroll, err, lease, makeBuild, makeCampaign, makeRun, ok, runBody } from "./helpers";

async function release(version: string, min_agent: string, channel = "stable") {
  const id = await makeBuild({ kind: "agent", version });
  ok(await admin("POST", "/v1/admin/agent/release", { version, build_id: id, channel, min_agent }));
}

const BOTH = ["selfplay", "fuzz"];

describe("a run's or campaign's min_agent (rev 9)", () => {
  it("canary-only rollout: self-play goes only to the new canary agent, stable agents keep their fuzz leases", async () => {
    await release("0.2.5", "0.2.5", "stable");
    await release("0.2.7", "0.2.5", "canary");
    await makeRun("bo3", { min_agent: "0.2.8", priority: 5 });
    await makeCampaign("fz");
    const friend = await enroll("friend"); // stable
    const canary = await enroll("canary", { channel: "canary" });

    // Before 0.2.8 is released anywhere: nobody can take the run's leases and nobody is told to update.
    for (const [m, agent] of [[friend, "0.2.5"], [canary, "0.2.7"]] as const) {
      const l = await lease(m, { agent, kinds: BOTH });
      expect([l.kind, l.campaign]).toEqual(["fuzz", "fz"]);
    }

    // 0.2.8 on canary only: the canary agent is told to update, the stable one is not (its channel has no 0.2.8).
    await release("0.2.8", "0.2.5", "canary");
    expect(ok(await ask(canary, { agent: "0.2.7", kinds: BOTH }))).toEqual({ update: { min_agent: "0.2.8" } });
    for (let i = 0; i < 3; i++) {
      const l = await lease(friend, { agent: "0.2.5", kinds: BOTH });
      expect(l.kind).toBe("fuzz");
    }
    // The updated canary agent gets the run's self-play (priority 5 over the campaign's 0.2).
    const sp = await lease(canary, { agent: "0.2.8", kinds: BOTH });
    expect([sp.kind, sp.run]).toEqual(["selfplay", "bo3"]);
    expect(sp.definition.key).toMatch(/^defs\/run\/bo3\//);
    // A newer agent than the limit qualifies too.
    const other = await enroll("canary2", { channel: "canary" });
    expect((await lease(other, { agent: "0.3.0", kinds: ["selfplay"] })).run).toBe("bo3");
    // An agent that offers only self-play and is too old for the run has nothing to do.
    expect(ok(await ask(friend, { agent: "0.2.5", kinds: ["selfplay"] })).reason).toBe("no_work");
  });

  it("a campaign's min_agent: older agents get other work or none, never its seeds", async () => {
    await release("0.2.6", "0.2.5");
    await makeCampaign("new-fz", { min_agent: "0.2.9", priority: 1 });
    await makeCampaign("old-fz", { priority: 0.1 });
    const m = await enroll("pc");
    for (let i = 0; i < 3; i++) expect((await lease(m, { agent: "0.2.6", kinds: ["fuzz"] })).campaign).toBe("old-fz");
    expect((await lease(m, { agent: "0.2.9", kinds: ["fuzz"] })).campaign).toBe("new-fz");
  });

  it("is stored, shown and cleared by the next PUT without it; bad values are refused", async () => {
    const build = await makeRun("r", { min_agent: "0.2.8" });
    expect(ok(await admin("GET", "/v1/admin/run/r")).min_agent).toBe("0.2.8");
    await makeCampaign("c", { min_agent: "0.2.8-rc1" });
    expect(ok(await admin("GET", "/v1/admin/campaign/c")).min_agent).toBe("0.2.8-rc1");
    const stats = ok(await admin("GET", "/v1/admin/stats"));
    expect(stats.runs.find((x: any) => x.run === "r").min_agent).toBe("0.2.8");
    expect(stats.campaigns.find((x: any) => x.campaign === "c").min_agent).toBe("0.2.8-rc1");

    for (const bad of ["0.2", "latest", 28, ""]) {
      err(await admin("PUT", "/v1/admin/run/r", { ...runBody(build), min_agent: bad }), 400, "bad_request");
    }
    expect(ok(await admin("GET", "/v1/admin/run/r")).min_agent).toBe("0.2.8");

    const m = await enroll("pc");
    expect(ok(await ask(m, { agent: "0.2.7", kinds: ["selfplay"] })).reason).toBe("no_work");
    // A PUT is the whole configuration: without min_agent the limit is gone.
    ok(await admin("PUT", "/v1/admin/run/r", runBody(build)));
    expect(ok(await admin("GET", "/v1/admin/run/r")).min_agent).toBeNull();
    expect((await lease(m, { agent: "0.2.7", kinds: ["selfplay"] })).run).toBe("r");
    ok(await admin("PUT", "/v1/admin/run/r", { ...runBody(build), min_agent: null }));
    expect(ok(await admin("GET", "/v1/admin/run/r")).min_agent).toBeNull();
  });

  it("counts with the builds' min_agent: the higher one wins", async () => {
    await release("0.3.0", "0.2.5");
    const w = await makeBuild({ min_agent: "0.2.9", tag: "w029" });
    await makeRun("r", { build: w, min_agent: "0.2.7" });
    const m = await enroll("pc");
    // 0.2.8 meets the run's 0.2.7 but not the build's 0.2.9: update (the channel offers 0.3.0).
    expect(ok(await ask(m, { agent: "0.2.8", kinds: ["selfplay"] }))).toEqual({ update: { min_agent: "0.2.9" } });
    expect((await lease(m, { agent: "0.2.9", kinds: ["selfplay"] })).run).toBe("r");
  });
});
