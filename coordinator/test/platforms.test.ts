// Rev 17 (PROTOCOL 11): platforms. A Mac (macos-arm64) gets only macOS builds and a Windows PC only Windows builds: a
// run's default goes only to machines of its platform, an alternate names its platform (a CPU alternate has no
// vendor), a campaign names one build per platform; agent releases, min_agent and the downloads are per platform; an
// agent without the header is windows-x64, exactly as before.

import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { bookFor } from "../src/leasebook";
import {
  LEGACY,
  PROFILE,
  admin,
  ask,
  call,
  define,
  enroll,
  err,
  lease,
  makeBuild,
  makeCampaign,
  makeRun,
  ok,
  publishCkpt,
  putArtifact,
  runBody,
  sha256,
  T0,
} from "./helpers";

const MAC = "macos-arm64";
const WIN = "windows-x64";
const MAC_AGENT = "0.3.0"; // PLATFORM_AGENT, the first agent that sends x-farm-platform
const MAC_PROFILE = {
  os: "macOS 26.1",
  cpu: "Apple M4 Pro",
  cores: 12,
  threads: 12,
  perf_cores: 8,
  eff_cores: 4,
  ram_mb: 24576,
  disk_free_mb: 300000,
  gpus: [],
  providers: ["cpu"],
  platform: MAC, // informational (the header counts)
};
const NV = PROFILE; // an RTX 4070
const CPU_ONLY = { ...PROFILE, gpus: [], providers: ["cpu"] };
const GPU_OK = { gpu: false, gpu_allowed: true };
const enrollMac = (name = "mac", o: { channel?: string } = {}) => enroll(name, { profile: MAC_PROFILE, platform: MAC, ...o });

interface Packs {
  cpuRt: string; // Windows CPU pack and the run's default worker
  def: string;
  cuda: string; // Windows CUDA pack and its worker (a GPU alternate)
  alt: string;
  macRt: string; // macOS CPU pack and its worker (a CPU alternate)
  mac: string;
}

async function packs(): Promise<Packs> {
  const cpuRt = await makeBuild({ kind: "runtime", tag: "cpu" });
  const cuda = await makeBuild({ kind: "runtime", tag: "cuda", min_agent: "0.2.10", gpu: { vendor: "nvidia", min_driver: "527.41", providers: ["cuda"] } });
  const macRt = await makeBuild({ kind: "runtime", tag: "mac-cpu", platform: MAC });
  return {
    cpuRt,
    def: await makeBuild({ tag: "w-cpu", requires_runtime: cpuRt }),
    cuda,
    alt: await makeBuild({ tag: "w-cuda", requires_runtime: cuda, min_agent: "0.2.10" }),
    macRt,
    mac: await makeBuild({ tag: "w-mac", requires_runtime: macRt, platform: MAC }),
  };
}

const cudaAlt = (p: Packs) => ({ build_id: p.alt, runtime: p.cuda, when: { vendor: "nvidia" } });
const macAlt = (p: Packs) => ({ build_id: p.mac, runtime: p.macRt, when: { platform: MAC } });

// A self-play run (default: the Windows CPU pack) with the CUDA alternate and the macOS CPU alternate; its definition
// lists every build unless `listMac` is false. With `evalBatch`, an eval batch too.
async function mixedRun(run: string, o: { listMac?: boolean; evalBatch?: boolean } = {}): Promise<Packs> {
  const p = await packs();
  const body = (extra: Record<string, unknown> = {}) => ({
    ...runBody(p.def, { games_per_lease: 8, quota: 100000 }),
    runtime: p.cpuRt,
    alternates: [cudaAlt(p), macAlt(p)],
    ...extra,
  });
  ok(await admin("PUT", `/v1/admin/run/${run}`, body()));
  await publishCkpt(run, 1);
  if (o.evalBatch) {
    await publishCkpt(run, 2);
    const ev = { batch: "b1", games: 40, checkpoints: [2, 1], games_per_lease: 10, job: { settings: { sims: 32 }, seats: [{ net: 2 }, { net: 1 }], paired: true } };
    ok(await admin("PUT", `/v1/admin/run/${run}`, body({ eval: ev })));
  }
  await define("run", run, (d) => {
    d.builds.push(p.alt);
    if (o.listMac !== false) d.builds.push(p.mac);
  });
  return p;
}

const pair = (l: any) => [l.build.build_id, l.runtime?.build_id ?? null];

async function release(version: string, min_agent: string, o: { channel?: string; platform?: string; github?: boolean } = {}) {
  const platform = o.platform ?? WIN;
  const id = await makeBuild({ kind: "agent", version, ...(platform === WIN ? {} : { platform }) });
  const github =
    platform === MAC
      ? `https://github.com/greggg230/zero-farm/releases/download/agent-macos-v${version}/ZeroFarm-macos-arm64.tar.gz`
      : `https://github.com/greggg230/zero-farm/releases/download/agent-v${version}/ZeroFarmSetup.exe`;
  return ok(
    await admin("POST", "/v1/admin/agent/release", {
      version,
      build_id: id,
      channel: o.channel ?? "stable",
      min_agent,
      ...(platform === WIN ? {} : { platform }),
      github_url: o.github ? github : null,
    }),
  );
}

