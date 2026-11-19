// Layout tuning (PROTOCOL 13, rev 19): agents report the CPU layouts they tuned with each lease request (`tuning`); the
// coordinator keeps the latest report per machine, shows it per machine and per hardware class, and gives a rolling
// self-play lease to an agent that lists the feature "tuning" the best layout its hardware class reported for the
// lease's build and runtime (`tuning_hint`). Reports are validated; nothing else changes for anyone else.

import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { type Machine, PROFILE, admin, advance, call, enroll, err, makeRun, ok, runBody } from "./helpers";
import { cleanTuning, hintFor, type TuningReport } from "../src/tuning";

const ROLL = "0.3.1";
const LAYOUT = { threads: 4, infer_workers: 4, infer_threads: 3, groups: 4, conc: 2304 };

async function rollingRun(run: string): Promise<string> {
  const build = await makeRun(run, { quota: 100000 });
  const body: any = runBody(build, { quota: 100000 });
  body.selfplay.rolling = { inflight: "auto", gpu_inflight: "auto", block: 10000 };
  ok(await admin("PUT", `/v1/admin/run/${run}`, body));
  return build;
}

function report(o: Partial<TuningReport> & { build: string }): TuningReport {
  return { run: "tr", runtime: null, net: "0123456789abcdef", layout: LAYOUT, leaves_s: 15000, trials: 9, state: "tuned", t: 1, ...o };
}

// (reports go with kinds no work exists for, so the reporting machines take no lease)
function ask(m: Machine, o: { tuning?: unknown; features?: string[]; profile?: object | null; kinds?: string[] } = {}) {
  advance(10); // (the lease rate limit: one per 5 s after a burst)
  return call("POST", "/v1/lease", {
    token: m.token,
    agent: ROLL,
    body: {
      profile_digest: "ab".repeat(32),
      profile: o.profile ?? null,
      caps: { threads: 16, ram_mb: 30000, gpu: false, disk_mb: 2500 },
      cached: { builds: [], ckpts: [] },
      kinds: o.kinds ?? ["selfplay"],
      features: o.features ?? ["rolling", "tuning"],
      ...(o.tuning !== undefined ? { tuning: o.tuning } : {}),
    },
  });
}

const stored = (m: Machine) => env.DB.prepare(`SELECT tuning, tuning_t FROM machines WHERE machine_id = ?`).bind(m.machine_id).first<any>();

describe("tuning reports (13.1)", () => {
  it("are validated, stored per machine (the latest) and shown in the stats per machine and per class", async () => {
    const build = await rollingRun("tr");
    const a = await enroll("a");
    const b = await enroll("b");
    for (const bad of [
      "fast",
      [{}],
      [report({ build: "x" })],
      [report({ build, net: "NOTHEX0123456789" })],
      [report({ build, layout: { ...LAYOUT, threads: 0 } })],
      [report({ build, layout: { ...LAYOUT, groups: 5000, conc: 4096 } })],
      [report({ build, layout: { ...LAYOUT, infer_workers: 65 } })],
      [report({ build, state: "great" as any })],
      [report({ build, leaves_s: -1 })],
      Array(5).fill(report({ build })),
    ]) {
      err(await ask(a, { tuning: bad, kinds: ["fuzz"] }), 400, "bad_request");
    }
    expect((await stored(a)).tuning).toBeNull();
    const r1 = report({ build, leaves_s: 14000 });
    ok(await ask(a, { tuning: [r1], kinds: ["fuzz"] }));
    expect(JSON.parse((await stored(a)).tuning)).toEqual([r1]);
    // absent or empty: the stored report stays
    ok(await ask(a, { kinds: ["fuzz"] }));
    ok(await ask(a, { tuning: [], kinds: ["fuzz"] }));
    expect(JSON.parse((await stored(a)).tuning)).toEqual([r1]);
    const r2 = report({ build, leaves_s: 16000, layout: { ...LAYOUT, groups: 6 } });
    ok(await ask(b, { tuning: [r2, report({ build, run: "other", net: "fedcba9876543210", state: "searching" })], kinds: ["fuzz"] }));
    const s = ok(await admin("GET", "/v1/admin/stats"));
    const ma = s.machines.find((m: any) => m.machine_id === a.machine_id);
    expect(ma.tuning).toEqual([r1]);
    expect(typeof ma.tuning_t).toBe("number");
    // per class (both machines have the test profile): the best layout per build, runtime and network
    const rows = s.tuning.filter((x: any) => x.build === build);
    expect(rows).toHaveLength(2);
    const main = rows.find((x: any) => x.net === "0123456789abcdef");
    expect(main).toMatchObject({ machines: 2, leaves_s: 16000, layout: r2.layout, runs: ["tr"], state: "tuned", platform: "windows-x64" });
    expect(main.class).toContain(PROFILE.cpu);
    // the admin page renders them
    const page = await call("GET", "/admin", { token: null });
    expect(page.status).toBe(200);
  });
});

