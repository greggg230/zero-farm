// The freshness budget (PROTOCOL 18, rev 26): a rolling self-play lease carries `rolling.fresh_s`, the seconds a game may
// last so its first rows still reach the trainer within the run's staleness window, worked out per run from the run's
// own pace (the median gap between its checkpoint registrations); beat and extension answers keep it current. Only
// agents from FRESH_AGENT on machines the owner's `fresh_cap` names (default: the canary channel) are sent it; every
// other answer keeps exactly its keys of before.

import { describe, expect, it } from "vitest";
import { FRESH_DELAY_S, FRESH_MARGIN, FRESH_MAX_S, FRESH_MIN_S, freshBudget, genPeriod } from "../src/fresh";
import { HW_ROUTING_DEFAULT, freshApplies, hwRoutingOf } from "../src/routing";
import { type Machine, type RunOpts, admin, advance, call, clock, enroll, err, makeRun, ok, publishCkpt, runBody } from "./helpers";

const FRESH = "0.3.17"; // FRESH_AGENT's default
const OLD = "0.3.16";
const ROLLING = { inflight: 8, gpu_inflight: 64, block: 1000, flush_s: 120 };
const routing = (body: unknown) => admin("POST", "/v1/admin/routing", body);

// Registration times, newest gen first, `gap` seconds apart, the newest at `now`.
const steady = (n: number, gap: number, now = 10_000) => Array.from({ length: n }, (_, i) => now - i * gap);
const want = (period: number, maxStale = 4, flush = 120) => Math.floor(FRESH_MARGIN * (maxStale + 1) * period - flush - FRESH_DELAY_S);

describe("the budget's formula (18.1)", () => {
  it("needs FRESH_MIN_GAPS gaps: a run with too little history has none", () => {
    expect(genPeriod([], 0)).toBe(null);
    expect(genPeriod([100], 100)).toBe(null);
    expect(freshBudget(steady(4, 120), 4, 120, 10_000)).toBe(null); // 3 gaps
    expect(freshBudget(steady(5, 120), 4, 120, 10_000)).toBe(want(120)); // 4 gaps
  });

  it("steady generations: margin x (max_stale + 1) x period - flush_s - the beat's delay", () => {
    expect(genPeriod(steady(9, 120), 10_000)).toBe(120);
    expect(freshBudget(steady(9, 120), 4, 120, 10_000)).toBe(270); // 0.75 x 5 x 120 - 120 - 60
    expect(freshBudget(steady(9, 90), 4, 120, 10_000)).toBe(157); // legacy-meta3-r1's pace of 2026-11-22
    expect(freshBudget(steady(9, 120), 1, 30, 10_000)).toBe(FRESH_MIN_S); // max_stale 1: 180 - 90 = 90, under the floor
    expect(freshBudget(steady(9, 600), 0, 120, 10_000)).toBe(want(600, 0)); // max_stale 0: one period
  });

  it("is robust to outliers: a trainer pause, a burst, gens registered out of order", () => {
    const ts = steady(9, 120);
    ts[3] = ts[2] - 3600; // one long gap (the trainer stopped for an hour)...
    for (let i = 4; i < 9; i++) ts[i] = ts[i - 1] - 120;
    expect(genPeriod(ts, 10_000)).toBe(120); // ...the median keeps the pace
    const burst = steady(9, 120);
    burst[1] = burst[0] - 2; // two gens registered seconds apart
    expect(genPeriod(burst, 10_000)).toBe(120);
    // an older gen registered late (an eval opponent): its gap is negative, left out
    const late = [10_000, 9_880, 9_760, 9_990, 9_520, 9_400, 9_280];
    expect(genPeriod(late, 10_000)).toBe(120);
  });

  it("counts the open gap once it is longer than the median (the trainer slowed down), floors and caps", () => {
    const ts = steady(9, 120);
    expect(genPeriod(ts, 10_000 + 60)).toBe(120); // within the median: not counted
    expect(genPeriod(ts, 10_000 + 3000)).toBe(120); // one long open gap among 9: the median moves to 120 still
    const short = steady(5, 120); // 4 gaps + a long open gap: median of 5
    expect(genPeriod(short, 10_000 + 3000)).toBe(120);
    expect(genPeriod([10_000, 9_880, 9_760, 9_400, 9_000], 10_000 + 3000)).toBe(360); // gaps 120,120,360,400 + 3000
    expect(freshBudget(steady(9, 10), 4, 120, 10_000)).toBe(FRESH_MIN_S); // fast gens: the floor
    expect(freshBudget(steady(9, 86_400), 4, 120, 10_000)).toBe(FRESH_MAX_S);
  });
});

describe("the owner's setting fresh_cap (18.4)", () => {
  it("defaults to canary; stored values are checked; POST validates", async () => {
    expect(HW_ROUTING_DEFAULT.fresh_cap).toBe("canary");
    expect(hwRoutingOf(JSON.stringify({ fresh_cap: "maybe" })).fresh_cap).toBe("canary");
    expect(hwRoutingOf(JSON.stringify({ fresh_cap: "all" })).fresh_cap).toBe("all");
    const hw = (fresh_cap: "off" | "canary" | "all") => ({ ...HW_ROUTING_DEFAULT, fresh_cap });
    expect([freshApplies(hw("off"), "canary"), freshApplies(hw("canary"), "canary"), freshApplies(hw("canary"), "stable"), freshApplies(hw("all"), "stable")]).toEqual([
      false,
      true,
      false,
      true,
    ]);
    err(await routing({ fresh_cap: "yes" }), 400, "bad_request");
    expect(ok(await routing({ fresh_cap: "all" }))).toMatchObject({ ok: true, fresh_cap: "all", priority_lane: "canary" });
    expect(ok(await admin("GET", "/v1/admin/routing"))).toMatchObject({ fresh_cap: "all", stored: true });
  });
});

