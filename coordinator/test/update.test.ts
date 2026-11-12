// update_required: agents older than any min_agent are told to update first.

import { describe, expect, it } from "vitest";
import { admin, ask, call, enroll, err, lease, makeBuild, makeCampaign, makeRun, ok, PROFILE } from "./helpers";

async function release(version: string, min_agent: string, channel = "stable") {
  const id = await makeBuild({ kind: "agent", version });
  ok(await admin("POST", "/v1/admin/agent/release", { version, build_id: id, channel, min_agent }));
}

describe("update_required", () => {
  it("an agent older than its channel's min_agent gets {update} instead of work", async () => {
    await makeCampaign("fz");
    const m = await enroll("pc");
    await release("0.4.0", "0.4.0");
    expect(ok(await ask(m, { agent: "0.3.1" }))).toEqual({ update: { min_agent: "0.4.0" } });
    expect(ok(await ask(m, { agent: "0.4.0" })).lease.kind).toBe("fuzz");
    expect(ok(await ask(m, { agent: "0.5.0-rc1" })).lease.kind).toBe("fuzz");
    // The canary channel has its own min_agent.
    const c = await enroll("mine", { channel: "canary" });
    await release("0.6.0", "0.6.0", "canary");
    expect(ok(await ask(c, { agent: "0.5.0" }))).toEqual({ update: { min_agent: "0.6.0" } });
    expect(ok(await ask(m, { agent: "0.5.0" })).lease).toBeTruthy();
  });

  it("a worker build that needs a newer agent makes the agent update first, if the channel has one", async () => {
    await release("0.5.0", "0.3.0");
    const w = await makeBuild({ min_agent: "0.5.0", tag: "new" });
    await makeRun("r", { build: w });
    const m = await enroll("pc");
    expect(ok(await ask(m, { agent: "0.4.0" }))).toEqual({ update: { min_agent: "0.5.0" } });
    expect((await lease(m, { agent: "0.5.0" })).run).toBe("r");
  });

  it("a build needing an agent the channel doesn't have yet is skipped, not an update loop", async () => {
    await release("0.4.0", "0.3.0");
    const w = await makeBuild({ min_agent: "0.9.0", tag: "future" });
    await makeRun("r", { build: w });
    await makeCampaign("fz");
    const m = await enroll("pc");
    expect(ok(await ask(m, { agent: "0.4.0" })).lease.kind).toBe("fuzz");
  });

  it("a worker speaking another agent-worker protocol is skipped", async () => {
    const w = await makeBuild({ aw: 2, tag: "aw2" });
    await makeRun("r", { build: w });
    const m = await enroll("pc");
    expect(ok(await ask(m)).reason).toBe("no_work");
    const b = ok(await call("POST", "/v1/lease", {
      token: m.token,
      headers: { "x-farm-aw": "2" },
      body: { profile_digest: "ab".repeat(32), caps: { threads: 4, ram_mb: 8000, gpu: false, disk_mb: 2000 }, kinds: ["selfplay"] },
    }));
    expect(b.lease.run).toBe("r");
  });

  it("enrolling with an agent older than the channel's min_agent -> 426 update_required", async () => {
    await release("0.4.0", "0.4.0");
    const { codes } = ok(await admin("POST", "/v1/admin/invites", {}));
    const body = { invite: codes[0], name: "pc", profile: PROFILE };
    expect(err(await call("POST", "/v1/enroll", { body, agent: "0.3.9" }), 426, "update_required").min_agent).toBe("0.4.0");
    ok(await call("POST", "/v1/enroll", { body, agent: "0.4.0" })); // the invite wasn't used up
  });
});
