// Worker builds Windows refused to start (PROTOCOL 3, `/v1/crash` kind "blocked", rev 5): found on
// a real PC with Smart App Control on 2026-11-12, where agent 0.2.1 leased the same build 521
// times in 45 minutes. The coordinator stops offering that build to that machine for a day, or
// until a newer agent asks; a newer build is offered at once; the admin page shows the block.

import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { admin, advance, ask, call, enroll, err, lease, makeBuild, makeCampaign, ok, type Machine } from "./helpers";

function report(m: Machine, body: Record<string, unknown>, agent?: string) {
  return call("POST", "/v1/crash", { token: m.token, agent, body });
}

describe("blocked worker builds (rev 5)", () => {
  it("a blocked report stops that build going to that machine; others still get it", async () => {
    const w = await makeBuild({ tag: "fz" });
    await makeCampaign("fz", { build: w });
    const [a, b] = [await enroll("a"), await enroll("b")];
    const l = await lease(a);
    err(await report(a, { kind: "blocked", message: "no build id" }), 400, "bad_request");
    ok(
      await report(a, {
        kind: "blocked",
        build_id: w,
        lease_id: l.lease_id,
        os_error: 4551,
        message: "could not start worker: An Application Control policy has blocked this file. (os error 4551)",
      }),
    );
    expect(ok(await ask(a)).reason).toBe("no_work");
    expect((await lease(b)).build.build_id).toBe(w);

    const crash = await env.DB.prepare(`SELECT kind, os_error, build_id FROM crashes WHERE machine_id = ?`).bind(a.machine_id).first<any>();
    expect(crash).toEqual({ kind: "blocked", os_error: 4551, build_id: w });
    const s = ok(await admin("GET", "/v1/admin/stats"));
    const ma = s.machines.find((x: any) => x.machine_id === a.machine_id);
    expect(ma.blocked).toEqual([expect.objectContaining({ build_id: w, os_error: 4551, agent_version: "0.3.1" })]);
    expect(ma.blocked[0].until - ma.blocked[0].t).toBe(86400);
    expect(s.machines.find((x: any) => x.machine_id === b.machine_id).blocked).toEqual([]);
    expect((await admin("GET", "/admin")).body).toContain("blocked builds");
  });

  it("is lifted after a day, by a newer agent, by a newer build, or by the admin", async () => {
    const w = await makeBuild({ tag: "fz" });
    await makeCampaign("fz", { build: w });
    const m = await enroll("a");
    const block = async () => ok(await report(m, { kind: "blocked", build_id: w, os_error: 1260, message: "AppLocker" }, "0.3.1"));

    await block();
    expect(ok(await ask(m)).reason).toBe("no_work");
    advance(86400);
    expect((await lease(m)).build.build_id).toBe(w); // a day later

    await block();
    expect(ok(await ask(m, { agent: "0.3.1" })).reason).toBe("no_work");
    expect((await lease(m, { agent: "0.3.2" })).build.build_id).toBe(w); // a newer agent may do better
    expect((await env.DB.prepare(`SELECT COUNT(*) AS n FROM blocks`).first<any>()).n).toBe(0);

    await block();
    advance(10);
    ok(await admin("POST", `/v1/admin/machine/${m.machine_id}`, { unblock: true }));
    expect((await lease(m)).build.build_id).toBe(w);

    await block();
    advance(10);
    const w2 = await makeBuild({ tag: "fz-newer" });
    await makeCampaign("fz", { build: w2 });
    expect((await lease(m)).build.build_id).toBe(w2); // another build_id: offered at once
  });

  it("other crash kinds don't block anything", async () => {
    const w = await makeBuild({ tag: "fz" });
    await makeCampaign("fz", { build: w });
    const m = await enroll("a");
    for (const kind of ["panic", "crash", "agent"]) ok(await report(m, { kind, build_id: w, message: kind, os_error: 2 }));
    err(await report(m, { kind: "weird", build_id: w }), 400, "bad_request");
    expect((await lease(m)).build.build_id).toBe(w);
  });
});
