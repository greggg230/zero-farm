// Rev 28 (PROTOCOL 20): fresher rows from fast machines. An extension that says its `gen` is offered a newer generation
// as a beat is; `POST /v1/lease/{id}/poll` asks for one every `poll_s` seconds, which the owner sets per machine
// (`machines.fresh`, migration 0018) with the machine's own freshness budget (`share` x the run's generation period);
// and a rolling lease's result limits are per hour of its life.

import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { ROLLING_LIMIT_S, resultLimitsFor } from "../src/book";
import { FRESH_SHARE_MIN_S, machineFreshOf, shareBudget } from "../src/fresh";
import { type Machine, admin, advance, call, clock, enroll, err, makeRun, ok, publishCkpt, runBody, sha256 } from "./helpers";

const POLL = "0.3.20"; // POLL_AGENT's default
const FRESH = "0.3.17"; // FRESH_AGENT's default: sent budgets, not poll_s
const ROLL = "0.2.13";
const ROLLING = { inflight: 8, gpu_inflight: 64, block: 1000, flush_s: 10 };

// A rolling run whose gens 1..n were registered `gap` seconds apart (the last one now), each signed by a new
// definition version (publishCkpt).
async function pacedRun(run: string, gens = 1, gap = 90, quota = 100_000): Promise<void> {
  const build = await makeRun(run, { max_stale: 4, quota });
  const body: any = runBody(build, { max_stale: 4, quota });
  body.selfplay.rolling = ROLLING;
  ok(await admin("PUT", `/v1/admin/run/${run}`, body));
  for (let g = 2; g <= gens; g++) {
    advance(gap);
    await publishCkpt(run, g);
  }
}

function askRolling(m: Machine, agent: string) {
  return call("POST", "/v1/lease", {
    token: m.token,
    agent,
    body: {
      profile_digest: "ab".repeat(32),
      profile: null,
      caps: { threads: 6, ram_mb: 12000, gpu: false, disk_mb: 2500 },
      cached: { builds: [], ckpts: [] },
      kinds: ["selfplay"],
      features: ["rolling"],
    },
  });
}
async function rollingLease(m: Machine, agent = POLL): Promise<any> {
  const b = ok(await askRolling(m, agent));
  expect(b.lease?.rolling, JSON.stringify(b)).toBeTruthy();
  return b.lease;
}
const extend = (m: Machine, id: string, body: Record<string, unknown>, agent = POLL) => call("POST", `/v1/lease/${id}/extend`, { token: m.token, agent, body });
const poll = (m: Machine, id: string, body: unknown, agent = POLL) => call("POST", `/v1/lease/${id}/poll`, { token: m.token, agent, body });
const beat = (m: Machine, id: string, gen: number, agent = POLL) =>
  call("POST", `/v1/lease/${id}/beat`, { token: m.token, agent, body: { state: "running", progress: { games_done: 0, games_total: 1 }, gen } });
const setFresh = (m: Machine, fresh: unknown) => admin("POST", `/v1/admin/machine/${m.machine_id}`, { fresh });
const row = (id: string) => env.DB.prepare(`SELECT * FROM leases WHERE lease_id = ?`).bind(id).first<any>();
const played = (id: string) =>
  env.DB.prepare(`UPDATE leases SET progress = json_object('games_done', rel_hi - lo, 'games_total', rel_hi - lo) WHERE lease_id = ?`).bind(id).run();

