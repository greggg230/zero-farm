// Rev 7 capability routing (PROTOCOL 8): run and campaign `gpu` / `providers`, the profile's
// probed providers (`providers`, `runtime_providers`, GPU `luid`), GPU eligibility per runtime
// pack, required / preferred / any, evaluation before self-play, and the lease's `providers`.

import { describe, expect, it } from "vitest";
import { gpuEligible, kindRank, place, providersOf, routingOf, verifiedProviders } from "../src/routing";
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
  runBody,
} from "./helpers";

// Profiles: an NVIDIA PC that probed TensorRT-RTX and DirectML, an AMD PC (MIGraphX failed its
// self-test there, the AMD GPU provider and DirectML passed), and a CPU-only PC.
const NVIDIA = { ...PROFILE, gpus: [{ vendor: "nvidia", name: "RTX 4070", vram_mb: 12282, driver: "610.88", luid: "1a2b" }], providers: ["tensorrt_rtx", "dml", "cpu"] };
const AMD = { ...PROFILE, gpus: [{ vendor: "amd", name: "RX 7800 XT", vram_mb: 16368, driver: "25.10", luid: "c0ffee" }], providers: ["amdgpu", "dml", "cpu"] };
const CPU_ONLY = { ...PROFILE, gpus: [], providers: ["cpu"] };
const GPU_CAPS = { caps: { gpu: true } };

// A self-play run with these routing settings (and the selfplay JSON's other fields as runBody's).
async function routedRun(run: string, routing: Record<string, unknown>, o: { runtime?: string | null; priority?: number; eval?: unknown } = {}) {
  const build = await makeBuild({ tag: run });
  const body: any = runBody(build, { priority: o.priority });
  body.runtime = o.runtime ?? null;
  body.selfplay = { ...body.selfplay, ...routing };
  ok(await admin("PUT", `/v1/admin/run/${run}`, body));
  await publishCkpt(run, 1);
  if (o.eval) ok(await admin("PUT", `/v1/admin/run/${run}`, { ...body, eval: o.eval }));
  await define("run", run);
  return build;
}

async function routedCampaign(id: string, routing: Record<string, unknown>, priority = 0.2) {
  const build = await makeBuild({ tag: `fuzz-${id}` });
  ok(
    await admin("PUT", `/v1/admin/campaign/${id}`, {
      build_id: build,
      mode: "delver",
      env: { FUZZ_POOL: "delver" },
      seed_base: 10_000_000,
      chunk: 1000,
      priority,
      ...routing,
    }),
  );
  await define("campaign", id);
}

