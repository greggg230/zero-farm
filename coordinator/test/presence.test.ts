// Rev 16 (PROTOCOL 3, POST /v1/presence): an agent that may not work says why; the admin stats
// show it while it is fresh and no newer contact or lease has replaced it.

import { describe, expect, it } from "vitest";
import { admin, advance, ask, call, enroll, err, makeRun, ok } from "./helpers";

const send = (token: string, body: unknown) => call("POST", "/v1/presence", { token, body });
const statsOf = async (id: string) => {
  const s = ok(await admin("GET", "/v1/admin/stats"));
  return { m: s.machines.find((x: any) => x.machine_id === id), c: s.contributors.find((x: any) => x.id === id) };
};

describe("presence", () => {
  it("is stored, keeps the machine online and shows in the stats while fresh", async () => {
    const m = await enroll("pc");
    advance(3600); // enrolment's last_seen is long gone: the machine would be offline
    expect(ok(await send(m.token, { reason: "user_active" }))).toEqual({ ok: true });
    let s = await statsOf(m.machine_id);
    expect(s.m.presence).toBe("user_active");
    expect(s.c).toMatchObject({ status: "online", presence: "user_active" });
    expect((await admin("GET", "/admin")).status).toBe(200);
    advance(600); // stale
    s = await statsOf(m.machine_id);
    expect(s.m.presence).toBeNull();
    expect(s.c.status).toBe("offline");
  });

  it("is replaced by a later contact (a lease request) and by an active lease", async () => {
    await makeRun("r", { games_per_lease: 4 });
    const m = await enroll("pc");
    advance(100);
    ok(await send(m.token, { reason: "fullscreen" }));
    advance(31);
    ok(await ask(m)); // gets a lease; last_seen moves past the presence
    const s = await statsOf(m.machine_id);
    expect(s.m.presence).toBeNull();
    expect(s.c.presence).toBeNull();
    expect(s.c.status).toBe("working");
  });

  it("accepts any reason slug, refuses junk, needs a machine token and is rate limited", async () => {
    const m = await enroll("pc");
    ok(await send(m.token, { reason: "some_future_reason" }));
    err(await send(m.token, { reason: "Not A Slug!" }), 400, "bad_request");
    err(await send(m.token, {}), 400, "bad_request");
    err(await call("POST", "/v1/presence", { body: { reason: "paused" } }), 401, "unauthorized");
    // burst 5: the two refused bodies above took a slot each too
    for (let i = 0; i < 2; i++) ok(await send(m.token, { reason: "paused" }));
    err(await send(m.token, { reason: "paused" }), 429, "rate_limited");
  });
});