describe("an extension that says its gen is offered a newer one (20.1)", () => {
  it("offers checkpoint + definition while the run's gen is newer, stores cur_gen; without gen the answer is as before", async () => {
    await pacedRun("x");
    const m = await enroll("pc");
    const l = await rollingLease(m);
    await played(l.lease_id);
    // nothing newer: checkpoint null
    expect(ok(await extend(m, l.lease_id, { games: 8, gen: 1 }))).toEqual({ games: 72, checkpoint: null, fresh_s: null, poll_s: null });
    await publishCkpt("x", 2);
    await played(l.lease_id);
    const a = ok(await extend(m, l.lease_id, { games: 8, gen: 1 }));
    expect(a.games).toBe(80);
    expect(a.checkpoint.gen).toBe(2);
    expect(a.definition.key).toMatch(/defs\/run\/x\//);
    expect((await row(l.lease_id)).cur_gen).toBe(1);
    // an extension without gen (an agent before 0.3.20): no offer, the keys of before
    await played(l.lease_id);
    expect(Object.keys(ok(await extend(m, l.lease_id, { games: 8 }, ROLL))).sort()).toEqual(["games"]);
    // the worker switched: the next extension says so, nothing is offered, cur_gen follows
    await played(l.lease_id);
    expect(ok(await extend(m, l.lease_id, { games: 8, gen: 2 })).checkpoint).toBe(null);
    expect((await row(l.lease_id)).cur_gen).toBe(2);
    // a gen older than the lease's first is not stored
    await played(l.lease_id);
    ok(await extend(m, l.lease_id, { games: 8, gen: 0 }));
    expect((await row(l.lease_id)).cur_gen).toBe(2);
    err(await extend(m, l.lease_id, { games: 8, gen: "two" }), 400, "bad_request");
  });

  it("finish answers carry the offer too (a draining lease keeps switching)", async () => {
    await pacedRun("x");
    const m = await enroll("pc");
    const l = await rollingLease(m);
    await publishCkpt("x", 2);
    ok(await admin("POST", "/v1/admin/pause", { paused: true }));
    const a = ok(await extend(m, l.lease_id, { games: 8, gen: 1 }));
    expect(a).toMatchObject({ finish: true, reason: "state" });
    expect(a.checkpoint.gen).toBe(2);
  });
});

describe("POST /v1/lease/{id}/poll (20.2)", () => {
  it("answers the offer and changes nothing; 400 / 404 / 410 as an extension", async () => {
    await pacedRun("x");
    const m = await enroll("pc");
    const l = await rollingLease(m);
    const before = await row(l.lease_id);
    expect(ok(await poll(m, l.lease_id, { gen: 1 }))).toEqual({ checkpoint: null, fresh_s: null, poll_s: null });
    await publishCkpt("x", 2);
    const a = ok(await poll(m, l.lease_id, { gen: 1 }));
    expect(a.checkpoint.gen).toBe(2);
    expect(a.definition.sha256).toMatch(/^[0-9a-f]{64}$/);
    expect(ok(await poll(m, l.lease_id, { gen: 2 })).checkpoint).toBe(null);
    const after = await row(l.lease_id);
    expect([after.rel_hi, after.until, after.cur_gen, after.pending]).toEqual([before.rel_hi, before.until, before.cur_gen, before.pending]);
    err(await poll(m, l.lease_id, {}), 400, "bad_request");
    const other = await enroll("other");
    err(await poll(other, l.lease_id, { gen: 1 }), 404, "not_found");
    ok(await call("POST", `/v1/lease/${l.lease_id}/done`, { token: m.token, agent: POLL, body: { results: [], stats: {}, given_back: false, unfinished: null } }));
    err(await poll(m, l.lease_id, { gen: 1 }), 410, "gone");
  });

  it("a classic lease is not polled", async () => {
    await makeRun("c", { quota: 1000 });
    const m = await enroll("pc");
    const b = ok(await call("POST", "/v1/lease", {
      token: m.token,
      agent: POLL,
      body: { profile_digest: "ab".repeat(32), profile: null, caps: { threads: 6, ram_mb: 12000, gpu: false, disk_mb: 2500 }, cached: { builds: [], ckpts: [] }, kinds: ["selfplay"] },
    }));
    err(await poll(m, b.lease.lease_id, { gen: 1 }), 400, "bad_request");
  });

  it("has its own bucket: polls every 2 s leave the beat bucket alone", async () => {
    await pacedRun("x");
    const m = await enroll("pc");
    const l = await rollingLease(m);
    for (let i = 0; i < 10; i++) ok(await poll(m, l.lease_id, { gen: 1 })); // the burst
    err(await poll(m, l.lease_id, { gen: 1 }), 429, "rate_limited");
    ok(await beat(m, l.lease_id, 1)); // the beat bucket is untouched
    advance(2);
    ok(await poll(m, l.lease_id, { gen: 1 }));
  });
});

describe("the owner's freshness settings per machine (20.2, 20.3)", () => {
  it("POST /v1/admin/machine fresh: validated, replaced whole, null clears; machines and stats show it", async () => {
    const m = await enroll("pc");
    err(await setFresh(m, { poll_s: 1 }), 400, "bad_request");
    err(await setFresh(m, { poll_s: 301 }), 400, "bad_request");
    err(await setFresh(m, { share: 0.05 }), 400, "bad_request");
    err(await setFresh(m, { share: 5 }), 400, "bad_request");
    err(await setFresh(m, { poll: 5 }), 400, "bad_request");
    err(await setFresh(m, [5]), 400, "bad_request");
    expect(ok(await setFresh(m, { poll_s: 5, share: 0.75 }))).toEqual({ ok: true, fresh: { poll_s: 5, share: 0.75 } });
    const mine = (x: any) => x.machines.find((y: any) => y.machine_id === m.machine_id);
    expect(mine(ok(await admin("GET", "/v1/admin/machines"))).fresh).toEqual({ poll_s: 5, share: 0.75 });
    expect(mine(ok(await admin("GET", "/v1/admin/stats"))).fresh).toEqual({ poll_s: 5, share: 0.75 });
    expect(ok(await setFresh(m, { share: 0.5 }))).toEqual({ ok: true, fresh: { share: 0.5 } });
    expect(ok(await setFresh(m, null))).toEqual({ ok: true, fresh: {} });
    expect(mine(ok(await admin("GET", "/v1/admin/machines"))).fresh).toEqual({});
    expect((await env.DB.prepare(`SELECT fresh FROM machines WHERE machine_id = ?`).bind(m.machine_id).first<any>()).fresh).toBe(null);
    expect(ok(await admin("GET", "/v1/admin/stats")).limits.poll_agent).toBe(POLL);
  });

  it("stored values are read defensively", () => {
    expect(machineFreshOf(null)).toEqual({});
    expect(machineFreshOf("not json")).toEqual({});
    expect(machineFreshOf(JSON.stringify({ poll_s: 1, share: 9 }))).toEqual({});
    expect(machineFreshOf(JSON.stringify({ poll_s: 5.5, share: 0.7 }))).toEqual({ share: 0.7 });
    expect(machineFreshOf(JSON.stringify({ poll_s: 5 }))).toEqual({ poll_s: 5 });
  });

  it("poll_s rides on the lease and every answer to agents from POLL_AGENT; null once unset; older agents never see it", async () => {
    await pacedRun("x");
    const m = await enroll("pc");
    ok(await setFresh(m, { poll_s: 5 }));
    const l = await rollingLease(m);
    expect(l.rolling.poll_s).toBe(5);
    expect(ok(await beat(m, l.lease_id, 1)).poll_s).toBe(5);
    expect(ok(await extend(m, l.lease_id, { games: 4, gen: 1 })).poll_s).toBe(5);
    expect(ok(await poll(m, l.lease_id, { gen: 1 })).poll_s).toBe(5);
    ok(await setFresh(m, null));
    expect(ok(await beat(m, l.lease_id, 1)).poll_s).toBe(null);
    expect(ok(await poll(m, l.lease_id, { gen: 1 })).poll_s).toBe(null);
    // an agent before POLL_AGENT: no poll_s anywhere
    const old = await enroll("old");
    ok(await setFresh(old, { poll_s: 5 }));
    const lo = await rollingLease(old, FRESH);
    expect("poll_s" in lo.rolling).toBe(false);
    expect("poll_s" in ok(await beat(old, lo.lease_id, 1, FRESH))).toBe(false);
    expect("poll_s" in ok(await extend(old, lo.lease_id, { games: 4 }, FRESH))).toBe(false);
  });

  it("share: the machine's budget is min(the run's, share x period), on any channel; fresh_cap off is the kill switch", async () => {
    await pacedRun("x", 9, 90); // period 90 s: the run's budget 0.75 x 5 x 90 - 10 - 60 = 267
    const desk = await enroll("desk"); // a stable machine: fresh_cap "canary" leaves it out...
    const plain = await enroll("plain");
    ok(await setFresh(desk, { share: 0.75 })); // ...but its own share counts
    const l = await rollingLease(desk, FRESH);
    expect(l.rolling.fresh_s).toBe(67); // floor(0.75 x 90)
    expect(ok(await beat(desk, l.lease_id, 9, FRESH)).fresh_s).toBe(67);
    expect(ok(await extend(desk, l.lease_id, { games: 4 }, FRESH)).fresh_s).toBe(67);
    // another stable machine: none, as before
    const lp = await rollingLease(plain, FRESH);
    expect("fresh_s" in lp.rolling).toBe(false);
    // a share over the run's budget gives the run's
    ok(await setFresh(desk, { share: 4 }));
    expect(ok(await beat(desk, l.lease_id, 9, FRESH)).fresh_s).toBe(267);
    // off: nobody
    ok(await setFresh(desk, { share: 0.75 }));
    ok(await admin("POST", "/v1/admin/routing", { fresh_cap: "off" }));
    expect(ok(await beat(desk, l.lease_id, 9, FRESH)).fresh_s).toBe(null);
  });

  it("shareBudget: share x period, at least FRESH_SHARE_MIN_S; none without history", () => {
    const steady = (n: number, gap: number, now = 10_000) => Array.from({ length: n }, (_, i) => now - i * gap);
    expect(shareBudget(steady(9, 93), 0.75, 10_000)).toBe(69);
    expect(shareBudget(steady(9, 20), 0.5, 10_000)).toBe(FRESH_SHARE_MIN_S);
    expect(shareBudget(steady(3, 93), 0.75, 10_000)).toBe(null);
  });
});

describe("a rolling lease's result limits are per hour (20.4)", () => {
  const lim = { chunks: 2000, bytes: 1024, late_s: 86400 };
  it("classic: per lease; rolling: per started hour of its life (to its close)", () => {
    expect(resultLimitsFor({ rel_hi: null, issued_at: 0, closed_at: null }, lim, 10 * ROLLING_LIMIT_S)).toEqual(lim);
    expect(resultLimitsFor({ rel_hi: 10, issued_at: 0, closed_at: null }, lim, 0).chunks).toBe(2000);
    expect(resultLimitsFor({ rel_hi: 10, issued_at: 0, closed_at: null }, lim, ROLLING_LIMIT_S).chunks).toBe(2000);
    expect(resultLimitsFor({ rel_hi: 10, issued_at: 0, closed_at: null }, lim, ROLLING_LIMIT_S + 1)).toMatchObject({ chunks: 4000, bytes: 2048 });
    expect(resultLimitsFor({ rel_hi: 10, issued_at: 0, closed_at: 2 * ROLLING_LIMIT_S }, lim, 30 * ROLLING_LIMIT_S).chunks).toBe(4000);
  });

  it("a rolling lease past its first hour takes more chunks than MAX_LEASE_CHUNKS", async () => {
    await pacedRun("x");
    const m = await enroll("pc");
    const l = await rollingLease(m);
    const caps = { MAX_LEASE_CHUNKS: "2" };
    const put = async (seq: number, envv = caps) => {
      const body = `chunk ${seq}`;
      return call("PUT", `/v1/lease/${l.lease_id}/result/${seq}`, {
        token: m.token,
        agent: POLL,
        raw: body,
        headers: { "x-sha256": await sha256(body), "x-ids": JSON.stringify([[seq, seq + 1]]), "x-gen": "1" },
        env: envv,
      });
    };
    ok(await put(0));
    ok(await put(1));
    expect(err(await put(2), 413, "too_large")).toMatchObject({ limit: "chunks" });
    // an hour on (beating meanwhile), it may have 4
    for (let i = 0; i < 61; i++) {
      advance(60);
      ok(await beat(m, l.lease_id, 1));
    }
    expect(clock.now - l.issued_at).toBeGreaterThan(ROLLING_LIMIT_S);
    ok(await put(2));
    ok(await put(3));
    err(await put(4), 413, "too_large");
  });
});
