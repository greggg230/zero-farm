// Hardening from the 2026-11-12 security review (H4, M3, M4, L8): result chunks and bytes per
// lease, no results long after a lease closed, active leases per machine, rates from credited ids
// only, fuzz failures inside the lease, crash rows per machine per day, retention of crashes and
// diagnostics, client IPs stored only as an HMAC and only for minutes, the coordinator's own
// header on every /v1/* answer, installers only from this repository's agent-v<version> releases.

import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { runCron } from "../src/cron";
import { ipKey, ipPrefix } from "../src/ratelimit";
import {
  LEGACY,
  PROFILE,
  admin,
  advance,
  ask,
  call,
  clock,
  enroll,
  err,
  finish,
  lease,
  makeBuild,
  makeCampaign,
  makeRun,
  ok,
  sha256,
} from "./helpers";

const put = async (m: { token: string }, leaseId: string, seq: number, body: string, o: { ids?: [number, number][]; env?: Record<string, string> } = {}) =>
  call("PUT", `/v1/lease/${leaseId}/result/${seq}`, {
    token: m.token,
    raw: body,
    headers: { "x-sha256": await sha256(body), ...(o.ids ? { "x-ids": JSON.stringify(o.ids) } : {}) },
    env: o.env,
  });
const objects = async (prefix: string) => (await env.BUCKET.list({ prefix })).objects.map((o) => o.key);