describe("routing functions", () => {
  it("validates allowlists and routing settings", () => {
    expect(providersOf(undefined, "p")).toBeNull();
    expect(providersOf(null, "p")).toBeNull();
    expect(providersOf(["migraphx", "dml", "cpu"], "p")).toEqual(["migraphx", "dml", "cpu"]);
    for (const bad of [[], ["metal"], ["dml", "dml"], "dml", [1]]) expect(() => providersOf(bad, "p")).toThrow();
    expect(routingOf("required", ["cuda"], "x.")).toEqual({ gpu: "required", providers: ["cuda"] });
    expect(routingOf("any", ["cpu"], "x.")).toEqual({ gpu: "any", providers: ["cpu"] }); // CPU inference only, on any PC
    expect(() => routingOf("preferred", ["cpu"], "x.")).toThrow(/no GPU provider/); // no machine could be eligible
  });

  it("reads the probe results for the job's runtime pack, else the union", () => {
    const rt = "ab".repeat(12);
    const p = { providers: ["tensorrt_rtx", "dml", "cpu"], runtime_providers: { [rt]: ["dml", "cpu"] } };
    expect(verifiedProviders(p, rt)).toEqual(["dml", "cpu"]);
    expect(verifiedProviders(p, "cd".repeat(12))).toEqual(["tensorrt_rtx", "dml", "cpu"]);
    expect(verifiedProviders(p, null)).toEqual(["tensorrt_rtx", "dml", "cpu"]);
    expect(verifiedProviders(null, rt)).toEqual([]);
    expect(verifiedProviders({ providers: "junk" }, null)).toEqual([]);
  });

  it("GPU eligibility: caps.gpu, a known GPU provider, inside the allowlist", () => {
    const amd = ["amdgpu", "dml", "cpu"];
    expect(gpuEligible(true, amd, null)).toBe(true);
    expect(gpuEligible(false, amd, null)).toBe(false); // the agent may not use the GPU now
    expect(gpuEligible(true, ["cpu"], null)).toBe(false);
    expect(gpuEligible(true, ["metal", "cpu"], null)).toBe(false); // unknown names never count
    expect(gpuEligible(true, amd, ["tensorrt_rtx"])).toBe(false); // vendor mismatch
    expect(gpuEligible(true, amd, ["tensorrt_rtx", "dml"])).toBe(true);
    expect(gpuEligible(true, amd, ["cpu"])).toBe(false);
  });

  it("places jobs by gpu mode; evaluation ranks before self-play", () => {
    const any = { gpu: "any" as const, providers: null };
    const pref = { gpu: "preferred" as const, providers: null };
    const req = { gpu: "required" as const, providers: null };
    expect(place("selfplay", any, false, false)).toEqual({ tier: 1, fit: 1 });
    expect(place("eval", pref, true, true)).toEqual({ tier: 1, fit: 1 });
    expect(place("eval", pref, false, true)).toEqual({ tier: 2, fit: 0.25 });
    expect(place("selfplay", req, false, true)).toBeNull();
    expect(place("selfplay", req, true, true)).toEqual({ tier: 1, fit: 1 });
    expect(place("fuzz", any, false, true)).toEqual({ tier: 2, fit: 0.5 }); // as before rev 7
    expect(place("fuzz", any, false, false)).toEqual({ tier: 2, fit: 1 });
    expect(place("fuzz", pref, true, true)).toEqual({ tier: 2, fit: 1 });
    expect(place("fuzz", pref, false, false)).toEqual({ tier: 2, fit: 0.25 });
    expect(place("fuzz", req, false, true)).toBeNull();
    expect(kindRank("eval")).toBeLessThan(kindRank("selfplay"));
    expect(kindRank("selfplay")).toBe(kindRank("fuzz"));
  });
});

describe("profile (rev 7)", () => {
  it("accepts adapter LUIDs and per-runtime provider lists; checks their shape", async () => {
    const rt = "0123456789abcdef01234567";
    // Unknown provider names are kept (a newer agent may know more).
    await enroll("ok", { profile: { ...NVIDIA, providers: ["future_ep", "dml", "cpu"], runtime_providers: { [rt]: ["future_ep", "cpu"] } } });
    const enrollWith = async (profile: object) => {
      const { codes } = ok(await admin("POST", "/v1/admin/invites", { count: 1 }));
      return call("POST", "/v1/enroll", { body: { invite: codes[0], name: "x", profile } });
    };
    const gpu = (luid: unknown) => ({ ...PROFILE, gpus: [{ vendor: "amd", name: "RX", vram_mb: 1, driver: "1", luid }] });
    for (const luid of ["C0FFEE", "", "0x1a", "1".repeat(17), 42]) err(await enrollWith(gpu(luid)), 400, "bad_request");
    ok(await enrollWith(gpu("0".repeat(16))));
    for (const runtime_providers of [
      ["cpu"],
      { "not-a-build": ["cpu"] },
      { [rt.toUpperCase()]: ["cpu"] },
      { [rt]: "cpu" },
      { [rt]: Array.from({ length: 17 }, (_, i) => `p${i}`) },
      Object.fromEntries(Array.from({ length: 17 }, (_, i) => [i.toString(16).padStart(24, "0"), ["cpu"]])),
    ]) {
      err(await enrollWith({ ...PROFILE, runtime_providers }), 400, "bad_request");
    }
    ok(await enrollWith({ ...PROFILE, runtime_providers: null }));
    ok(await enrollWith({ ...PROFILE, runtime_providers: Object.fromEntries(Array.from({ length: 16 }, (_, i) => [i.toString(16).padStart(24, "0"), ["dml", "cpu"]])) }));
  });

  it("the admin page data shows each machine's providers and LUIDs", async () => {
    await enroll("nv", { profile: NVIDIA });
    await enroll("old", { profile: { ...PROFILE, providers: ["cpu"] } });
    const s = ok(await admin("GET", "/v1/admin/stats"));
    const nv = s.machines.find((m: any) => m.name === "nv");
    expect(nv).toMatchObject({ providers: ["tensorrt_rtx", "dml", "cpu"], luids: ["1a2b"] });
    expect(s.machines.find((m: any) => m.name === "old")).toMatchObject({ providers: ["cpu"], luids: [] });
    expect((await call("GET", "/admin")).body as string).toContain("LUIDs");
  });
});