describe("tuning hints (13.2)", () => {
  it("give a rolling lease the best layout of the machine's hardware class for its build and runtime", async () => {
    const build = await rollingRun("th");
    const reporter = await enroll("reporter");
    const other = await enroll("other-class", { profile: { ...PROFILE, cpu: "Apple M4 Max" }, digest: "cd".repeat(32) });
    const asker = await enroll("asker");
    // the reporter tuned it; a machine of another class reported a faster layout (not this class's)
    ok(await ask(reporter, { tuning: [report({ run: "th", build, leaves_s: 9000 }), report({ run: "th", build: "0".repeat(24), leaves_s: 99999 })], kinds: ["fuzz"] }));
    const fast = { ...LAYOUT, threads: 8 };
    ok(await call("POST", "/v1/lease", {
      token: other.token,
      agent: ROLL,
      body: { profile_digest: "cd".repeat(32), profile: null, caps: { threads: 16, ram_mb: 30000, gpu: false, disk_mb: 2500 }, cached: { builds: [], ckpts: [] }, kinds: ["fuzz"], features: ["rolling", "tuning"], tuning: [report({ run: "th", build, leaves_s: 50000, layout: fast })] },
    }));
    const l = ok(await ask(asker)).lease;
    expect(l.rolling, JSON.stringify(l)).toBeTruthy();
    expect(l.tuning_hint).toEqual({ layout: LAYOUT, leaves_s: 9000, machines: 1, net: "0123456789abcdef" });
  });

  it("only to agents that list the feature, and never for a classic lease", async () => {
    const build = await rollingRun("tf");
    const reporter = await enroll("r2");
    ok(await ask(reporter, { tuning: [report({ run: "tf", build })], features: ["rolling"], kinds: ["fuzz"] }));
    const a = await enroll("no-feature");
    const l = ok(await ask(a, { features: ["rolling"] })).lease;
    expect(l.rolling).toBeTruthy();
    expect(l.tuning_hint).toBeUndefined();
    const c = await enroll("classic");
    const l2 = ok(await ask(c, { features: ["tuning"] })).lease;
    expect(l2.rolling).toBeUndefined();
    expect(l2.tuning_hint).toBeUndefined();
  });

  it("prefer the lease's run, then tuned over searching, then the faster", () => {
    const b = "a".repeat(24);
    const r = (o: Partial<TuningReport>) => report({ build: b, ...o });
    const lay = (threads: number) => ({ ...LAYOUT, threads });
    const reports = [
      { machine_id: "m1", reports: [r({ run: "x", leaves_s: 20000, layout: lay(1), net: "1111111111111111" })] },
      { machine_id: "m2", reports: [r({ run: "y", leaves_s: 9000, layout: lay(2), state: "searching" })] },
      { machine_id: "m3", reports: [r({ run: "y", leaves_s: 8000, layout: lay(3) })] },
      { machine_id: "m4", reports: [r({ run: "y", leaves_s: 7000, layout: lay(4) }), r({ run: "y", runtime: "b".repeat(24), leaves_s: 99999, layout: lay(5) })] },
    ];
    expect(hintFor(reports, b, null, "y")).toEqual({ layout: lay(3), leaves_s: 8000, machines: 3, net: "0123456789abcdef" });
    expect(hintFor(reports, b, null, "x")?.layout).toEqual(lay(1));
    expect(hintFor(reports, b, null, "z")?.layout, "no report of the run: the fastest tuned").toEqual(lay(1));
    expect(hintFor(reports, b, "b".repeat(24), "y")?.layout, "the runtime must match").toEqual(lay(5));
    expect(hintFor(reports, "c".repeat(24), null, "y")).toBeNull();
    expect(cleanTuning(null)).toBeNull();
    expect(cleanTuning([])).toEqual([]);
  });
});