describe("result uploads per lease (M3)", () => {
  it("at most MAX_LEASE_CHUNKS chunks and MAX_LEASE_BYTES bytes; a refused chunk leaves nothing behind", async () => {
    await makeRun("r", { games_per_lease: 10 });
    const m = await enroll("pc");
    const l = await lease(m);
    const caps = { MAX_LEASE_CHUNKS: "3", MAX_LEASE_BYTES: "100" };
    for (let i = 0; i < 3; i++) ok(await put(m, l.lease_id, i, `chunk ${i}`, { ids: [[i, i + 1]], env: caps }));
    ok(await put(m, l.lease_id, 2, "chunk 2", { ids: [[2, 3]], env: caps })); // a retry is no new chunk
    const over = err(await put(m, l.lease_id, 3, "chunk 3", { ids: [[3, 4]], env: caps }), 413, "too_large");
    expect(over).toMatchObject({ limit: "chunks" });
    // Bytes: refused from the declared length before the body is read, and by LeaseBook after.
    const l2 = await lease(m);
    ok(await put(m, l2.lease_id, 0, "x".repeat(60), { env: caps }));
    expect(err(await put(m, l2.lease_id, 1, "y".repeat(60), { env: caps }), 413, "too_large")).toMatchObject({ limit: "bytes" });
    expect((await objects(`results/r/${l2.lease_id}/`)).length).toBe(1);
    ok(await put(m, l2.lease_id, 1, "y".repeat(40), { env: caps }));
    // The defaults: 2,000 chunks, 1 GiB.
    expect(ok(await admin("GET", "/v1/admin/stats")).limits).toMatchObject({ max_lease_chunks: 2000, max_lease_bytes: 1024 ** 3, late_results_s: 86400 });
  });

  it("results are taken until LATE_RESULTS_S (24 h) after the lease closed", async () => {
    await makeRun("r", { games_per_lease: 10 });
    const m = await enroll("pc");
    const l = await lease(m);
    ok(await put(m, l.lease_id, 0, "first", { ids: [[0, 2]] }));
    ok(await finish(m, l.lease_id, [0]));
    advance(86400);
    ok(await put(m, l.lease_id, 1, "late but in time", { ids: [[2, 4]] }));
    advance(1);
    expect(err(await put(m, l.lease_id, 2, "too late", { ids: [[4, 6]] }), 410, "gone").message).toMatch(/86400 s after a lease ends/);
    ok(await put(m, l.lease_id, 1, "late but in time", { ids: [[2, 4]] })); // a retry of a stored chunk still answers
  });

  it("a machine's rate counts only ids credited to it (M4)", async () => {
    await makeRun("r", { games_per_lease: 10 });
    const m = await enroll("pc");
    const l = await lease(m);
    ok(await put(m, l.lease_id, 0, "a", { ids: [[0, 5]] }));
    expect(ok(await put(m, l.lease_id, 1, "a again", { ids: [[0, 5]] })).credited).toBe(0);
    ok(await put(m, l.lease_id, 2, "b", { ids: [[5, 10]] }));
    const row = await env.DB.prepare(`SELECT units_done FROM leases WHERE lease_id = ?`).bind(l.lease_id).first<any>();
    expect(row.units_done).toBe(10);
  });

  it("fuzz failures must name a seed of the lease", async () => {
    await makeCampaign("fz", { seed_base: 1000, chunk: 100 });
    const m = await enroll("pc");
    const l = await lease(m);
    const res = (seed: number) => JSON.stringify({ campaign: "fz", first: 1000, count: 100, done: null, failures: [{ seed, site: "a.rs:1:1" }] });
    expect(err(await put(m, l.lease_id, 0, res(999)), 400, "bad_request").message).toMatch(/outside this lease's seeds/);
    err(await put(m, l.lease_id, 0, res(1100)), 400, "bad_request");
    ok(await put(m, l.lease_id, 0, res(1099)));
  });
});

describe("leases per machine (M4)", () => {
  it("at most MAX_ACTIVE_LEASES active leases (default 2); expired or finished ones don't count", async () => {
    await makeCampaign("fz", { chunk: 100 });
    const m = await enroll("pc");
    const cap = { MAX_ACTIVE_LEASES: "2" };
    const a = await lease(m, { env: cap });
    advance(5);
    await lease(m, { env: cap });
    advance(5);
    expect(ok(await ask(m, { env: cap }))).toEqual({ wait_s: 300, reason: "too_many_leases" });
    ok(await finish(m, a.lease_id, [], { given_back: true }));
    advance(5);
    await lease(m, { env: cap });
    advance(200); // both silent past 3 beats: expired, even before the sweep
    await lease(m, { env: cap });
    expect(ok(await call("GET", "/v1/admin/stats", { token: env.ADMIN_TOKEN, env: { ...cap } })).limits.max_active_leases).toBe(2);
  });
});

describe("crash reports (M3)", () => {
  it("at most MAX_CRASHES_PER_DAY rows per machine per day; a block is still recorded", async () => {
    const m = await enroll("pc");
    const w = await makeBuild({ tag: "w" });
    const cap = { MAX_CRASHES_PER_DAY: "3" };
    const crash = (body: object) => call("POST", "/v1/crash", { token: m.token, body, env: cap });
    for (let i = 0; i < 3; i++) expect(ok(await crash({ kind: "crash", message: `#${i}` }))).toEqual({ ok: true });
    expect(ok(await crash({ kind: "panic", message: "#3" }))).toEqual({ ok: true, stored: false });
    expect(ok(await crash({ kind: "blocked", build_id: w, os_error: 4551 }))).toEqual({ ok: true, stored: false });
    expect((await env.DB.prepare(`SELECT COUNT(*) AS n FROM crashes`).first<any>()).n).toBe(3);
    expect((await env.DB.prepare(`SELECT COUNT(*) AS n FROM blocks`).first<any>()).n).toBe(1);
    advance(86401);
    expect(ok(await crash({ kind: "crash", message: "a day later" }))).toEqual({ ok: true });
  });

  it("the cron deletes crash rows after CRASH_RETENTION_DAYS and diagnostics after DIAG_RETENTION_DAYS", async () => {
    await makeRun("r", { define: false });
    const m = await enroll("pc");
    ok(await call("POST", "/v1/crash", { token: m.token, body: { kind: "crash", message: "old" } }));
    const up = ok(await call("POST", "/v1/diag/logs", { token: m.token, raw: "logs", headers: { "x-farm-diag": "user" } }));
    ok(await admin("POST", "/v1/admin/diag", { machine_id: m.machine_id, kind: "sweep" }));
    const dl = await lease(m, { agent: LEGACY });
    expect(dl.kind).toBe("diag");
    ok(await put(m, dl.lease_id, 0, "sweep results"));
    ok(await finish(m, dl.lease_id, [0]));
    expect(await objects(`diag/${m.machine_id}/`)).toHaveLength(2);
    expect(await runCron(env, clock.now + 13 * 86400)).toMatchObject({ crashes: 0, diag: 0 });
    expect(await runCron(env, clock.now + 15 * 86400)).toMatchObject({ crashes: 0, diag: 2 });
    expect(await objects("diag/")).toEqual([]);
    const n = async (t: string) => (await env.DB.prepare(`SELECT COUNT(*) AS n FROM ${t}`).first<any>()).n;
    expect([await n("diag_uploads"), await n("diag_requests"), await n("results")]).toEqual([0, 0, 0]);
    expect(await n("crashes")).toBe(1);
    expect((await runCron(env, clock.now + 31 * 86400)).crashes).toBe(1);
    expect(await n("crashes")).toBe(0);
    expect(up.key).toMatch(/^diag\//);
  });
});

describe("client IPs (enroll rate limit)", () => {
  it("are stored only as an HMAC (IPv6 by /64) and deleted once the limit has reset", async () => {
    const { codes } = ok(await admin("POST", "/v1/admin/invites", { count: 3 }));
    const from = (ip: string, code: string) => call("POST", "/v1/enroll", { headers: { "cf-connecting-ip": ip }, body: { invite: code, name: "pc", profile: PROFILE } });
    ok(await from("203.0.113.9", codes[0]));
    ok(await from("2001:db8:1:2:aaaa::1", codes[1]));
    ok(await from("2001:0db8:1:2:bbbb:cccc:dddd:eeee", codes[2])); // the same /64
    const rows = (await env.DB.prepare(`SELECT key, tat FROM rate_limits ORDER BY key`).all<any>()).results;
    expect(rows).toHaveLength(2);
    for (const r of rows) {
      expect(r.key).toMatch(/^ip:[A-Za-z0-9_-]{24}$/);
      expect(r.key).not.toContain("203");
      expect(r.key).not.toContain("2001");
    }
    expect(rows.map((r: any) => r.key).sort()).toEqual([await ipKey("203.0.113.9", "test-ip-salt"), await ipKey("2001:db8:1:2::", "test-ip-salt")].sort());
    expect(await ipKey("203.0.113.9", "another salt")).not.toBe(await ipKey("203.0.113.9", "test-ip-salt"));
    // Gone at the first cron run after the limit has reset (a few minutes), machine rows kept a day.
    await runCron(env, clock.now + 60);
    expect((await env.DB.prepare(`SELECT COUNT(*) AS n FROM rate_limits`).first<any>()).n).toBe(2);
    await runCron(env, clock.now + 200);
    expect((await env.DB.prepare(`SELECT COUNT(*) AS n FROM rate_limits`).first<any>()).n).toBe(0);
  });

  it("ipPrefix: IPv4 as is, IPv6 its /64", () => {
    expect(ipPrefix("203.0.113.9")).toBe("203.0.113.9");
    expect(ipPrefix("2001:DB8:0:1::5")).toBe("2001:db8:0:1::/64");
    expect(ipPrefix("2001:db8::1")).toBe("2001:db8:0:0::/64");
    expect(ipPrefix("::1")).toBe("0:0:0:0::/64");
    expect(ipPrefix("2001:0db8:0001:0002:0003:0004:0005:0006")).toBe("2001:db8:1:2::/64");
    expect(ipPrefix("::ffff:192.0.2.1")).toBe("0:0:0:0::/64");
  });
});

describe("the coordinator's mark and other edges", () => {
  it("every /v1/* answer carries x-zerofarm-coordinator: 1, errors included; other pages don't", async () => {
    await makeCampaign("fz");
    const m = await enroll("pc");
    const mark = (r: { headers: Headers }) => r.headers.get("x-zerofarm-coordinator");
    expect(mark(await ask(m))).toBe("1");
    expect(mark(await call("GET", "/v1/agent/latest", { token: m.token }))).toBe("1"); // 404
    expect(mark(await call("GET", "/v1/agent/latest", { token: "nope" }))).toBe("1"); // 401
    expect(mark(await call("GET", "/v1/admin/stats"))).toBe("1"); // 401
    ok(await admin("POST", `/v1/admin/machine/${m.machine_id}/revoke`, {}));
    const revoked = err(await ask(m), 403, "revoked");
    expect(revoked).toBeTruthy();
    expect(mark(await ask(m))).toBe("1"); // 403 revoked
    expect(mark(await call("DELETE", "/v1/lease"))).toBe("1"); // 405
    expect(mark(await call("GET", "/v1/nothing"))).toBe("1"); // 404
    const w = await makeBuild({ tag: "x" });
    const m2 = await enroll("pc2");
    const art = await call("GET", `/v1/artifact/builds/${w}/manifest.json`, { token: m2.token });
    expect([art.status, mark(art)]).toEqual([200, "1"]);
    const range = await call("GET", `/v1/artifact/builds/${w}/manifest.json`, { token: m2.token, headers: { range: "bytes=0-3" } });
    expect([range.status, mark(range)]).toEqual([206, "1"]);
    for (const p of ["/", "/admin"]) expect(mark(await call("GET", p))).toBeNull();
  });

  it("malformed percent-encoding in a key is a 400, not a 500 (L8)", async () => {
    const m = await enroll("pc");
    err(await call("GET", "/v1/artifact/builds/%E0%A4%A", { token: m.token }), 400, "bad_request");
    err(await admin("GET", "/v1/admin/artifact/builds/%E0%A4%A"), 400, "bad_request");
  });

  it("an installer is only this repository's agent-v<version>/ZeroFarmSetup.exe (H4)", async () => {
    const id = await makeBuild({ kind: "agent", version: "0.3.0" });
    const rel = (github_url: string, e?: Record<string, string>) =>
      admin("POST", "/v1/admin/agent/release", { version: "0.3.0", build_id: id, channel: "stable", min_agent: "0.1.0", github_url }, { env: e });
    const good = "https://github.com/greggg230/zero-farm/releases/download/agent-v0.3.0/ZeroFarmSetup.exe";
    for (const u of [
      "https://github.com/someone/zero-farm/releases/download/agent-v0.3.0/ZeroFarmSetup.exe",
      "https://github.com/greggg230/other/releases/download/agent-v0.3.0/ZeroFarmSetup.exe",
      "https://github.com/greggg230/zero-farm/releases/download/agent-v0.2.0/ZeroFarmSetup.exe",
      "https://github.com/greggg230/zero-farm/releases/download/v0.3.0/ZeroFarmSetup.exe",
      "https://github.com/greggg230/zero-farm/releases/download/agent-v0.3.0/Other.exe",
      "http://github.com/greggg230/zero-farm/releases/download/agent-v0.3.0/ZeroFarmSetup.exe",
      good + "?x=1",
    ]) {
      expect(err(await rel(u), 400, "bad_request").message).toContain(good);
    }
    expect(ok(await rel(good)).github_url).toBe(good);
    // GITHUB_REPO is a var (a fork deploys its own coordinator).
    const fork = "https://github.com/fork/zf/releases/download/agent-v0.3.0/ZeroFarmSetup.exe";
    err(await rel(fork), 400, "bad_request");
    ok(await rel(fork, { GITHUB_REPO: "fork/zf" }));
  });
});