describe("run and campaign config (rev 7)", () => {
  it("run selfplay/eval take a providers allowlist; GET returns it", async () => {
    const build = await makeBuild();
    const put = (selfplay: Record<string, unknown>) => admin("PUT", "/v1/admin/run/r", { ...runBody(build), selfplay: { ...runBody(build).selfplay, ...selfplay } });
    for (const providers of [[], ["metal"], ["dml", "dml"], "dml", [3]]) err(await put({ providers }), 400, "bad_request");
    err(await put({ gpu: "required", providers: ["cpu"] }), 400, "bad_request");
    ok(await put({ gpu: "required", providers: ["migraphx", "amdgpu", "dml"] }));
    let r = ok(await admin("GET", "/v1/admin/run/r"));
    expect(r.selfplay).toMatchObject({ gpu: "required", providers: ["migraphx", "amdgpu", "dml"] });
    ok(await put({ providers: null }));
    r = ok(await admin("GET", "/v1/admin/run/r"));
    expect(r.selfplay).toMatchObject({ gpu: "any", providers: null });
    // eval too (not part of the batch's identity: changing it keeps the batch).
    await publishCkpt("r", 1);
    const ev = { batch: "b1", games: 4, checkpoints: [1], job: { seats: [{ net: 1 }, { bot: "heuristic" }] } };
    err(await admin("PUT", "/v1/admin/run/r", { ...runBody(build), eval: { ...ev, providers: ["vulkan"] } }), 400, "bad_request");
    ok(await admin("PUT", "/v1/admin/run/r", { ...runBody(build), eval: { ...ev, gpu: "preferred", providers: ["tensorrt_rtx"] } }));
    ok(await admin("PUT", "/v1/admin/run/r", { ...runBody(build), eval: { ...ev, gpu: "preferred", providers: ["tensorrt_rtx", "cuda"] } }));
    r = ok(await admin("GET", "/v1/admin/run/r"));
    expect(r.eval).toMatchObject({ batch: "b1", gpu: "preferred", providers: ["tensorrt_rtx", "cuda"] });
  });

  it("campaigns take gpu and providers (default any, null); GET returns them", async () => {
    const build = await makeBuild({ tag: "fz" });
    const base = { build_id: build, mode: "delver", env: {}, seed_base: 0, chunk: 1000 };
    ok(await admin("PUT", "/v1/admin/campaign/fz", base));
    expect(ok(await admin("GET", "/v1/admin/campaign/fz"))).toMatchObject({ gpu: "any", providers: null });
    err(await admin("PUT", "/v1/admin/campaign/fz", { ...base, gpu: "maybe" }), 400, "bad_request");
    err(await admin("PUT", "/v1/admin/campaign/fz", { ...base, providers: [] }), 400, "bad_request");
    err(await admin("PUT", "/v1/admin/campaign/fz", { ...base, providers: ["openvino", "openvino"] }), 400, "bad_request");
    err(await admin("PUT", "/v1/admin/campaign/fz", { ...base, gpu: "required", providers: ["cpu"] }), 400, "bad_request");
    ok(await admin("PUT", "/v1/admin/campaign/fz", { ...base, gpu: "preferred", providers: ["openvino", "dml"] }));
    expect(ok(await admin("GET", "/v1/admin/campaign/fz"))).toMatchObject({ gpu: "preferred", providers: ["openvino", "dml"] });
    ok(await admin("PUT", "/v1/admin/campaign/fz", base)); // a put without them resets to the defaults
    expect(ok(await admin("GET", "/v1/admin/campaign/fz"))).toMatchObject({ gpu: "any", providers: null });
  });
});

