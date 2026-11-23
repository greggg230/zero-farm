// Rev 28 (PROTOCOL 20.7): the owner's disk cap per machine (`machines.disk_cap_mb`, migration 0018), set with
// POST /v1/admin/machine/{id} `disk_cap_mb` and sent in every /v1/lease answer to agents from DISK_CAP_AGENT, which use
// it in place of their own `disk_cap_mb` while it is set.

import { describe, expect, it } from "vitest";
import { type Machine, admin, call, enroll, err, makeRun, ok } from "./helpers";

const CAP = "0.3.20"; // DISK_CAP_AGENT's default
const OLD = "0.3.19";

function ask(m: Machine, agent: string, diskMb = 2500) {
  return call("POST", "/v1/lease", {
    token: m.token,
    agent,
    body: {
      profile_digest: "ab".repeat(32),
      profile: null,
      caps: { threads: 6, ram_mb: 12000, gpu: false, disk_mb: diskMb },
      cached: { builds: [], ckpts: [] },
      kinds: ["selfplay"],
    },
  });
}
const setCap = (m: Machine, v: unknown) => admin("POST", `/v1/admin/machine/${m.machine_id}`, { disk_cap_mb: v });

describe("the owner's disk cap per machine (20.7)", () => {
  it("is set, shown and cleared by the owner; values outside 1024..16777216 MB are refused", async () => {
    const m = await enroll("pc");
    expect(ok(await setCap(m, 12288))).toEqual({ ok: true, disk_cap_mb: 12288 });
    const list = ok(await admin("GET", "/v1/admin/machines")).machines;
    expect(list.find((x: any) => x.machine_id === m.machine_id).disk_cap_mb).toBe(12288);
    const stats = ok(await admin("GET", "/v1/admin/stats"));
    expect(stats.machines.find((x: any) => x.machine_id === m.machine_id).disk_cap_mb).toBe(12288);
    expect(stats.limits.disk_cap_agent).toBe(CAP);
    // other settings leave it alone
    ok(await admin("POST", `/v1/admin/machine/${m.machine_id}`, { name: "renamed" }));
    expect(ok(await admin("GET", "/v1/admin/machines")).machines.find((x: any) => x.machine_id === m.machine_id).disk_cap_mb).toBe(12288);
    for (const v of [500, 1 << 25, 2048.5, "12288", true]) err(await setCap(m, v), 400, "bad_request");
    expect(ok(await setCap(m, null))).toEqual({ ok: true, disk_cap_mb: null });
    expect(ok(await admin("GET", "/v1/admin/machines")).machines.find((x: any) => x.machine_id === m.machine_id).disk_cap_mb).toBe(null);
    err(await admin("POST", "/v1/admin/machine/m_nosuchmachine000000000000", { disk_cap_mb: 4096 }), 404, "not_found");
  });

  it("rides on every lease answer to agents from DISK_CAP_AGENT (null when unset); older agents never see it", async () => {
    const m = await enroll("pc");
    // no work: wait no_work
    let a = ok(await ask(m, CAP));
    expect(a.reason).toBe("no_work");
    expect(a.owner_disk_cap_mb).toBe(null);
    ok(await setCap(m, 12288));
    a = ok(await ask(m, CAP));
    expect([a.reason, a.owner_disk_cap_mb]).toEqual(["no_work", 12288]);
    // paused by the owner: still told
    ok(await admin("POST", `/v1/admin/machine/${m.machine_id}`, { paused: true }));
    a = ok(await ask(m, CAP));
    expect([a.reason, a.owner_disk_cap_mb]).toEqual(["paused_by_admin", 12288]);
    ok(await admin("POST", `/v1/admin/machine/${m.machine_id}`, { paused: false }));
    // a lease: with it too
    await makeRun("x");
    a = ok(await ask(m, CAP));
    expect(a.lease?.kind, JSON.stringify(a)).toBe("selfplay");
    expect(a.owner_disk_cap_mb).toBe(12288);
    // an agent before DISK_CAP_AGENT: never the key
    const old = await enroll("old");
    ok(await setCap(old, 12288));
    const b = ok(await ask(old, OLD));
    expect("owner_disk_cap_mb" in b, JSON.stringify(b)).toBe(false);
    // cleared: null again
    ok(await setCap(m, null));
    expect(ok(await ask(m, CAP)).owner_disk_cap_mb).toBe(null);
  });

  it("changes nothing in the fit: the disk the agent reports (caps.disk_mb) still decides", async () => {
    await makeRun("x");
    const m = await enroll("pc");
    ok(await setCap(m, 12288));
    // an agent that still reports 0 MB free under its cap gets no lease whatever the owner's cap says
    const a = ok(await ask(m, CAP, 0));
    expect([a.reason, a.owner_disk_cap_mb]).toEqual(["no_work", 12288]);
  });
});