// A rolling run whose gens 1..n were registered `gap` seconds apart (the last one now).
async function pacedRun(run: string, gens: number, gap: number, o: RunOpts = {}): Promise<void> {
  const build = await makeRun(run, { max_stale: 4, quota: 100_000, ...o });
  const body: any = runBody(build, { max_stale: 4, quota: 100_000, ...o });
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
async function rollingLease(m: Machine, agent: string): Promise<any> {
  const b = ok(await askRolling(m, agent));
  expect(b.lease?.rolling, JSON.stringify(b)).toBeTruthy();
  return b.lease;
}
const extend = (m: Machine, id: string, agent: string) => call("POST", `/v1/lease/${id}/extend`, { token: m.token, agent, body: { games: 4 } });
const beatAs = (m: Machine, id: string, agent: string) =>
  call("POST", `/v1/lease/${id}/beat`, { token: m.token, agent, body: { state: "running", progress: { games_done: 0, games_total: 1 }, gen: 1 } });

describe("rolling leases carry the budget (18.2)", () => {
  it("a canary machine's agent from FRESH_AGENT: the lease, its beats and its extensions; the admin GET shows it", async () => {
    await pacedRun("fr", 9, 120);
    const m = await enroll("pc", { channel: "canary" });
    const l = await rollingLease(m, FRESH);
    expect(l.rolling.fresh_s).toBe(270);
    expect(ok(await beatAs(m, l.lease_id, FRESH)).fresh_s).toBe(270);
    const x = ok(await extend(m, l.lease_id, FRESH));
    expect(x.fresh_s).toBe(270);
    expect(typeof x.games).toBe("number");
    expect(ok(await admin("GET", "/v1/admin/run/fr")).fresh_s).toBe(270);
    // the trainer slows down: the answers follow
    for (let g = 10; g <= 13; g++) {
      for (let i = 0; i < 10; i++) {
        advance(60);
        ok(await beatAs(m, l.lease_id, FRESH)); // (the lease stays alive)
      }
      await publishCkpt("fr", g);
    }
    // gaps now 600 x 4, 120 x 4: median 360
    expect(ok(await beatAs(m, l.lease_id, FRESH)).fresh_s).toBe(want(360));
    expect(ok(await extend(m, l.lease_id, FRESH)).fresh_s).toBe(want(360));
  });

  it("a run with too little history: no fresh_s in the lease, null in the answers", async () => {
    await pacedRun("young", 3, 120);
    const m = await enroll("pc", { channel: "canary" });
    const l = await rollingLease(m, FRESH);
    expect("fresh_s" in l.rolling).toBe(false);
    const b = ok(await beatAs(m, l.lease_id, FRESH));
    expect(b.fresh_s).toBe(null);
    expect(ok(await extend(m, l.lease_id, FRESH)).fresh_s).toBe(null);
    expect(ok(await admin("GET", "/v1/admin/run/young")).fresh_s).toBe(null);
  });

  it("older agents, machines fresh_cap leaves out, and fresh_cap off get the answers of before", async () => {
    await pacedRun("fr", 9, 120);
    const canary = await enroll("c", { channel: "canary" });
    const stable = await enroll("s");
    const keysOf = (x: any) => Object.keys(x).sort();
    for (const [m, agent] of [
      [canary, OLD],
      [stable, FRESH],
    ] as const) {
      const l = await rollingLease(m, agent);
      expect("fresh_s" in l.rolling).toBe(false);
      const b = ok(await beatAs(m, l.lease_id, agent));
      expect("fresh_s" in b).toBe(false);
      expect(keysOf(b)).toEqual(["action", "checkpoint", "expires_at"]);
      const x = ok(await extend(m, l.lease_id, agent));
      expect("fresh_s" in x).toBe(false);
    }
    // "all": a stable machine too
    ok(await routing({ fresh_cap: "all" }));
    const stable2 = await enroll("s2");
    const l = await rollingLease(stable2, FRESH);
    expect(l.rolling.fresh_s).toBe(270);
    expect(ok(await beatAs(stable2, l.lease_id, FRESH)).fresh_s).toBe(270);
    // "off": nobody, the running lease's answers lose the key
    ok(await routing({ fresh_cap: "off" }));
    expect("fresh_s" in ok(await beatAs(stable2, l.lease_id, FRESH))).toBe(false);
    expect("fresh_s" in ok(await extend(stable2, l.lease_id, FRESH))).toBe(false);
  });

  it("is per run: two runs of different paces give their own budgets on the same machine", async () => {
    await pacedRun("fast", 9, 60);
    const t = clock.now;
    await pacedRun("slow", 9, 300);
    expect(clock.now).toBeGreaterThan(t);
    expect(ok(await admin("GET", "/v1/admin/run/fast")).fresh_s).toBe(FRESH_MIN_S); // 0.75 x 5 x 60 - 180 = 45 -> floor
    expect(ok(await admin("GET", "/v1/admin/run/slow")).fresh_s).toBe(want(300));
  });
});