describe("routing leases (rev 7)", () => {
  it("required: only GPU-eligible machines, judged by verified providers, not caps.gpu alone", async () => {
    await routedRun("r", { gpu: "required" });
    const nv = await enroll("nv", { profile: NVIDIA });
    const amd = await enroll("amd", { profile: AMD });
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    const old = await enroll("old", { profile: { ...PROFILE, providers: ["cpu"] } }); // today's agents report only cpu
    expect(ok(await ask(cpu, GPU_CAPS)).reason).toBe("no_work"); // caps.gpu without a verified GPU provider
    expect(ok(await ask(old, GPU_CAPS)).reason).toBe("no_work");
    expect(ok(await ask(nv)).reason).toBe("no_work"); // a verified GPU, but the agent may not use it now
    expect((await lease(nv, GPU_CAPS)).run).toBe("r");
    expect((await lease(amd, GPU_CAPS)).run).toBe("r");
  });

  it("an allowlist routes by provider: tensorrt_rtx excludes an AMD PC", async () => {
    await routedRun("nv-only", { gpu: "required", providers: ["tensorrt_rtx"] });
    const nv = await enroll("nv", { profile: NVIDIA });
    const amd = await enroll("amd", { profile: AMD });
    expect(ok(await ask(amd, GPU_CAPS)).reason).toBe("no_work");
    const l = await lease(nv, GPU_CAPS);
    expect(l).toMatchObject({ run: "nv-only", kind: "selfplay", providers: ["tensorrt_rtx"] });
    // `any` never filters: the allowlist only says what the job may use.
    ok(await admin("PUT", "/v1/admin/run/nv-only", { ...runBody(l.build.build_id), selfplay: { ...runBody(l.build.build_id).selfplay, gpu: "any", providers: ["tensorrt_rtx"] } }));
    await define("run", "nv-only");
    expect(await lease(amd, GPU_CAPS)).toMatchObject({ run: "nv-only", providers: ["tensorrt_rtx"] });
    expect(await lease(await enroll("cpu", { profile: CPU_ONLY }))).toMatchObject({ run: "nv-only", providers: ["tensorrt_rtx"] });
  });

  it("eligibility uses the probe results for the run's runtime pack when the profile has them", async () => {
    const rt = await makeBuild({ kind: "runtime", tag: "rt" });
    await routedRun("r", { gpu: "required" }, { runtime: rt });
    // TensorRT-RTX passed with another runtime pack, but with this one only the CPU did.
    const a = await enroll("a", { profile: { ...NVIDIA, runtime_providers: { [rt]: ["cpu"], ["ab".repeat(12)]: ["tensorrt_rtx", "cpu"] } } });
    // No results for this runtime: the union counts.
    const b = await enroll("b", { profile: { ...NVIDIA, runtime_providers: { ["ab".repeat(12)]: ["cpu"] } } });
    // DirectML passed with this runtime pack (the union is out of date).
    const c = await enroll("c", { profile: { ...CPU_ONLY, runtime_providers: { [rt]: ["dml", "cpu"] } } });
    expect(ok(await ask(a, GPU_CAPS)).reason).toBe("no_work");
    expect((await lease(b, GPU_CAPS)).runtime.build_id).toBe(rt);
    expect((await lease(c, GPU_CAPS)).runtime.build_id).toBe(rt);
  });

  it("uses a profile sent with the lease request at once", async () => {
    await routedRun("r", { gpu: "required" });
    const m = await enroll("m", { profile: CPU_ONLY });
    expect(ok(await ask(m, GPU_CAPS)).reason).toBe("no_work");
    // The agent's probe found a GPU provider: new digest, new profile.
    expect((await lease(m, { ...GPU_CAPS, digest: "cd".repeat(32), profile: AMD })).run).toBe("r");
    expect((await lease(m, { ...GPU_CAPS, digest: "cd".repeat(32) })).run).toBe("r"); // stored now
  });

  it("preferred: GPU-eligible machines play it first; others only as backfill (x0.25 against fuzz)", async () => {
    await routedRun("r", { gpu: "preferred", providers: ["migraphx", "amdgpu"] });
    await makeCampaign("fz", { priority: 1 });
    const amd = await enroll("amd", { profile: AMD });
    const nv = await enroll("nv", { profile: NVIDIA });
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    expect(await lease(amd, GPU_CAPS)).toMatchObject({ kind: "selfplay", providers: ["migraphx", "amdgpu"] });
    expect((await lease(nv, GPU_CAPS)).kind).toBe("fuzz"); // NVIDIA isn't in the allowlist: backfill, fuzz wins (0.5 > 0.25)
    expect((await lease(cpu)).kind).toBe("fuzz"); // 1.0 > 0.25
    // A cheaper campaign makes the backfill worth more than fuzz (by score: 0.25 / 2 > 0.2 / 3).
    ok(await admin("PUT", "/v1/admin/campaign/fz", { build_id: (await admin("GET", "/v1/admin/campaign/fz")).body.build_id, mode: "delver", env: { FUZZ_POOL: "delver" }, seed_base: 10_000_000, chunk: 1000, priority: 0.2 }));
    await define("campaign", "fz");
    expect((await lease(cpu)).kind).toBe("selfplay");
  });

  it("evaluation goes before self-play within a tier, whatever the score", async () => {
    const seats = [{ net: 1 }, { bot: "heuristic" }];
    await routedRun("hi", { gpu: "any" }, { priority: 5 });
    await routedRun("lo", { gpu: "any" }, { priority: 0.5, eval: { batch: "b1", games: 4, checkpoints: [1], games_per_lease: 2, job: { seats, paired: true }, providers: ["dml"] } });
    const m = await enroll("m", { profile: NVIDIA });
    expect(await lease(m)).toMatchObject({ run: "lo", kind: "eval", providers: ["dml"] });
    expect(await lease(m)).toMatchObject({ run: "lo", kind: "eval" });
    expect(await lease(m)).toMatchObject({ run: "hi", kind: "selfplay", providers: null }); // batch all issued
  });

  it("a GPU-preferring eval on a PC that isn't eligible is backfill: after the tier-1 self-play", async () => {
    const seats = [{ net: 1 }, { bot: "heuristic" }];
    await routedRun("r", { gpu: "any" }, { eval: { batch: "b1", games: 4, checkpoints: [1], games_per_lease: 2, job: { seats, paired: true }, gpu: "preferred" } });
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    const gpu = await enroll("gpu", { profile: AMD });
    expect((await lease(cpu)).kind).toBe("selfplay");
    expect((await lease(gpu, GPU_CAPS)).kind).toBe("eval");
  });

  it("campaigns: gpu and providers route fuzz the same way (tier 2)", async () => {
    await routedCampaign("plain", {}, 1);
    await routedCampaign("dml", { gpu: "preferred", providers: ["dml"] }, 0.8);
    await routedCampaign("cuda", { gpu: "required", providers: ["cuda"] }, 5);
    const nv = await enroll("nv", { profile: NVIDIA });
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    // NVIDIA PC (tensorrt_rtx, dml): not eligible for "cuda"; "dml" 0.8 x 1 beats "plain" 1 x 0.5.
    expect(await lease(nv, GPU_CAPS)).toMatchObject({ kind: "fuzz", campaign: "dml", providers: ["dml"] });
    // CPU PC: "plain" 1 x 1 beats "dml" 0.8 x 0.25; "cuda" is not for it.
    expect(await lease(cpu)).toMatchObject({ kind: "fuzz", campaign: "plain", providers: null });
    const cudaPc = await enroll("cuda", { profile: { ...NVIDIA, providers: ["cuda", "cpu"] } });
    expect(await lease(cudaPc, GPU_CAPS)).toMatchObject({ campaign: "cuda", providers: ["cuda"] });
    for (let i = 0; i < 4; i++) expect((await lease(cpu)).campaign).not.toBe("cuda");
  });

  it("every lease carries providers: null without an allowlist, including diag leases", async () => {
    const build = await makeRun("r", { define: false });
    const m = await enroll("pc");
    ok(await admin("POST", "/v1/admin/diag", { machine_id: m.machine_id, kind: "sweep", params: {} }));
    const d = await lease(m, { agent: LEGACY });
    expect(d.kind).toBe("diag");
    expect(d).toHaveProperty("providers", null);
    const s = await lease(m, { agent: LEGACY });
    expect(s).toMatchObject({ kind: "selfplay", build: { build_id: build } });
    expect(s).toHaveProperty("providers", null);
    await makeCampaign("fz", { priority: 0.01 });
    const f = await lease(await enroll("f"), { kinds: ["fuzz"] });
    expect(f.kind).toBe("fuzz");
    expect(f).toHaveProperty("providers", null);
  });
});
