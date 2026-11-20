// Rev 11 (PROTOCOL 9): runtime packs per GPU vendor. A run's alternates (a worker build + the pack it requires,
// with a vendor condition) go only to agents from 0.2.10 on machines that qualify; everyone else - CPU-only PCs,
// older agents (stable 0.2.8, canary 0.2.9), fuzz - only ever gets the default runtime. Rev 22 (PROTOCOL 9.8): AMD GPUs
// with DirectML only - an AMD alternate needs a DirectML-only pack and goes to agents from AMD_DML_AGENT; the owner's
// gpu_vendors_off turns a vendor's alternates off.

import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { vendorPackProblem, altProviders, servesVendor, vendorsOffOf } from "../src/routing";
import { PROFILE, admin, advance, ask, call, clock, define, enroll, err, lease, makeBuild, makeCampaign, ok, publishCkpt, runBody } from "./helpers";

const NEW = "0.2.10";
const AMD_AGENT = "0.3.11"; // AMD_DML_AGENT's default (rev 22)
const NV = PROFILE; // an RTX 4070, NVIDIA driver 610.88
const OLD_DRIVER = { ...PROFILE, gpus: [{ vendor: "nvidia", name: "GTX 1080", vram_mb: 8192, driver: "516.94" }] };
const AMD = { ...PROFILE, gpus: [{ vendor: "amd", name: "RX 9070 XT", vram_mb: 16304, driver: "32.0.21001.9024" }], providers: ["dml", "cpu"] };
const CPU_ONLY = { ...PROFILE, gpus: [], providers: ["cpu"] };
const GPU_OK = { gpu: false, gpu_allowed: true };

interface Setup {
  cpuRt: string;
  cuda: string;
  def: string;
  alt: string;
}

// The CPU pack + its worker (the run's default) and the CUDA pack + the same engine signed for it (an alternate).
async function packs(): Promise<Setup> {
  const cpuRt = await makeBuild({ kind: "runtime", tag: "cpu" });
  const cuda = await makeBuild({ kind: "runtime", tag: "cuda", min_agent: NEW, gpu: { vendor: "nvidia", min_driver: "527.41", providers: ["cuda"] } });
  const def = await makeBuild({ tag: "w-cpu", requires_runtime: cpuRt });
  const alt = await makeBuild({ tag: "w-cuda", requires_runtime: cuda, min_agent: NEW });
  return { cpuRt, cuda, def, alt };
}

const altEntry = (s: Setup) => ({ build_id: s.alt, runtime: s.cuda, when: { vendor: "nvidia" } });

// A self-play run (default: CPU pack) with the CUDA alternate, its definition listing both builds.
async function cudaRun(run: string, o: { gpl?: number | "auto"; listAlt?: boolean; evalOnly?: boolean } = {}): Promise<Setup> {
  const s = await packs();
  const body = (extra: Record<string, unknown> = {}) => ({
    ...runBody(s.def, { games_per_lease: o.gpl ?? "auto", selfplay: !o.evalOnly, quota: 100000 }),
    runtime: s.cpuRt,
    alternates: [altEntry(s)],
    ...extra,
  });
  ok(await admin("PUT", `/v1/admin/run/${run}`, body()));
  await publishCkpt(run, 1);
  if (o.evalOnly) {
    await publishCkpt(run, 2);
    const ev = { batch: "b1", games: 400, checkpoints: [2, 1], games_per_lease: "auto", job: { settings: { sims: 32 }, seats: [{ net: 2 }, { net: 1 }], paired: true } };
    ok(await admin("PUT", `/v1/admin/run/${run}`, body({ eval: ev })));
  }
  await define("run", run, (d) => {
    if (o.listAlt !== false) d.builds.push(s.alt);
  });
  return s;
}

const gamesOf = (l: any) => l.job.games as number;