describe("the agent's platform (11.1)", () => {
  it("is stored at enrollment (no header: windows-x64) and shown; an unknown one is refused", async () => {
    const mac = await enrollMac();
    const win = await enroll("win");
    const list = ok(await admin("GET", "/v1/admin/machines")).machines;
    expect(Object.fromEntries(list.map((m: any) => [m.name, m.platform]))).toEqual({ mac: MAC, win: WIN });
    const stats = ok(await admin("GET", "/v1/admin/stats"));
    expect(Object.fromEntries(stats.machines.map((m: any) => [m.name, m.platform]))).toEqual({ mac: MAC, win: WIN });
    expect(stats.limits.platform_agent).toBe("0.3.0");
    expect([mac.name, win.name]).toEqual(["mac", "win"]);
    // An unknown platform: 400, and the invite isn't used up.
    const { codes } = ok(await admin("POST", "/v1/admin/invites", {}));
    const body = { invite: codes[0], name: "pc", profile: PROFILE };
    expect(err(await call("POST", "/v1/enroll", { body, platform: "linux-x64" }), 400, "bad_request").message).toMatch(/unknown platform/);
    ok(await call("POST", "/v1/enroll", { body, platform: WIN }));
  });

  it("is fixed: a request naming another platform is 409 conflict reason platform", async () => {
    await makeCampaign("fz");
    const mac = await enrollMac();
    const win = await enroll("win");
    // A Mac's token without the header (an older, i.e. Windows, agent) or with windows-x64.
    for (const platform of [null, WIN]) {
      const r = err(await ask(mac, { agent: MAC_AGENT, platform, kinds: ["fuzz"] }), 409, "conflict");
      expect(r.reason).toBe("platform");
      expect(r.need_profile).toBeUndefined();
    }
    expect(err(await call("GET", "/v1/agent/latest", { token: mac.token, platform: null }), 409, "conflict").reason).toBe("platform");
    // A Windows PC's token from a Mac.
    expect(err(await ask(win, { platform: MAC, kinds: ["fuzz"] }), 409, "conflict").reason).toBe("platform");
    // An unknown header: 400.
    err(await ask(win, { platform: "beos", kinds: ["fuzz"] }), 400, "bad_request");
    // The right one works (a Windows agent may name its platform too).
    ok(await ask(win, { platform: WIN, kinds: ["fuzz"] }));
    ok(await ask(mac, { agent: MAC_AGENT, kinds: ["fuzz"] }));
  });
});

