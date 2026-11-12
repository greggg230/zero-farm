// Per-machine rate limits (PROTOCOL 1): a buggy agent gets 429 + retry-after after a small burst;
// a well-behaved one is never limited.

import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { runCron } from "../src/cron";
import { LIMITS } from "../src/ratelimit";
import { admin, advance, ask, beat, call, clock, DIGEST, enroll, err, finish, lease, makeCampaign, ok, PROFILE } from "./helpers";

function limited(r: { status: number; body: any; headers: Headers }, retry: number) {
  err(r, 429, "rate_limited");
  expect(r.body.retry_after).toBe(retry);
  expect(r.headers.get("retry-after")).toBe(String(retry));
}

describe("rate limits", () => {
  it("a machine hammering /v1/lease gets 429 with retry-after after its burst", async () => {
    const m = await enroll("buggy");
    for (let i = 0; i < LIMITS.lease.burst; i++) ok(await ask(m));
    limited(await ask(m), 5);
    advance(2);
    limited(await ask(m), 3);
    advance(3);
    ok(await ask(m)); // one more slot, 5 s after the burst
    limited(await ask(m), 5);
  });

  it("the 2026-11-11 hot loop (8 lease requests a second) gets one answer per 5 s", async () => {
    const m = await enroll("canary-pc");
    let answered = 0;
    let refused = 0;
    const SECONDS = 180; // 3 of the 9 minutes it ran
    for (let s = 0; s < SECONDS; s++) {
      for (let i = 0; i < 8; i++) {
        const r = await ask(m);
        if (r.status === 429) refused++;
        else answered++;
      }
      advance(1);
    }
    // The burst, then one per interval; refusals change nothing, so it is never locked out.
    expect(answered).toBe(LIMITS.lease.burst + SECONDS / LIMITS.lease.interval - 1);
    expect(refused).toBe(SECONDS * 8 - answered);
    advance(LIMITS.lease.interval);
    ok(await ask(m));
  }, 600_000);

  it("a well-behaved agent is never limited", async () => {
    await makeCampaign("fz", { chunk: 1000 });
    const m = await enroll("good");
    // A day of: lease ~450 s with beats every 60 s, done, ask again at once (with a 409
    // need_profile retry when the profile changed now and then).
    let digest = DIGEST;
    for (let round = 0; round < 60; round++) {
      let l;
      if (round % 10 === 5) {
        digest = round.toString(16).padStart(2, "0").repeat(32);
        err(await ask(m, { digest }), 409, "conflict");
        l = await lease(m, { digest, profile: PROFILE });
      } else l = await lease(m, { digest });
      for (let b = 0; b < 7; b++) {
        advance(60);
        ok(await beat(m, l.lease_id));
      }
      advance(30);
      ok(await finish(m, l.lease_id, [], { given_back: true }));
    }
    // No work: one request per wait_s for hours.
    await makeCampaign("fz", { state: "paused" });
    for (let i = 0; i < 50; i++) {
      const b = ok(await ask(m, { digest }));
      expect(b.reason).toBe("no_work");
      advance(b.wait_s);
    }
    // Back-to-back requests every 5 s are still fine: exactly the limit's rate.
    for (let i = 0; i < 100; i++) {
      ok(await ask(m, { digest }));
      advance(LIMITS.lease.interval);
    }
  }, 300_000);

  it("limits are per machine", async () => {
    const a = await enroll("a");
    const b = await enroll("b");
    for (let i = 0; i < LIMITS.lease.burst; i++) ok(await ask(a));
    limited(await ask(a), 5);
    ok(await ask(b));
  });

  it("beats and crash reports have their own limits", async () => {
    await makeCampaign("fz");
    const m = await enroll("pc");
    const l = await lease(m);
    for (let i = 0; i < LIMITS.beat.burst; i++) ok(await beat(m, l.lease_id));
    limited(await beat(m, l.lease_id), LIMITS.beat.interval);
    // The lease is untouched by refused beats; lease requests are counted separately.
    ok(await ask(m));
    for (let i = 0; i < LIMITS.crash.burst; i++) ok(await call("POST", "/v1/crash", { token: m.token, body: { kind: "crash", message: `#${i}` } }));
    limited(await call("POST", "/v1/crash", { token: m.token, body: { kind: "crash", message: "one too many" } }), LIMITS.crash.interval);
    expect((await env.DB.prepare(`SELECT COUNT(*) AS n FROM crashes`).first<any>()).n).toBe(LIMITS.crash.burst);
  });

  it("enroll is limited per client IP, without using up the invite", async () => {
    const { codes } = ok(await admin("POST", "/v1/admin/invites", { count: LIMITS.enroll.burst + 2 }));
    const from = (ip: string, code: string) =>
      call("POST", "/v1/enroll", { headers: { "cf-connecting-ip": ip }, body: { invite: code, name: "pc", profile: PROFILE } });
    for (let i = 0; i < LIMITS.enroll.burst; i++) ok(await from("203.0.113.9", codes[i]));
    const last = codes[LIMITS.enroll.burst];
    limited(await from("203.0.113.9", last), LIMITS.enroll.interval);
    ok(await from("198.51.100.4", last)); // another address; the code was still unused
    // Wrong codes count too (no guessing).
    advance(LIMITS.enroll.interval);
    err(await from("203.0.113.9", "AAAA-BBBB-CCCC"), 403, "unauthorized");
    limited(await from("203.0.113.9", codes[LIMITS.enroll.burst + 1]), LIMITS.enroll.interval);
  });

  it("the cron drops rows that carry no state", async () => {
    const m = await enroll("pc");
    ok(await ask(m));
    expect((await env.DB.prepare(`SELECT COUNT(*) AS n FROM rate_limits`).first<any>()).n).toBe(1);
    await runCron(env, clock.now + 3600);
    expect((await env.DB.prepare(`SELECT COUNT(*) AS n FROM rate_limits`).first<any>()).n).toBe(1);
    await runCron(env, clock.now + 2 * 86400);
    expect((await env.DB.prepare(`SELECT COUNT(*) AS n FROM rate_limits`).first<any>()).n).toBe(0);
  });
});