describe("run alternates: per-vendor runtime packs (rev 11)", () => {
  it("an NVIDIA PC with a new agent and GPU work allowed gets the CUDA alternate; its auto lease is big", async () => {
    const s = await cudaRun("cuda-sp");
    const nv = await enroll("nv", { profile: NV });
    const l = await lease(nv, { agent: NEW, caps: GPU_OK, kinds: ["selfplay"] });
    expect([l.kind, l.build.build_id, l.runtime.build_id]).toEqual(["selfplay", s.alt, s.cuda]);
    expect(l.definition.key).toMatch(/^defs\/run\/cuda-sp\//);
    expect(gamesOf(l)).toBeGreaterThanOrEqual(64); // GPU_LEASE_MIN.selfplay
    // the lease row carries the alternate's build
    const run = ok(await admin("GET", "/v1/admin/run/cuda-sp"));
    expect(run.alternates).toEqual([{ ...altEntry(s), problems: [] }]);
  });

  it("evaluation too, at least 128 games per auto lease", async () => {
    const s = await cudaRun("cuda-ev", { evalOnly: true });
    const nv = await enroll("nv", { profile: NV });
    const l = await lease(nv, { agent: NEW, caps: GPU_OK, kinds: ["eval"] });
    expect([l.kind, l.build.build_id, l.runtime.build_id]).toEqual(["eval", s.alt, s.cuda]);
    expect(gamesOf(l)).toBeGreaterThanOrEqual(128);
  });

  it("an AMD PC and a CPU-only PC never get the CUDA pack, however they ask", async () => {
    const s = await cudaRun("cuda-sp");
    for (const [name, profile] of [["amd", AMD], ["cpu", CPU_ONLY]] as const) {
      const m = await enroll(name, { profile });
      for (const caps of [GPU_OK, { gpu: true, gpu_allowed: true }, { gpu: false, gpu_allowed: false }]) {
        const l = await lease(m, { agent: NEW, caps, kinds: ["selfplay"] });
        expect([l.build.build_id, l.runtime.build_id], `${name} ${JSON.stringify(caps)}`).toEqual([s.def, s.cpuRt]);
      }
    }
  });

  it("older agents (stable 0.2.8, canary 0.2.9) only ever see the default runtime, even on an NVIDIA PC", async () => {
    const s = await cudaRun("cuda-sp");
    const nv = await enroll("nv", { profile: NV });
    for (const agent of ["0.2.5", "0.2.8", "0.2.9"]) {
      for (const caps of [GPU_OK, { gpu: true }]) {
        const l = await lease(nv, { agent, caps, kinds: ["selfplay"] });
        expect([l.build.build_id, l.runtime.build_id], agent).toEqual([s.def, s.cpuRt]);
        // and an old agent's auto lease keeps its old size (no GPU minimum: it has no speed choice)
        expect(gamesOf(l), agent).toBe(16);
      }
    }
  });

  it("an NVIDIA PC falls back to the default when a condition fails", async () => {
    const s = await cudaRun("cuda-sp");
    const dflt = [s.def, s.cpuRt];
    // GPU work not allowed now
    const nv = await enroll("nv", { profile: NV });
    let l = await lease(nv, { agent: NEW, caps: { gpu: false, gpu_allowed: false }, kinds: ["selfplay"] });
    expect([l.build.build_id, l.runtime.build_id]).toEqual(dflt);
    // a driver older than the pack's minimum
    const old = await enroll("old", { profile: OLD_DRIVER });
    l = await lease(old, { agent: NEW, caps: GPU_OK, kinds: ["selfplay"] });
    expect([l.build.build_id, l.runtime.build_id]).toEqual(dflt);
    // the pack failed its probe here: an entry without cuda ([] or the CPU only)
    for (const failed of [[], ["cpu"]]) {
      const m = await enroll(`failed${failed.length}`, { profile: { ...NV, runtime_providers: { [s.cuda]: failed } } });
      l = await lease(m, { agent: NEW, caps: GPU_OK, kinds: ["selfplay"] });
      expect([l.build.build_id, l.runtime.build_id], JSON.stringify(failed)).toEqual(dflt);
    }
    // ... and once it passed, it keeps getting it
    const passed = await enroll("passed", { profile: { ...NV, runtime_providers: { [s.cuda]: ["cuda", "cpu"] } } });
    expect((await lease(passed, { agent: NEW, caps: GPU_OK, kinds: ["selfplay"] })).runtime.build_id).toBe(s.cuda);
    // no disk room for the alternate (the default's files are cached)
    const full = await enroll("full", { profile: NV });
    l = await lease(full, { agent: NEW, caps: { ...GPU_OK, disk_mb: 0 }, cached: { builds: [s.def, s.cpuRt] }, kinds: ["selfplay"] });
    expect([l.build.build_id, l.runtime.build_id]).toEqual(dflt);
    // the job's provider allowlist excludes cuda
    ok(await admin("PUT", "/v1/admin/run/cuda-sp", { ...runBody(s.def, { games_per_lease: "auto" }), runtime: s.cpuRt, alternates: [altEntry(s)], selfplay: { ...runBody(s.def).selfplay, quota: 100000, games_per_lease: "auto", providers: ["dml", "cpu"] } }));
    l = await lease(nv, { agent: NEW, caps: GPU_OK, kinds: ["selfplay"] });
    expect([l.build.build_id, l.runtime.build_id]).toEqual(dflt);
  });

  it("an alternate the definition doesn't list is never leased; the admin view says why", async () => {
    const s = await cudaRun("cuda-sp", { listAlt: false });
    const nv = await enroll("nv", { profile: NV });
    const l = await lease(nv, { agent: NEW, caps: GPU_OK, kinds: ["selfplay"] });
    expect([l.build.build_id, l.runtime.build_id]).toEqual([s.def, s.cpuRt]);
    const run = ok(await admin("GET", "/v1/admin/run/cuda-sp"));
    expect(run.alternates[0].problems.join(" ")).toMatch(/is not in definition/);
  });

  it("fuzz never carries a runtime pack, on an NVIDIA PC either", async () => {
    await cudaRun("cuda-sp");
    await makeCampaign("fz", { priority: 10 });
    const nv = await enroll("nv", { profile: NV });
    const l = await lease(nv, { agent: NEW, caps: { gpu: true, gpu_allowed: true }, kinds: ["fuzz"] });
    expect([l.kind, l.runtime]).toEqual(["fuzz", null]);
  });

  it("a run whose default pack needs an NVIDIA GPU: nothing for AMD PCs or older agents", async () => {
    const s = await packs();
    ok(await admin("PUT", "/v1/admin/run/nv-only", { ...runBody(s.alt, { games_per_lease: 5 }), runtime: s.cuda }));
    await publishCkpt("nv-only", 1);
    await define("run", "nv-only");
    const amd = await enroll("amd", { profile: AMD });
    expect(ok(await ask(amd, { agent: NEW, caps: GPU_OK, kinds: ["selfplay"] })).reason).toBe("no_work");
    const nv = await enroll("nv", { profile: NV });
    expect(ok(await ask(nv, { agent: "0.2.9", caps: GPU_OK, kinds: ["selfplay"] })).reason).toBe("no_work");
    const l = await lease(nv, { agent: NEW, caps: GPU_OK, kinds: ["selfplay"] });
    expect(l.runtime.build_id).toBe(s.cuda);
  });

  it("a run without alternates is leased exactly as before (old and new agents)", async () => {
    const s = await packs();
    ok(await admin("PUT", "/v1/admin/run/plain", { ...runBody(s.def, { games_per_lease: 6 }), runtime: s.cpuRt }));
    await publishCkpt("plain", 1);
    await define("run", "plain");
    const nv = await enroll("nv", { profile: NV });
    for (const agent of ["0.2.8", "0.2.9", NEW]) {
      const l = await lease(nv, { agent, caps: { gpu: true, gpu_allowed: true }, kinds: ["selfplay"] });
      expect([l.build.build_id, l.runtime.build_id, gamesOf(l)], agent).toEqual([s.def, s.cpuRt, 6]);
    }
    expect(ok(await admin("GET", "/v1/admin/run/plain")).alternates).toBeNull();
  });

  it("PUT checks each alternate against the registered builds", async () => {
    const s = await packs();
    const put = (alternates: unknown) => admin("PUT", "/v1/admin/run/r", { ...runBody(s.def), runtime: s.cpuRt, alternates });
    // the worker build must require exactly that pack
    expect(err(await put([{ build_id: s.def, runtime: s.cuda, when: { vendor: "nvidia" } }]), 400, "bad_request").message).toMatch(/requires runtime/);
    // a pack without its own gpu needs a vendor; a vendor must match the pack's
    const winml = await makeBuild({ kind: "runtime", tag: "winml" });
    const wml = await makeBuild({ tag: "w-winml", requires_runtime: winml });
    expect(err(await put([{ build_id: wml, runtime: winml }]), 400, "bad_request").message).toMatch(/when.vendor is required/);
    expect(err(await put([{ build_id: s.alt, runtime: s.cuda, when: { vendor: "amd" } }]), 400, "bad_request").message).toMatch(/for nvidia GPUs/);
    // the default can't be an alternate too; at most 4; unknown fields refused
    expect(err(await put([{ build_id: s.def, runtime: s.cpuRt, when: { vendor: "nvidia" } }]), 400, "bad_request").message).toMatch(/default/);
    err(await put([1, 2, 3, 4, 5].map(() => altEntry(s))), 400, "bad_request");
    err(await put([{ ...altEntry(s), extra: 1 }]), 400, "bad_request");
    // the vendor defaults to the pack's; a pack without its own gpu takes it spelled out (rev 22: amd needs its own gpu)
    const intel = await makeBuild({ kind: "runtime", tag: "intel" });
    const wi = await makeBuild({ tag: "w-intel", requires_runtime: intel });
    ok(await put([{ build_id: s.alt, runtime: s.cuda }, { build_id: wi, runtime: intel, when: { vendor: "intel" } }]));
    const run = ok(await admin("GET", "/v1/admin/run/r"));
    expect(run.alternates.map((a: any) => a.when.vendor)).toEqual(["nvidia", "intel"]);
    // a PUT without alternates clears them (a PUT is the whole configuration)
    ok(await admin("PUT", "/v1/admin/run/r", { ...runBody(s.def), runtime: s.cpuRt }));
    expect(ok(await admin("GET", "/v1/admin/run/r")).alternates).toBeNull();
  });

  it("rev 22: AMD alternates only with a DirectML-only pack: PUT refuses any other, and one stored before is never offered", async () => {
    const s = await packs();
    const body = (alternates: unknown[]) => ({ ...runBody(s.def, { games_per_lease: "auto", quota: 100000 }), runtime: s.cpuRt, alternates });
    const alt = async (tag: string, o: { gpu?: { vendor: string; providers: string[] }; files?: string[] }) => {
      const rt = await makeBuild({ kind: "runtime", tag, min_agent: NEW, ...(o.gpu ? { gpu: o.gpu } : {}), extra_files: ["DirectML.dll", ...(o.files ?? [])] });
      return { rt, w: await makeBuild({ tag: `w-${tag}`, requires_runtime: rt, min_agent: NEW }) };
    };
    // a pack without its own gpu requirement, one that brings another provider, AMD's own providers, Windows ML's catalog
    const bare = await alt("bare", {});
    const extra = await alt("dml-migraphx", { gpu: { vendor: "amd", providers: ["dml", "migraphx"] } });
    const own = await alt("amdgpu", { gpu: { vendor: "amd", providers: ["amdgpu"] } });
    const cat = await alt("catalog", { gpu: { vendor: "amd", providers: ["dml"] }, files: ["Microsoft.Windows.AI.MachineLearning.dll"] });
    expect(err(await admin("PUT", "/v1/admin/run/mixed", body([{ build_id: bare.w, runtime: bare.rt, when: { vendor: "amd" } }])), 400, "bad_request").message).toMatch(/needs a gpu requirement naming amd/);
    expect(err(await admin("PUT", "/v1/admin/run/mixed", body([{ build_id: extra.w, runtime: extra.rt }])), 400, "bad_request").message).toMatch(/run dml only, but the runtime brings/);
    expect(err(await admin("PUT", "/v1/admin/run/mixed", body([{ build_id: own.w, runtime: own.rt }])), 400, "bad_request").message).toMatch(/run dml only/);
    expect(err(await admin("PUT", "/v1/admin/run/mixed", body([{ build_id: cat.w, runtime: cat.rt }])), 400, "bad_request").message).toMatch(/Microsoft\.Windows\.AI\.MachineLearning\.dll/);
    // the DirectML-only pack: accepted, its vendor taken from the pack, shown with its rule
    const dml = await alt("dml", { gpu: { vendor: "amd", providers: ["dml"] } });
    ok(await admin("PUT", "/v1/admin/run/mixed", body([altEntry(s), { build_id: dml.w, runtime: dml.rt }])));
    await publishCkpt("mixed", 1);
    await define("run", "mixed", (d) => d.builds.push(s.alt, bare.w, dml.w));
    const run = ok(await admin("GET", "/v1/admin/run/mixed"));
    expect(run.alternates[1]).toMatchObject({ build_id: dml.w, runtime: dml.rt, when: { vendor: "amd" }, problems: [] });
    expect(run.alternates[1].gpu_notes[0]).toMatch(/amd GPUs run dml only; offered to agents from 0\.3\.11/);
    expect(run.alternates[0].gpu_notes).toBeUndefined();
    // an AMD alternate stored by an older coordinator (a Windows ML pack without gpu): shown as not offered, never offered
    const amdOld = { build_id: bare.w, runtime: bare.rt, when: { vendor: "amd" } };
    await env.DB.prepare(`UPDATE runs SET alternates = ? WHERE run = 'mixed'`).bind(JSON.stringify([altEntry(s), amdOld])).run();
    expect(ok(await admin("GET", "/v1/admin/run/mixed")).alternates[1].problems.join(" ")).toMatch(/not offered: amd GPUs run dml only/);
    const amd = await enroll("amd", { profile: AMD });
    expect((await lease(amd, { agent: AMD_AGENT, caps: GPU_OK, kinds: ["selfplay"] })).runtime.build_id).toBe(s.cpuRt);
  });

  it("rev 22: an AMD PC with an agent from AMD_DML_AGENT gets the DirectML alternate; older agents, NVIDIA and CPU PCs don't", async () => {
    const s = await packs();
    const rt = await makeBuild({ kind: "runtime", tag: "dml", min_agent: NEW, gpu: { vendor: "amd", providers: ["dml"] }, extra_files: ["DirectML.dll"] });
    const w = await makeBuild({ tag: "w-dml", requires_runtime: rt, min_agent: NEW });
    const amdAlt = { build_id: w, runtime: rt };
    ok(await admin("PUT", "/v1/admin/run/mixed", { ...runBody(s.def, { games_per_lease: "auto", quota: 100000 }), runtime: s.cpuRt, alternates: [altEntry(s), amdAlt] }));
    await publishCkpt("mixed", 1);
    await define("run", "mixed", (d) => d.builds.push(s.alt, w));
    const amd = await enroll("amd", { profile: AMD });
    const nv = await enroll("nv", { profile: NV });
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    const l = await lease(amd, { agent: AMD_AGENT, caps: GPU_OK, kinds: ["selfplay"] });
    expect([l.build.build_id, l.runtime.build_id]).toEqual([w, rt]);
    expect(gamesOf(l)).toBeGreaterThanOrEqual(64); // a GPU lease's size
    // an agent before AMD_DML_AGENT (it never reports AMD adapters, but the coordinator gates it too): the default
    expect((await lease(amd, { agent: "0.3.10", caps: GPU_OK, kinds: ["selfplay"] })).runtime.build_id).toBe(s.cpuRt);
    // GPU work not allowed right now: the default
    expect((await lease(amd, { agent: AMD_AGENT, caps: { gpu: false, gpu_allowed: false }, kinds: ["selfplay"] })).runtime.build_id).toBe(s.cpuRt);
    // the AMD pack failed its probe there (no DirectML in runtime_providers): never offered again for those drivers
    const failed = { ...AMD, runtime_providers: { [rt]: [] } };
    const amd2 = await enroll("amd2", { profile: failed });
    expect((await lease(amd2, { agent: AMD_AGENT, caps: GPU_OK, kinds: ["selfplay"] })).runtime.build_id).toBe(s.cpuRt);
    // an NVIDIA PC gets CUDA, a CPU-only PC the default
    expect((await lease(nv, { agent: AMD_AGENT, caps: GPU_OK, kinds: ["selfplay"] })).runtime.build_id).toBe(s.cuda);
    expect((await lease(cpu, { agent: AMD_AGENT, caps: GPU_OK, kinds: ["selfplay"] })).runtime.build_id).toBe(s.cpuRt);
    // an AMD + NVIDIA PC takes the first alternate it qualifies for (CUDA, listed first)
    const both = await enroll("both", { profile: { ...NV, gpus: [...NV.gpus, AMD.gpus[0]] } });
    expect((await lease(both, { agent: AMD_AGENT, caps: GPU_OK, kinds: ["selfplay"] })).runtime.build_id).toBe(s.cuda);
  });

  it("rev 22: gpu_vendors_off turns a vendor's alternates off, rides on /v1/agent/latest, and is lifted", async () => {
    const s = await packs();
    const rt = await makeBuild({ kind: "runtime", tag: "dml", min_agent: NEW, gpu: { vendor: "amd", providers: ["dml"] }, extra_files: ["DirectML.dll"] });
    const w = await makeBuild({ tag: "w-dml", requires_runtime: rt, min_agent: NEW });
    ok(await admin("PUT", "/v1/admin/run/mixed", { ...runBody(s.def, { games_per_lease: "auto", quota: 100000 }), runtime: s.cpuRt, alternates: [altEntry(s), { build_id: w, runtime: rt }] }));
    await publishCkpt("mixed", 1);
    await define("run", "mixed", (d) => d.builds.push(s.alt, w));
    const amd = await enroll("amd", { profile: AMD });
    const nv = await enroll("nv", { profile: NV });
    expect(ok(await admin("POST", "/v1/admin/gpu_vendors_off", { vendors: ["amd", "amd"] })).vendors).toEqual(["amd"]);
    expect(ok(await admin("GET", "/v1/admin/gpu_vendors_off")).vendors).toEqual(["amd"]);
    expect((await lease(amd, { agent: AMD_AGENT, caps: GPU_OK, kinds: ["selfplay"] })).runtime.build_id).toBe(s.cpuRt);
    expect((await lease(nv, { agent: AMD_AGENT, caps: GPU_OK, kinds: ["selfplay"] })).runtime.build_id).toBe(s.cuda);
    const run = ok(await admin("GET", "/v1/admin/run/mixed"));
    expect(run.alternates[1].problems).toEqual(["GPU work on amd GPUs is off (gpu_vendors_off): not offered"]);
    expect(run.alternates[0].problems).toEqual([]);
    const stats = ok(await admin("GET", "/v1/admin/stats"));
    expect(stats.gpu_vendors_off).toEqual(["amd"]);
    expect(stats.limits.amd_dml_agent).toBe("0.3.11");
    // any vendor: NVIDIA off sends the NVIDIA PC to the default too
    ok(await admin("POST", "/v1/admin/gpu_vendors_off", { vendors: ["nvidia"] }));
    expect((await lease(nv, { agent: AMD_AGENT, caps: GPU_OK, kinds: ["selfplay"] })).runtime.build_id).toBe(s.cpuRt);
    expect((await lease(amd, { agent: AMD_AGENT, caps: GPU_OK, kinds: ["selfplay"] })).runtime.build_id).toBe(rt);
    // lifted
    ok(await admin("POST", "/v1/admin/gpu_vendors_off", { vendors: [] }));
    expect(ok(await admin("GET", "/v1/admin/gpu_vendors_off")).vendors).toEqual([]);
    expect((await lease(nv, { agent: AMD_AGENT, caps: GPU_OK, kinds: ["selfplay"] })).runtime.build_id).toBe(s.cuda);
    // checks: a list of vendor names, at most 8, admin only
    err(await admin("POST", "/v1/admin/gpu_vendors_off", { vendors: "amd" }), 400, "bad_request");
    err(await admin("POST", "/v1/admin/gpu_vendors_off", { vendors: ["AMD!"] }), 400, "bad_request");
    err(await admin("POST", "/v1/admin/gpu_vendors_off", { vendors: Array.from({ length: 9 }, (_, i) => `v${i}`) }), 400, "bad_request");
    err(await admin("POST", "/v1/admin/gpu_vendors_off", { vendors: [], extra: 1 }), 400, "bad_request");
    err(await call("POST", "/v1/admin/gpu_vendors_off", { token: amd.token, body: { vendors: [] } }), 401, "unauthorized");
  });

  it("rev 22: the vendor rules (unit)", () => {
    const m = (gpu: unknown, files: string[] = ["onnxruntime.dll", "DirectML.dll"]) => JSON.stringify({ kind: "runtime", gpu, files: files.map((path) => ({ path })) });
    expect(vendorPackProblem("amd", m({ vendor: "amd", providers: ["dml"] }))).toBeNull();
    expect(vendorPackProblem("amd", m(undefined))).toMatch(/needs a gpu requirement/);
    expect(vendorPackProblem("amd", m({ vendor: "nvidia", providers: ["dml"] }))).toMatch(/for nvidia GPUs/);
    expect(vendorPackProblem("amd", m({ vendor: "amd", providers: ["dml", "dml"] }))).toMatch(/brings/);
    expect(vendorPackProblem("amd", m({ vendor: "amd", providers: ["dml"] }, ["winml/microsoft.windows.ai.machinelearning.dll"]))).toMatch(/ships/);
    // no rule for other vendors: an NVIDIA DirectML-only pack, the CUDA pack, Windows ML's pack keep working as before
    expect(vendorPackProblem("nvidia", m({ vendor: "nvidia", providers: ["dml"] }))).toBeNull();
    expect(vendorPackProblem("nvidia", m(undefined, ["Microsoft.Windows.AI.MachineLearning.dll"]))).toBeNull();
    // dml is vendor-neutral; AMD's own providers serve no GPU of the farm's
    expect(servesVendor("dml", "amd") && servesVendor("dml", "nvidia") && servesVendor("dml", "intel")).toBe(true);
    expect(servesVendor("migraphx", "amd") || servesVendor("amdgpu", "amd") || servesVendor("cuda", "amd")).toBe(false);
    expect(altProviders(null, null, "amd")).toEqual(["dml"]);
    expect(altProviders(null, null, "nvidia")).toEqual(["tensorrt_rtx", "cuda", "dml"]);
    expect(altProviders(null, ["cuda", "openvino"], "intel")).toEqual(["openvino"]);
    expect(altProviders({ vendor: "amd", min_driver: null, providers: ["dml"] }, ["cpu"], "amd")).toEqual([]);
    expect([...vendorsOffOf('["amd","Bad!",3]')]).toEqual(["amd"]);
    expect(vendorsOffOf("not json").size).toBe(0);
    expect(vendorsOffOf(null).size).toBe(0);
  });
});

describe("GPU lease sizes (rev 11, PROTOCOL 9.4)", () => {
  // A self-play run like the Delver one: 6 games per lease for CPU PCs, ~3-minute iterations.
  async function delverLike(o: { gpu_gpl?: number | "auto" } = {}) {
    const s = await packs();
    const sp = { ...runBody(s.def).selfplay!, quota: 1000, games_per_lease: 6, max_stale: 1, ...(o.gpu_gpl !== undefined ? { gpu_games_per_lease: o.gpu_gpl } : {}) };
    ok(await admin("PUT", "/v1/admin/run/bo3", { ...runBody(s.def), runtime: s.cpuRt, alternates: [altEntry(s)], selfplay: sp }));
    for (let g = 1; g <= 4; g++) {
      await publishCkpt("bo3", g);
      advance(180);
    }
    await define("run", "bo3", (d) => d.builds.push(s.alt));
    return s;
  }

  it("without gpu_games_per_lease a configured size is kept for GPU leases too (the live run's case)", async () => {
    const s = await delverLike();
    const nv = await enroll("nv", { profile: NV });
    const l = await lease(nv, { agent: NEW, caps: { ...GPU_OK, gpu_games: { bo3: 40 } }, kinds: ["selfplay"] });
    expect([l.runtime.build_id, gamesOf(l)]).toEqual([s.cuda, 6]);
  });

  it("a number for GPU leases, the run's own size for everyone else", async () => {
    const s = await delverLike({ gpu_gpl: 24 });
    const nv = await enroll("nv", { profile: NV });
    const amd = await enroll("amd", { profile: AMD });
    expect(gamesOf(await lease(nv, { agent: NEW, caps: GPU_OK, kinds: ["selfplay"] }))).toBe(24);
    expect(gamesOf(await lease(amd, { agent: NEW, caps: GPU_OK, kinds: ["selfplay"] }))).toBe(6);
    expect(gamesOf(await lease(nv, { agent: "0.2.9", caps: GPU_OK, kinds: ["selfplay"] }))).toBe(6);
    expect(ok(await admin("GET", "/v1/admin/run/bo3")).selfplay.gpu_games_per_lease).toBe(24);
    void s;
  });

  it("auto: the machine's measured break-even, never past the staleness window", async () => {
    await delverLike({ gpu_gpl: "auto" });
    const nv = await enroll("nv", { profile: NV });
    // the machine measured that no GPU pays for this network (0): the normal size
    expect(gamesOf(await lease(nv, { agent: NEW, caps: { ...GPU_OK, gpu_games: { bo3: 0 } }, kinds: ["selfplay"] }))).toBe(6);
    // no rate measured yet: the agent's hint (or 64 before any bench)
    expect(gamesOf(await lease(nv, { agent: NEW, caps: { ...GPU_OK, gpu_games: { bo3: 40 } }, kinds: ["selfplay"] }))).toBe(40);
    expect(gamesOf(await lease(nv, { agent: NEW, caps: GPU_OK, kinds: ["selfplay"] }))).toBe(64);
    // a measured rate of 0.1 games/s and 180 s iterations, max_stale 1: at most 0.8 x 0.1 x 180 = 14 games
    await env.DB.prepare(`INSERT OR REPLACE INTO rates (machine_id, kind, scope, rate, samples, t) VALUES (?, 'selfplay', 'run:bo3', 0.1, 3, ?)`)
      .bind(nv.machine_id, clock.now)
      .run();
    // the GPU needs 40 games to pay: they don't fit, so the normal 6 (the agent then runs the CPU)
    expect(gamesOf(await lease(nv, { agent: NEW, caps: { ...GPU_OK, gpu_games: { bo3: 40 } }, kinds: ["selfplay"] }))).toBe(6);
    // it pays from 10 games: 10
    expect(gamesOf(await lease(nv, { agent: NEW, caps: { ...GPU_OK, gpu_games: { bo3: 10 } }, kinds: ["selfplay"] }))).toBe(10);
  });

  it("caps.gpu_games is checked", async () => {
    await delverLike();
    const nv = await enroll("nv", { profile: NV });
    err(await ask(nv, { agent: NEW, caps: { ...GPU_OK, gpu_games: { "Not A Run": 3 } } as any }), 400, "bad_request");
    err(await ask(nv, { agent: NEW, caps: { ...GPU_OK, gpu_games: { bo3: -1 } } as any }), 400, "bad_request");
  });
});