describe("which build a machine gets (11.4)", () => {
  it("runs: a Mac gets the macOS CPU alternate, Windows PCs the CUDA alternate or the default, never another platform's", async () => {
    const p = await mixedRun("mixed");
    const mac = await enrollMac();
    const nv = await enroll("nv", { profile: NV });
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    // A CPU alternate has no GPU condition: GPU work allowed or not.
    for (const caps of [GPU_OK, { gpu: false, gpu_allowed: false }]) {
      const l = await lease(mac, { agent: MAC_AGENT, caps, kinds: ["selfplay"] });
      expect(pair(l), JSON.stringify(caps)).toEqual([p.mac, p.macRt]);
      expect(l.definition.key).toMatch(/^defs\/run\/mixed\//);
      expect(l.job.games).toBe(8); // a CPU alternate's lease is no GPU lease
    }
    expect(pair(await lease(nv, { agent: MAC_AGENT, caps: GPU_OK, kinds: ["selfplay"] }))).toEqual([p.alt, p.cuda]);
    for (const m of [nv, cpu]) {
      for (const agent of ["0.2.9", "0.2.14", MAC_AGENT]) {
        const l = await lease(m, { agent, caps: { gpu: false, gpu_allowed: false }, kinds: ["selfplay"] });
        expect(pair(l), `${m.name} ${agent}`).toEqual([p.def, p.cpuRt]);
      }
    }
    // The lease rows carry the builds handed out.
    const rows = (await env.DB.prepare(`SELECT l.build_id, m.platform FROM leases l JOIN machines m USING (machine_id)`).all<any>()).results;
    for (const r of rows) expect(r.platform === MAC ? r.build_id === p.mac : r.build_id !== p.mac, JSON.stringify(r)).toBe(true);
    const run = ok(await admin("GET", "/v1/admin/run/mixed"));
    expect(run.alternates).toEqual([
      { ...cudaAlt(p), problems: [] },
      { ...macAlt(p), problems: [] },
    ]);
    expect(run.platforms).toEqual([WIN, MAC]);
  });

  it("eval too: a Mac plays the batch with the macOS build", async () => {
    const p = await mixedRun("mixed-ev", { evalBatch: true });
    const mac = await enrollMac();
    const l = await lease(mac, { agent: MAC_AGENT, kinds: ["eval"] });
    expect([l.kind, ...pair(l)]).toEqual(["eval", p.mac, p.macRt]);
    const win = await enroll("win", { profile: CPU_ONLY });
    expect(pair(await lease(win, { kinds: ["eval"] }))).toEqual([p.def, p.cpuRt]);
  });

  it("a Mac never gets a Windows default: no macOS alternate (or not in the definition) means nothing from that run", async () => {
    const p = await mixedRun("mixed", { listMac: false });
    await makeCampaign("fz"); // a Windows campaign: nothing for the Mac either
    const mac = await enrollMac();
    for (const caps of [GPU_OK, { gpu: true, gpu_allowed: true }, { gpu: false, gpu_allowed: false }]) {
      expect(ok(await ask(mac, { agent: MAC_AGENT, caps })).reason, JSON.stringify(caps)).toBe("no_work");
    }
    const run = ok(await admin("GET", "/v1/admin/run/mixed"));
    expect(run.alternates[1].problems.join(" ")).toMatch(/is not in definition/);
    expect(run.platforms).toEqual([WIN]);
    // A run with no macOS alternate at all.
    ok(await admin("PUT", "/v1/admin/run/mixed", { ...runBody(p.def, { games_per_lease: 8, quota: 100000 }), runtime: p.cpuRt, alternates: [cudaAlt(p)] }));
    expect(ok(await ask(mac, { agent: MAC_AGENT, caps: GPU_OK })).reason).toBe("no_work");
  });

  it("a macOS-only run: Windows PCs get nothing from it and are never told to update for its builds", async () => {
    await release("0.3.0", "0.2.5"); // the Windows channel offers 0.3.0, which the macOS build's min_agent names
    const p = await packs();
    ok(await admin("PUT", "/v1/admin/run/maconly", { ...runBody(p.mac, { games_per_lease: 5 }), runtime: p.macRt }));
    await publishCkpt("maconly", 1);
    await define("run", "maconly");
    await makeCampaign("fz", { priority: 0.01 });
    const win = await enroll("win", { profile: NV });
    for (const agent of ["0.2.9", "0.2.14", "0.3.0"]) {
      const b = ok(await ask(win, { agent, caps: { gpu: true, gpu_allowed: true } }));
      expect(b.update, agent).toBeUndefined();
      expect([b.lease.kind, b.lease.campaign], agent).toEqual(["fuzz", "fz"]);
    }
    const mac = await enrollMac();
    expect(pair(await lease(mac, { agent: MAC_AGENT, kinds: ["selfplay"] }))).toEqual([p.mac, p.macRt]);
    expect(ok(await admin("GET", "/v1/admin/run/maconly")).platforms).toEqual([MAC]);
    const stats = ok(await admin("GET", "/v1/admin/stats"));
    expect(stats.runs.find((r: any) => r.run === "maconly").platforms).toEqual([MAC]);
  });

  it("a run with a macOS alternate leases Windows PCs exactly as before", async () => {
    const p = await packs();
    const plain = { ...runBody(p.def, { games_per_lease: 6 }), runtime: p.cpuRt };
    ok(await admin("PUT", "/v1/admin/run/r", plain));
    await publishCkpt("r", 1);
    await define("run", "r");
    const win = await enroll("win");
    const before = await lease(win, { kinds: ["selfplay"] });
    ok(await admin("PUT", "/v1/admin/run/r", { ...plain, alternates: [macAlt(p)] }));
    await define("run", "r", (d) => d.builds.push(p.mac));
    const after = await lease(win, { kinds: ["selfplay"] });
    const shape = (l: any) => ({ ...l, lease_id: "", job: { ...l.job, gid_base: 0 }, definition: { ...l.definition, key: "", sha256: "" } });
    expect(shape(after)).toEqual(shape(before));
    expect(Object.keys(after).sort()).toEqual(Object.keys(before).sort());
  });

  it("a macOS alternate that needs a newer agent: the Mac is told to update when its channel offers one, else skipped", async () => {
    const p = await packs();
    const mac2 = await makeBuild({ tag: "w-mac2", requires_runtime: p.macRt, platform: MAC, min_agent: "0.3.5" });
    ok(await admin("PUT", "/v1/admin/run/r", { ...runBody(p.def), runtime: p.cpuRt, alternates: [{ build_id: mac2, runtime: p.macRt, when: { platform: MAC } }] }));
    await publishCkpt("r", 1);
    await define("run", "r", (d) => d.builds.push(mac2));
    await makeCampaign("fz", { priority: 0.01 });
    const mac = await enrollMac();
    // No macOS release offers 0.3.5 (a Windows one does: it doesn't count): skipped, no update.
    await release("0.4.0", "0.2.5");
    expect(ok(await ask(mac, { agent: MAC_AGENT })).reason).toBe("no_work");
    await release("0.3.5", "0.3.0", { platform: MAC });
    expect(ok(await ask(mac, { agent: MAC_AGENT }))).toEqual({ update: { min_agent: "0.3.5" } });
    expect(pair(await lease(mac, { agent: "0.3.5", kinds: ["selfplay"] }))).toEqual([mac2, p.macRt]);
  });

  it("the lease book refuses a build of another platform even if asked for it", async () => {
    const p = await mixedRun("mixed");
    const mac = await enrollMac();
    const win = await enroll("win");
    const grant = (machine_id: string, platform: string, alt?: { build_id: string; runtime: string }) =>
      bookFor(env, "run:mixed").grant({ now: T0, machine_id, kind: "selfplay", want: 4, run: "mixed", def_aware: true, platform, ...(alt ? { alt } : {}) });
    expect(await grant(mac.machine_id, MAC)).toEqual({ ok: null }); // the Windows default
    expect(await grant(win.machine_id, WIN, { build_id: p.mac, runtime: p.macRt })).toEqual({ ok: null });
    const l = (await grant(mac.machine_id, MAC, { build_id: p.mac, runtime: p.macRt })) as any;
    expect(pair(l.ok)).toEqual([p.mac, p.macRt]);
    expect(pair(((await grant(win.machine_id, WIN)) as any).ok)).toEqual([p.def, p.cpuRt]);
  });
});

describe("hardware routing (rev 18) and macOS CPU alternates", () => {
  it("a macOS CPU alternate is not a GPU version: CPU-only Windows PCs keep the self-play ahead of fuzz", async () => {
    const p = await packs();
    ok(await admin("PUT", "/v1/admin/run/maconly", { ...runBody(p.def, { games_per_lease: 8, quota: 100000 }), runtime: p.cpuRt, alternates: [macAlt(p)] }));
    await publishCkpt("maconly", 1);
    await define("run", "maconly", (d) => d.builds.push(p.mac));
    await makeCampaign("fz");
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    expect((await lease(cpu)).kind).toBe("selfplay");
  });

  it("with the CUDA alternate as well, the run has a GPU version: a CPU-only Windows PC takes fuzz first", async () => {
    await mixedRun("mixed");
    await makeCampaign("fz");
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    expect((await lease(cpu)).kind).toBe("fuzz");
  });
});

describe("run alternates per platform: PUT checks (11.4)", () => {
  it("a CPU alternate names its platform; builds and runtimes must be of it; no GPU alternates for macOS", async () => {
    const p = await packs();
    const put = (alternates: unknown, extra: Record<string, unknown> = {}) =>
      admin("PUT", "/v1/admin/run/r", { ...runBody(p.def), runtime: p.cpuRt, alternates, ...extra });
    // A macOS build as a windows-x64 alternate (when.platform's default), or a Windows build as a macOS one.
    expect(err(await put([{ build_id: p.mac, runtime: p.macRt }]), 400, "bad_request").message).toMatch(/is for macos-arm64, not windows-x64/);
    expect(err(await put([{ build_id: p.def, runtime: p.cpuRt, when: { platform: MAC } }]), 400, "bad_request").message).toMatch(/default|is for windows-x64/);
    const w2 = await makeBuild({ tag: "w-cpu2", requires_runtime: p.cpuRt });
    expect(err(await put([{ build_id: w2, runtime: p.cpuRt, when: { platform: MAC } }]), 400, "bad_request").message).toMatch(/is for windows-x64, not macos-arm64/);
    // A macOS worker that requires a Windows pack.
    const odd = await makeBuild({ tag: "w-odd", requires_runtime: p.cpuRt, platform: MAC });
    expect(err(await put([{ build_id: odd, runtime: p.cpuRt, when: { platform: MAC } }]), 400, "bad_request").message).toMatch(/runtime .* is for windows-x64/);
    // No GPU work on macOS; a CPU alternate has no min_driver; unknown platforms and fields are refused.
    expect(err(await put([{ ...macAlt(p), when: { platform: MAC, vendor: "nvidia" } }]), 400, "bad_request").message).toMatch(/no GPU work on macos-arm64/);
    expect(err(await put([{ ...macAlt(p), when: { platform: MAC, min_driver: "1.0" } }]), 400, "bad_request").message).toMatch(/needs a vendor/);
    err(await put([{ ...macAlt(p), when: { platform: "linux-x64" } }]), 400, "bad_request");
    err(await put([{ ...macAlt(p), when: { platform: MAC, os: "13" } }]), 400, "bad_request");
    // The default build and runtime are of one platform.
    expect(err(await put(null, { runtime: p.macRt }), 400, "bad_request").message).toMatch(/is for macos-arm64, but build_id .* is for windows-x64/);
    // Stored as put; a Windows CPU alternate keeps its platform too (so GET -> PUT round-trips), a GPU alternate none.
    const winml = await makeBuild({ kind: "runtime", tag: "winml" });
    const wml = await makeBuild({ tag: "w-winml", requires_runtime: winml });
    ok(await put([cudaAlt(p), { build_id: wml, runtime: winml, when: { platform: WIN } }, macAlt(p)]));
    const got = ok(await admin("GET", "/v1/admin/run/r")).alternates.map(({ problems: _p, ...a }: any) => a);
    expect(got).toEqual([cudaAlt(p), { build_id: wml, runtime: winml, when: { platform: WIN } }, macAlt(p)]);
    ok(await put(got));
    expect(ok(await admin("GET", "/v1/admin/run/r")).alternates.map(({ problems: _p, ...a }: any) => a)).toEqual(got);
    // Still at most 4.
    const extra = await Promise.all([1, 2].map((i) => makeBuild({ tag: `w-mac-x${i}`, requires_runtime: p.macRt, platform: MAC })));
    err(await put([...got, ...extra.map((b) => ({ build_id: b, runtime: p.macRt, when: { platform: MAC } }))]), 400, "bad_request");
  });
});

describe("campaign alternates (11.4)", () => {
  async function macFuzz(o: { game?: string; profile?: string; min_agent?: string; tag?: string } = {}) {
    return makeBuild({ tag: o.tag ?? "fuzz-mac", platform: MAC, game: o.game, profile: o.profile, ...(o.min_agent ? { min_agent: o.min_agent } : {}) });
  }

  it("a Mac gets the campaign's macOS build, a Windows PC its build; GET and stats show them", async () => {
    const win = await makeBuild({ tag: "fuzz-win" });
    const mb = await macFuzz();
    await makeCampaign("fz", { build: win, define: false });
    const body = { build_id: win, mode: "delver", env: { FUZZ_POOL: "delver" }, seed_base: 10_000_000, chunk: 1000, alternates: [{ build_id: mb, when: { platform: MAC } }] };
    ok(await admin("PUT", "/v1/admin/campaign/fz", body));
    await define("campaign", "fz"); // lists only the campaign's own build
    const mac = await enrollMac();
    const pc = await enroll("pc");
    expect(ok(await ask(mac, { agent: MAC_AGENT, kinds: ["fuzz"] })).reason).toBe("no_work");
    let c = ok(await admin("GET", "/v1/admin/campaign/fz"));
    expect(c.alternates).toEqual([{ build_id: mb, when: { platform: MAC }, problems: [expect.stringMatching(/is not in definition/)] }]);
    expect(c.platforms).toEqual([WIN]);
    await define("campaign", "fz", (d) => d.builds.push(mb));
    const lm = await lease(mac, { agent: MAC_AGENT, kinds: ["fuzz"] });
    expect([lm.kind, lm.build.build_id, lm.runtime]).toEqual(["fuzz", mb, null]);
    expect(lm.definition.key).toMatch(/^defs\/campaign\/fz\//);
    expect((await lease(pc, { kinds: ["fuzz"] })).build.build_id).toBe(win);
    c = ok(await admin("GET", "/v1/admin/campaign/fz"));
    expect(c.alternates).toEqual([{ build_id: mb, when: { platform: MAC }, problems: [] }]);
    expect(c.platforms).toEqual([WIN, MAC]);
    const s = ok(await admin("GET", "/v1/admin/stats")).campaigns.find((x: any) => x.campaign === "fz");
    expect([s.alternates, s.platforms]).toEqual([c.alternates, [WIN, MAC]]);
    // The Mac's results land in the campaign like anyone's.
    const r = await call("PUT", `/v1/lease/${lm.lease_id}/result/0`, {
      token: mac.token,
      raw: JSON.stringify({ failures: [] }),
      headers: { "x-sha256": await sha256(JSON.stringify({ failures: [] })), "x-ids": JSON.stringify([[lm.job.first, lm.job.first + lm.job.count]]) },
    });
    expect(ok(r).credited).toBe(lm.job.count);
    // A PUT without alternates removes them.
    const { alternates: _a, ...rest } = body;
    ok(await admin("PUT", "/v1/admin/campaign/fz", rest));
    expect(ok(await admin("GET", "/v1/admin/campaign/fz")).alternates).toBeNull();
    expect(ok(await ask(mac, { agent: MAC_AGENT, kinds: ["fuzz"] })).reason).toBe("no_work");
  });

  it("a macOS-only campaign gives Windows PCs nothing; a Windows-only one gives Macs nothing", async () => {
    const mb = await macFuzz();
    await makeCampaign("macfz", { build: mb });
    await makeCampaign("winfz", { priority: 0.01 });
    const mac = await enrollMac();
    const pc = await enroll("pc");
    for (let i = 0; i < 3; i++) {
      expect((await lease(mac, { agent: MAC_AGENT, kinds: ["fuzz"] })).campaign).toBe("macfz");
      expect((await lease(pc, { kinds: ["fuzz"] })).campaign).toBe("winfz");
    }
  });

  it("PUT checks: one per platform, the campaign's game and profile, the build's platform, at most 4", async () => {
    const win = await makeBuild({ tag: "fuzz-win" });
    const mb = await macFuzz();
    const put = (alternates: unknown) => admin("PUT", "/v1/admin/campaign/c", { build_id: win, mode: "delver", env: {}, seed_base: 0, alternates });
    const at = (build_id: string, platform: string = MAC) => ({ build_id, when: { platform } });
    expect(err(await put([at(mb), at(await macFuzz({ tag: "fuzz-mac2" }))]), 400, "bad_request").message).toMatch(/one per platform/);
    expect(err(await put([at(await macFuzz({ game: "kb" }))]), 400, "bad_request").message).toMatch(/game kb/);
    expect(err(await put([at(await macFuzz({ profile: "fuzz" }))]), 400, "bad_request").message).toMatch(/profile fuzz/);
    expect(err(await put([at(mb, WIN)]), 400, "bad_request").message).toMatch(/is for macos-arm64, not windows-x64/);
    expect(err(await put([at(win, WIN)]), 400, "bad_request").message).toMatch(/campaign's own build/);
    expect(err(await put([{ build_id: mb }]), 400, "bad_request").message).toMatch(/when/);
    expect(err(await put([{ build_id: mb, when: {} }]), 400, "bad_request").message).toMatch(/when.platform is required/);
    err(await put([{ build_id: mb, when: { platform: MAC }, runtime: mb }]), 400, "bad_request");
    err(await put([at(mb, "linux-x64")]), 400, "bad_request");
    err(await put([1, 2, 3, 4, 5].map(() => at(mb))), 400, "bad_request");
    const rt = await makeBuild({ kind: "runtime", tag: "rt-mac", platform: MAC });
    expect(err(await put([at(rt)]), 400, "bad_request").message).toMatch(/runtime build, not worker/);
    ok(await put([at(mb)]));
    expect(ok(await admin("GET", "/v1/admin/campaign/c")).alternates).toEqual([{ ...at(mb), problems: ["the campaign has no definition"] }]);
  });

  it("a macOS campaign build that needs a newer agent: update only when the Mac's channel offers it", async () => {
    const win = await makeBuild({ tag: "fuzz-win" });
    const mb = await macFuzz({ min_agent: "0.3.2" });
    ok(await admin("PUT", "/v1/admin/campaign/fz", { build_id: win, mode: "delver", env: { FUZZ_POOL: "delver" }, seed_base: 0, alternates: [{ build_id: mb, when: { platform: MAC } }] }));
    await define("campaign", "fz", (d) => d.builds.push(mb));
    const mac = await enrollMac();
    expect(ok(await ask(mac, { agent: MAC_AGENT, kinds: ["fuzz"] })).reason).toBe("no_work");
    await release("0.3.2", "0.3.0", { platform: MAC, channel: "canary" }); // canary only: the stable Mac isn't asked
    expect(ok(await ask(mac, { agent: MAC_AGENT, kinds: ["fuzz"] })).reason).toBe("no_work");
    await release("0.3.2", "0.3.0", { platform: MAC });
    expect(ok(await ask(mac, { agent: MAC_AGENT, kinds: ["fuzz"] }))).toEqual({ update: { min_agent: "0.3.2" } });
    expect((await lease(mac, { agent: "0.3.2", kinds: ["fuzz"] })).build.build_id).toBe(mb);
  });
});

describe("agent releases per platform (11.2)", () => {
  it("min_agent and latest are per platform: a Windows-only release never asks a Mac to update, and the reverse", async () => {
    await makeCampaign("fz");
    const mb = await makeBuild({ tag: "fuzz-mac", platform: MAC });
    await makeCampaign("macfz", { build: mb });
    const mac = await enrollMac();
    const win = await enroll("win");
    await release("0.4.0", "0.4.0");
    expect(ok(await ask(win, { agent: "0.3.1" }))).toEqual({ update: { min_agent: "0.4.0" } });
    expect(ok(await ask(mac, { agent: MAC_AGENT })).lease.campaign).toBe("macfz");
    // No macOS release: GET /v1/agent/latest has nothing for the Mac.
    err(await call("GET", "/v1/agent/latest", { token: mac.token }), 404, "not_found");
    await release("0.3.5", "0.3.5", { platform: MAC });
    expect(ok(await ask(mac, { agent: MAC_AGENT }))).toEqual({ update: { min_agent: "0.3.5" } });
    expect(ok(await ask(win, { agent: "0.4.0" })).lease.campaign).toBe("fz");
    expect(ok(await ask(mac, { agent: "0.3.5" })).lease.campaign).toBe("macfz");
    const lm = ok(await call("GET", "/v1/agent/latest", { token: mac.token }));
    const lw = ok(await call("GET", "/v1/agent/latest", { token: win.token }));
    expect([lm.version, lw.version]).toEqual(["0.3.5", "0.4.0"]);
    const macBuild = await env.DB.prepare(`SELECT platform FROM builds WHERE build_id = ?`).bind(lm.build_id).first<any>();
    expect(macBuild.platform).toBe(MAC);
    // The stats list the latest release per channel and platform.
    const rel = ok(await admin("GET", "/v1/admin/stats")).releases.map((r: any) => [r.channel, r.platform, r.version]);
    expect(rel).toEqual([["stable", MAC, "0.3.5"], ["stable", WIN, "0.4.0"]]);
  });

  it("enroll's 426 uses the platform's releases and points at its download", async () => {
    await release("0.4.0", "0.4.0", { github: true });
    await release("0.3.5", "0.3.5", { platform: MAC, github: true });
    const invite = async (channel?: string) => ok(await admin("POST", "/v1/admin/invites", { channel })).codes[0];
    const body = async (channel?: string) => ({ invite: await invite(channel), name: "pc", profile: MAC_PROFILE });
    const r = err(await call("POST", "/v1/enroll", { agent: "0.3.0", platform: MAC, body: await body() }), 426, "update_required");
    expect([r.min_agent, r.download]).toEqual(["0.3.5", "https://farm.test/download?platform=macos-arm64"]);
    // 0.3.5 is enough on a Mac although Windows needs 0.4.0.
    ok(await call("POST", "/v1/enroll", { agent: "0.3.5", platform: MAC, body: await body() }));
    expect(err(await call("POST", "/v1/enroll", { agent: "0.3.5", body: await body() }), 426, "update_required").download).toBe("https://farm.test/download");
    await release("0.3.6", "0.3.6", { platform: MAC, channel: "canary" });
    const c = err(await call("POST", "/v1/enroll", { agent: "0.3.5", platform: MAC, body: await body("canary") }), 426, "update_required");
    expect(c.download).toBe("https://farm.test/download?platform=macos-arm64&channel=canary");
  });

  it("POST /v1/admin/agent/release: the platform must be the build's; a macOS github_url is its tarball", async () => {
    const macAgent = await makeBuild({ kind: "agent", version: "0.3.0", platform: MAC });
    const winAgent = await makeBuild({ kind: "agent", version: "0.3.0" });
    const post = (b: Record<string, unknown>) => admin("POST", "/v1/admin/agent/release", { version: "0.3.0", channel: "canary", min_agent: "0.3.0", ...b });
    const tar = "https://github.com/greggg230/zero-farm/releases/download/agent-macos-v0.3.0/ZeroFarm-macos-arm64.tar.gz";
    const exe = "https://github.com/greggg230/zero-farm/releases/download/agent-v0.3.0/ZeroFarmSetup.exe";
    expect(err(await post({ build_id: macAgent }), 400, "bad_request").message).toMatch(/macos-arm64 agent, not windows-x64/);
    expect(err(await post({ build_id: winAgent, platform: MAC }), 400, "bad_request").message).toMatch(/windows-x64 agent, not macos-arm64/);
    err(await post({ build_id: macAgent, platform: "linux-x64" }), 400, "bad_request");
    for (const bad of [exe, tar.replace("agent-macos-v", "agent-v"), tar.replace("0.3.0/", "0.3.1/"), tar.replace(".tar.gz", ".zip")]) {
      err(await post({ build_id: macAgent, platform: MAC, github_url: bad }), 400, "bad_request");
    }
    err(await post({ build_id: winAgent, github_url: tar }), 400, "bad_request");
    const r = ok(await post({ build_id: macAgent, platform: MAC, github_url: tar }));
    expect([r.platform, r.github_url]).toEqual([MAC, tar]);
    expect(ok(await post({ build_id: winAgent, github_url: exe })).platform).toBe(WIN);
  });
});

describe("builds per platform (11.3)", () => {
  async function upload(manifest: Record<string, unknown>) {
    const file = `binary ${JSON.stringify(manifest)}`;
    const entry = (manifest.entry as string) ?? "libonnxruntime.dylib";
    const m: Record<string, unknown> = { manifest: 1, created: T0, files: [{ path: entry, sha256: await sha256(file), size: new TextEncoder().encode(file).length }], ...manifest };
    const text = JSON.stringify(m, null, 2);
    const id = (await sha256(text)).slice(0, 24);
    await putArtifact(`builds/${id}/manifest.json`, text);
    await putArtifact(`builds/${id}/manifest.json.minisig`, `untrusted comment: test\nAAAA\ntrusted comment: build_id=${id} kind=${m.kind}\nAAAA\n`);
    await putArtifact(`builds/${id}/${entry}`, file);
    return admin("POST", "/v1/admin/build", { build_id: id });
  }
  const worker = { kind: "worker", version: "w", game: "mtg", aw: 1, profile: "release", entry: "mtgz-worker" };

  it("the manifest's platform is stored; unknown platforms and a min_agent below 0.3.0 are refused", async () => {
    const r = ok(await upload({ ...worker, platform: MAC, min_agent: "0.3.0" }));
    expect(r.platform).toBe(MAC);
    expect((await env.DB.prepare(`SELECT platform FROM builds WHERE build_id = ?`).bind(r.build_id).first<any>()).platform).toBe(MAC);
    expect(ok(await upload({ ...worker, entry: "mtgz-worker.exe" })).platform).toBe(WIN); // no platform: windows-x64
    expect(ok(await upload({ ...worker, entry: "mtgz-worker.exe", platform: WIN, min_agent: "0.3.1" })).platform).toBe(WIN);
    expect(err(await upload({ ...worker, platform: "linux-x64", min_agent: "0.3.0" }), 400, "bad_request").problems.join(" ")).toMatch(/unknown platform/);
    for (const min of [{}, { min_agent: "0.2.14" }]) {
      const e = err(await upload({ ...worker, platform: MAC, ...min }), 400, "bad_request");
      expect(e.problems.join(" "), JSON.stringify(min)).toMatch(/needs min_agent 0.3.0 or newer/);
    }
    expect(ok(await upload({ kind: "runtime", version: "ort-mac", platform: MAC, min_agent: "0.3.0" })).platform).toBe(MAC);
  });
});

describe("diag leases (11.4)", () => {
  it("a request whose build is of another platform than the machine's is refused", async () => {
    const winBuild = await makeRun("r");
    const macBuild = await makeBuild({ tag: "w-mac", platform: MAC });
    const mac = await enrollMac();
    const win = await enroll("win");
    const diag = (body: Record<string, unknown>) => admin("POST", "/v1/admin/diag", { kind: "sweep", ...body });
    expect(err(await diag({ machine_id: win.machine_id, params: { build_id: macBuild } }), 400, "bad_request").message).toMatch(/is for macos-arm64/);
    expect(err(await diag({ machine_id: mac.machine_id, params: { build_id: winBuild } }), 400, "bad_request").message).toMatch(/is for windows-x64/);
    expect(err(await diag({ machine_id: mac.machine_id, params: { run: "r" } }), 400, "bad_request").message).toMatch(/is for windows-x64/);
    expect(err(await diag({ class: "Apple M4 Pro / no gpu", params: { build_id: winBuild } }), 400, "bad_request").message).toMatch(/is for windows-x64/);
    ok(await diag({ machine_id: mac.machine_id, params: { build_id: macBuild } }));
    ok(await diag({ machine_id: win.machine_id, params: { build_id: winBuild } }));
    ok(await admin("POST", "/v1/admin/diag", { machine_id: mac.machine_id, kind: "logs" })); // logs: no build
    // A legacy agent (the only kind that takes diag leases) runs the Windows build.
    const l = await lease(win, { agent: LEGACY, kinds: ["diag"] });
    expect([l.kind, l.build.build_id]).toEqual(["diag", winBuild]);
  });
});

describe("downloads (11.2)", () => {
  const get = (path: string) => call("GET", path, { agent: null });
  const tar = (v: string) => `https://github.com/greggg230/zero-farm/releases/download/agent-macos-v${v}/ZeroFarm-macos-arm64.tar.gz`;
  const exe = (v: string) => `https://github.com/greggg230/zero-farm/releases/download/agent-v${v}/ZeroFarmSetup.exe`;

  it("GET /download?platform=macos-arm64 redirects to the newest macOS tarball; the Windows download is unchanged", async () => {
    err(await get("/download?platform=macos-arm64"), 404, "not_found");
    await release("0.3.0", "0.3.0", { platform: MAC, github: true, channel: "canary" });
    err(await get("/download"), 404, "not_found"); // a macOS release is no Windows installer
    let r = await get("/download?platform=macos-arm64");
    expect([r.status, r.headers.get("location"), r.headers.get("x-farm-channel"), r.headers.get("x-farm-platform")]).toEqual([302, tar("0.3.0"), "canary", MAC]);
    err(await get("/download?platform=macos-arm64&channel=stable"), 404, "not_found");
    await release("0.3.1", "0.3.0", { platform: MAC, github: true });
    await release("0.2.14", "0.2.5", { github: true });
    r = await get("/download?platform=macos-arm64");
    expect([r.status, r.headers.get("location"), r.headers.get("x-farm-channel")]).toEqual([302, tar("0.3.1"), "stable"]);
    expect((await get("/download?channel=canary&platform=macos-arm64")).headers.get("location")).toBe(tar("0.3.0"));
    r = await get("/download");
    expect([r.status, r.headers.get("location"), r.headers.get("x-farm-platform")]).toEqual([302, exe("0.2.14"), WIN]);
    expect((await get("/download?platform=windows-x64")).headers.get("location")).toBe(exe("0.2.14"));
    err(await get("/download?platform=linux-x64"), 400, "bad_request");
  });

  it("GET /install-macos.sh: a POSIX script that downloads, checks and runs the macOS setup", async () => {
    const r = await get("/install-macos.sh");
    expect(r.status).toBe(200);
    expect(r.headers.get("content-type")).toBe("text/plain; charset=utf-8");
    const sh = r.body as string;
    expect(sh.startsWith("#!/bin/sh\n")).toBe(true);
    for (const x of [
      "set -eu",
      `[ "$(uname -m)" = arm64 ] || die`,
      `'https://farm.test/download?platform=macos-arm64'`,
      `'https://github.com/greggg230/zero-farm/releases/download/agent-macos-v'*/ZeroFarm-macos-arm64.tar.gz) ;;`,
      `"$dir/SHA256SUMS"`,
      "shasum -a 256 -c",
      `mktemp -d`,
      `tar -xzf`,
      `./ZeroFarm setup "$@"`,
      "curl -fsSL https://farm.test/install-macos.sh | sh -s -- --invite CODE",
      `main "$@"`,
    ]) {
      expect(sh, x).toContain(x);
    }
    expect(sh).not.toContain("{{");
    expect(sh).not.toContain("\r");
    const canary = (await get("/install-macos.sh?channel=canary")).body as string;
    expect(canary).toContain(`'https://farm.test/download?platform=macos-arm64&channel=canary'`);
    err(await get("/install-macos.sh?channel=beta"), 400, "bad_request");
  });

  it("the onboarding page shows the macOS install command once there is a macOS release", async () => {
    expect((await get("/")).body).not.toContain("install-macos.sh");
    await release("0.3.0", "0.3.0", { platform: MAC, github: true });
    let html = (await get("/")).body as string;
    expect(html).toContain("No Windows download yet");
    expect(html).toContain(`data-copy="curl -fsSL https://farm.test/install-macos.sh | sh -s -- --invite CODE"`);
    expect(html).toContain(`<span class="url">https://farm.test/install-macos.sh</span>`);
    expect(html).toContain("Zero Farm 0.3.0");
    await release("0.2.14", "0.2.5", { github: true });
    html = (await get("/")).body as string;
    expect(html).toContain("Download Zero Farm 0.2.14");
    expect(html).toContain("install-macos.sh | sh -s -- --invite CODE");
  });
});
