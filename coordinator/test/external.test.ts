// The owner's own machines (POST /v1/admin/external): a desktop reporter pushes their numbers;
// the admin page merges them with the farm's PCs (Contributors, totals by origin, per run). Each
// POST replaces its reporter's sources; offline after 30 min; deleted after 7 days; never on a
// public page.

import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { runCron } from "../src/cron";
import { admin, advance, call, clock, enroll, err, lease, makeRun, ok, upload, finish } from "./helpers";

const source = (o: Record<string, unknown> = {}) => ({
  id: "local-desktop",
  name: "Desktop (local)",
  kind: "local",
  hardware: "i9-13900K / RTX 5090",
  working_on: ["run:r", "campaign:mac-fuzz"],
  selfplay_games: { "1h": 120, "24h": 2400 },
  eval_games: { "1h": 10, "24h": 50 },
  fuzz_seeds: { "1h": 0, "24h": 90000 },
  busy_h_24h: 20.5,
  ...o,
});
const report = (sources: object[], reporter = "desktop") => ({ reporter, t: clock.now, sources });
const post = (body: unknown, o: { token?: string | null; raw?: string } = {}) =>
  call("POST", "/v1/admin/external", { token: o.token === undefined ? env.ADMIN_TOKEN : o.token, body: o.raw === undefined ? body : undefined, raw: o.raw });

describe("external contributions (owner's machines)", () => {
  it("are merged with the farm's PCs into contributors, totals by origin and per run", async () => {
    await makeRun("r", { games_per_lease: 4 });
    const m = await enroll("sam-pc");
    const l = await lease(m);
    ok(await upload(m, l.lease_id, 0, "games", { ids: [[0, 4]] }));
    ok(await finish(m, l.lease_id, [0]));
    expect(ok(await post(report([source(), source({ id: "mac", name: "MacBook", kind: "ssh", working_on: [], hardware: null, busy_h_24h: null })])))).toEqual({
      ok: true,
      reporter: "desktop",
      sources: 2,
    });
    const s = ok(await admin("GET", "/v1/admin/stats"));
    const farm = s.contributors.find((c: any) => c.origin === "farm");
    expect(farm).toMatchObject({ kind: "farm", id: m.machine_id, name: "sam-pc", status: "online", selfplay: { h1: 4, h24: 4 }, eval: { h1: 0, h24: 0 } });
    const own = s.contributors.find((c: any) => c.id === "desktop/local-desktop");
    expect(own).toMatchObject({ origin: "own", kind: "local", name: "Desktop (local)", status: "working", selfplay: { h1: 120, h24: 2400 }, fuzz: { h1: 0, h24: 90000 } });
    expect(s.contributors.find((c: any) => c.id === "desktop/mac")).toMatchObject({ kind: "ssh", status: "online" });
    expect(s.totals).toEqual({
      farm: { selfplay: { h1: 4, h24: 4 }, eval: { h1: 0, h24: 0 }, fuzz: { h1: 0, h24: 0 } },
      own: { selfplay: { h1: 240, h24: 4800 }, eval: { h1: 20, h24: 100 }, fuzz: { h1: 0, h24: 180000 } },
      all: { selfplay: { h1: 244, h24: 4804 }, eval: { h1: 20, h24: 100 }, fuzz: { h1: 0, h24: 180000 } },
    });
    expect(s.by_scope).toEqual([
      { scope: "campaign:mac-fuzz", farm: { selfplay: { h1: 0, h24: 0 }, eval: { h1: 0, h24: 0 }, fuzz: { h1: 0, h24: 0 } }, own: ["Desktop (local)"] },
      { scope: "run:r", farm: { selfplay: { h1: 4, h24: 4 }, eval: { h1: 0, h24: 0 }, fuzz: { h1: 0, h24: 0 } }, own: ["Desktop (local)"] },
    ]);
    const page = (await admin("GET", "/admin")).body as string;
    for (const x of ["Contributors", "own: ", "Per run and campaign"]) expect(page).toContain(x);
    // Never on a public page.
    expect((await call("GET", "/")).body).not.toContain("Desktop (local)");
  });

  it("each POST replaces the reporter's sources; offline after 30 min; deleted after 7 days", async () => {
    ok(await post(report([source(), source({ id: "vast-1", name: "Vast 1", kind: "vast" })])));
    ok(await post(report([source({ id: "other", name: "Other" })], "laptop")));
    ok(await post(report([source()])));
    const ids = async () => ok(await admin("GET", "/v1/admin/stats")).contributors.map((c: any) => `${c.id}:${c.status}`).sort();
    expect(await ids()).toEqual(["desktop/local-desktop:working", "laptop/other:working"]);
    advance(31 * 60);
    ok(await post(report([source({ id: "other", name: "Other" })], "laptop")));
    expect(await ids()).toEqual(["desktop/local-desktop:offline", "laptop/other:working"]);
    // An offline source's last hour no longer counts; its day still does while under a day old.
    const s = ok(await admin("GET", "/v1/admin/stats"));
    expect(s.totals.own.selfplay).toEqual({ h1: 120, h24: 4800 });
    advance(86401);
    expect(ok(await admin("GET", "/v1/admin/stats")).totals.own.selfplay).toEqual({ h1: 0, h24: 0 });
    // The clock is a day on: both were last reported a day ago, so 5 more days keep them...
    await runCron(env, clock.now + 5 * 86400);
    expect(await ids()).toEqual(["desktop/local-desktop:offline", "laptop/other:offline"]);
    // ...and past 7 days since, they go.
    await runCron(env, clock.now + 6 * 86400);
    expect(await ids()).toEqual([]);
    // An empty list clears the reporter.
    ok(await post(report([source()])));
    ok(await post(report([])));
    expect(await ids()).toEqual([]);
  });

  it("validates reports and is admin only", async () => {
    const bad = async (body: unknown, what: RegExp) => expect(err(await post(body), 400, "bad_request").message).toMatch(what);
    await bad(report([source({ kind: "cloud" })]), /kind must be one of/);
    await bad(report([source(), source()]), /listed twice/);
    await bad(report([source({ id: "Bad Id" })]), /id is malformed/);
    await bad(report([source({ selfplay_games: { "1h": -1 } })]), /must be >= 0/);
    await bad(report([source({ eval_games: { "7d": 1 } })]), /only "1h" and "24h"/);
    await bad(report([source({ busy_h_24h: 25 })]), /busy_h_24h must be <= 24/);
    await bad(report([source({ working_on: ["run:<script>"] })]), /working_on/);
    await bad(report([source({ name: "tab\there" })]), /printable/);
    await bad(report(Array.from({ length: 65 }, (_, i) => source({ id: `s${i}` }))), /at most 64/);
    await bad({ reporter: "desktop", sources: [] }, /t is required/);
    await bad({ reporter: "Desk Top", t: 1, sources: [] }, /reporter is malformed/);
    err(await post(undefined, { raw: JSON.stringify(report([source({ hardware: "x".repeat(70000) })])) }), 413, "too_large");
    // Admin only: no token, a machine token.
    err(await post(report([source()]), { token: null }), 401, "unauthorized");
    const m = await enroll("pc");
    err(await post(report([source()]), { token: m.token }), 401, "unauthorized");
    // Missing windows count as zero.
    ok(await post(report([{ id: "bare", name: "Bare", kind: "other" }])));
    const bare = ok(await admin("GET", "/v1/admin/stats")).contributors.find((c: any) => c.origin === "own");
    expect(bare).toMatchObject({ id: "desktop/bare", selfplay: { h1: 0, h24: 0 }, working_on: [], status: "online" });
  });
});
