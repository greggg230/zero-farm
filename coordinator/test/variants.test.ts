// Rev 20 (PROTOCOL 14): model variants per generation. A definition's checkpoints may sign `variants` (other model files
// of a gen, sharing its reference set); PUT .../checkpoint/{gen} registers them; a run alternate's `model` names the
// variant its machines get, in self-play and eval leases and in beat answers' generation switches, while every other
// machine gets the default model. A gen without the variant gives the alternate's machines its default model (an admin
// note). A run whose current definition signs a variant counts as min_agent 0.3.5 (older agents can't read it).

import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import {
  type Machine,
  PROFILE,
  admin,
  ask,
  beat,
  call,
  checkDefinitionLease,
  define,
  definitionFor,
  enroll,
  err,
  lease,
  makeBuild,
  makeCampaign,
  ok,
  putArtifact,
  putDefinition,
  runBody,
  sha256,
} from "./helpers";

const NEW = "0.3.5"; // VARIANT_AGENT's default
const OLD = "0.3.4"; // takes alternates (0.2.10+) and rolling leases, but can't read a definition with variants
const NV = PROFILE; // an RTX 4070
const CPU_ONLY = { ...PROFILE, gpus: [], providers: ["cpu"] };
const GPU_OK = { gpu: false, gpu_allowed: true };

interface Setup {
  cpuRt: string;
  cuda: string;
  def: string;
  alt: string;
}

async function packs(): Promise<Setup> {
  const cpuRt = await makeBuild({ kind: "runtime", tag: "v-cpu" });
  const cuda = await makeBuild({ kind: "runtime", tag: "v-cuda", min_agent: "0.2.10", gpu: { vendor: "nvidia", min_driver: "527.41", providers: ["cuda"] } });
  const def = await makeBuild({ tag: "v-w-cpu", requires_runtime: cpuRt });
  const alt = await makeBuild({ tag: "v-w-cuda", requires_runtime: cuda, min_agent: "0.2.10" });
  return { cpuRt, cuda, def, alt };
}

const altEntry = (s: Setup, model?: string) => ({ build_id: s.alt, runtime: s.cuda, when: { vendor: "nvidia" }, ...(model ? { model } : {}) });

// Uploads a gen's model, reference and the named variants' files and registers them (no definition).
async function register(run: string, gen: number, variants: string[] = [], o: { tag?: string } = {}): Promise<Res> {
  const model = `onnx ${run} ${gen}`;
  const ref = `reference ${run} ${gen}`;
  await putArtifact(`ckpt/${run}/${gen}/model.onnx`, model);
  await putArtifact(`ckpt/${run}/${gen}/reference.npz`, ref);
  const vs: Record<string, { model: string; sha256: string }> = {};
  for (const v of variants) {
    const text = `onnx ${run} ${gen} variant ${v}${o.tag ?? ""}`;
    await putArtifact(`ckpt/${run}/${gen}/model-${v}.onnx`, text);
    vs[v] = { model: `ckpt/${run}/${gen}/model-${v}.onnx`, sha256: await sha256(text) };
  }
  return admin("PUT", `/v1/admin/run/${run}/checkpoint/${gen}`, {
    model: `ckpt/${run}/${gen}/model.onnx`,
    sha256: await sha256(model),
    reference: `ckpt/${run}/${gen}/reference.npz`,
    reference_sha256: await sha256(ref),
    ...(variants.length ? { variants: vs } : {}),
  });
}
type Res = Awaited<ReturnType<typeof admin>>;

// A self-play run: the CPU pack by default, the CUDA alternate naming variant "gpu"; gen 1 with that variant, signed.
async function variantRun(run: string, o: { model?: string | null; rolling?: boolean; min_agent?: string; evalGens?: [number, number][] } = {}) {
  const s = await packs();
  const body = (extra: Record<string, unknown> = {}) => {
    const b: any = { ...runBody(s.def, { games_per_lease: 10, quota: 100000 }), runtime: s.cpuRt, alternates: [altEntry(s, o.model === null ? undefined : (o.model ?? "gpu"))] };
    if (o.min_agent) b.min_agent = o.min_agent;
    if (o.rolling) b.selfplay.rolling = { inflight: 8, gpu_inflight: 32, block: 1000 };
    return { ...b, ...extra };
  };
  ok(await admin("PUT", `/v1/admin/run/${run}`, body()));
  ok(await register(run, 1, ["gpu"]));
  await define("run", run, (d) => d.builds.push(s.alt));
  return { s, body };
}

const rollingAsk = (m: Machine, agent: string, caps: Record<string, unknown>) =>
  call("POST", "/v1/lease", {
    token: m.token,
    agent,
    body: {
      profile_digest: "ab".repeat(32),
      profile: null,
      caps: { threads: 6, ram_mb: 12000, gpu: false, disk_mb: 2500, ...caps },
      cached: { builds: [], ckpts: [] },
      kinds: ["selfplay"],
      features: ["rolling"],
    },
  });

async function release(version: string, min_agent: string, channel = "stable") {
  const id = await makeBuild({ kind: "agent", version });
  ok(await admin("POST", "/v1/admin/agent/release", { version, build_id: id, channel, min_agent }));
}

const variantRow = async (run: string, gen: number) =>
  JSON.parse((await env.DB.prepare(`SELECT variants FROM checkpoints WHERE run = ? AND gen = ?`).bind(run, gen).first<{ variants: string }>())!.variants);

describe("model variants: the definition (14.1)", () => {
  it("signs variants per gen; malformed ones are refused, an empty object too", async () => {
    await variantRun("vdef");
    const d0 = await definitionFor("run", "vdef");
    expect(d0.checkpoints["1"].variants.gpu.model_size).toBeGreaterThan(0);
    const bad = async (mutate: (v: any) => void, why: RegExp) => {
      const d = await definitionFor("run", "vdef");
      mutate(d.checkpoints["1"]);
      const b = err(await putDefinition(d), 400, "bad_request");
      expect(JSON.stringify(b.problems ?? b.message)).toMatch(why);
    };
    await bad((c) => (c.variants = {}), /variants must be an object of 1 to 8/);
    await bad((c) => (c.variants = { GPU: c.variants.gpu }), /not a variant name/);
    await bad((c) => (c.variants = { "-x": c.variants.gpu }), /not a variant name/);
    await bad((c) => (c.variants.gpu.reference_sha256 = "a".repeat(64)), /variants.gpu.reference_sha256: unknown field/);
    await bad((c) => (c.variants.gpu.model_sha256 = "A".repeat(64)), /model_sha256 must be lowercase hex/);
    await bad((c) => (c.variants.gpu.model_size = 0), /model_size must be 1 to/);
    await bad((c) => (c.variants = Object.fromEntries([..."abcdefghi"].map((n) => [n, c.variants.gpu]))), /1 to 8/);
    // ... and a variant the run registered with other files (it can't change)
    await bad((c) => (c.variants.gpu.model_sha256 = "b".repeat(64)), /variant gpu differs from definition/);
    // a variant signed that the run hasn't registered (yet) is fine; so is the key order
    const d = await definitionFor("run", "vdef");
    d.checkpoints["1"].variants.fp16 = { model_size: 99, model_sha256: "c".repeat(64) };
    ok(await putDefinition(d));
    const st = ok(await admin("GET", "/v1/admin/run/vdef")).definition;
    expect(st.variants).toEqual({ "1": ["fp16", "gpu"] });
    expect(st.needs_agent).toBe(NEW);
  });
});

describe("model variants: registering (14.2)", () => {
  it("PUT checkpoint takes variants; a registered gen adds names, never changes or drops one", async () => {
    const s = await packs();
    ok(await admin("PUT", "/v1/admin/run/vreg", { ...runBody(s.def), runtime: s.cpuRt }));
    expect(ok(await register("vreg", 1, ["gpu"]))).toEqual({ ok: true, unchanged: false, current: true, variants: ["gpu"] });
    const row = await variantRow("vreg", 1);
    expect(row.gpu.model).toBe("ckpt/vreg/1/model-gpu.onnx");
    expect(row.gpu.size).toBe(new TextEncoder().encode("onnx vreg 1 variant gpu").length);
    // the same again: unchanged; another name added (the gen stays current, its quota untouched); none dropped
    expect(ok(await register("vreg", 1, ["gpu"]))).toEqual({ ok: true, unchanged: true, current: true, variants: ["gpu"] });
    expect(ok(await register("vreg", 1, ["fp16"]))).toEqual({ ok: true, unchanged: false, current: true, variants: ["fp16", "gpu"] });
    expect(ok(await register("vreg", 1))).toEqual({ ok: true, unchanged: true, current: true, variants: ["fp16", "gpu"] });
    // a registered name with another file: 409
    err(await register("vreg", 1, ["gpu"], { tag: " other" }), 409, "conflict");
    // GET run: the current gen's variants and every gen that has any
    await register("vreg", 2);
    const run = ok(await admin("GET", "/v1/admin/run/vreg"));
    expect(run.checkpoint.variants).toEqual({});
    expect(run.variants).toEqual({ "1": ["fp16", "gpu"] });
    // an older gen with a variant is registered without becoming current
    expect(ok(await register("vreg", 0, ["gpu"]))).toEqual({ ok: true, unchanged: false, current: false, variants: ["gpu"] });

    // malformed bodies
    const m = await sha256("onnx vreg 1");
    const r = await sha256("reference vreg 1");
    const base = { model: "ckpt/vreg/1/model.onnx", sha256: m, reference: "ckpt/vreg/1/reference.npz", reference_sha256: r };
    const vsha = await sha256("onnx vreg 1 variant gpu");
    const put = (variants: unknown) => admin("PUT", "/v1/admin/run/vreg/checkpoint/1", { ...base, variants });
    err(await put({ GPU: { model: "ckpt/vreg/1/model-gpu.onnx", sha256: vsha } }), 400, "bad_request");
    err(await put({ gpu: { model: "ckpt/other/1/model-gpu.onnx", sha256: vsha } }), 400, "bad_request");
    err(await put({ gpu: { model: "ckpt/vreg/1/missing.onnx", sha256: vsha } }), 400, "bad_request");
    err(await put({ gpu: { model: "ckpt/vreg/1/model-gpu.onnx", sha256: "d".repeat(64) } }), 400, "bad_request"); // R2's checksum
    err(await put({ gpu: { model: "ckpt/vreg/1/model.onnx", sha256: m } }), 400, "bad_request"); // the gen's own model
    err(await put({ gpu: { model: "ckpt/vreg/1/model-gpu.onnx", sha256: vsha, size: 5 } }), 400, "bad_request");
    err(await put([]), 400, "bad_request");
  });

  it("a variant the current definition signs must be exactly those files", async () => {
    const s = await packs();
    ok(await admin("PUT", "/v1/admin/run/vsig", { ...runBody(s.def), runtime: s.cpuRt }));
    ok(await register("vsig", 1));
    // the definition signs gen 1's variant before it is registered (allowed); registering other bytes is refused
    await define("run", "vsig", (d) => (d.checkpoints["1"].variants = { gpu: { model_sha256: "e".repeat(64), model_size: 10 } }));
    const b = err(await register("vsig", 1, ["gpu"]), 400, "bad_request");
    expect(b.message).toMatch(/variant gpu differs from definition/);
  });
});

describe("model variants: which machines get one (14.3)", () => {
  it("an NVIDIA PC with agent 0.3.5 gets the alternate's variant; a CPU PC the default; both verify against the definition", async () => {
    const { s } = await variantRun("vsp", { min_agent: NEW });
    const nv = await enroll("nv", { profile: NV });
    const l = await lease(nv, { agent: NEW, caps: GPU_OK, kinds: ["selfplay"] });
    expect([l.build.build_id, l.runtime.build_id]).toEqual([s.alt, s.cuda]);
    expect(l.checkpoint).toEqual({
      gen: 1,
      model: "ckpt/vsp/1/model-gpu.onnx",
      sha256: await sha256("onnx vsp 1 variant gpu"),
      reference: "ckpt/vsp/1/reference.npz",
      reference_sha256: await sha256("reference vsp 1"),
      variant: "gpu",
    });
    await checkDefinitionLease(nv, l);
    expect((await env.DB.prepare(`SELECT variant FROM leases WHERE lease_id = ?`).bind(l.lease_id).first<any>()).variant).toBe("gpu");

    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    const c = await lease(cpu, { agent: NEW, caps: GPU_OK, kinds: ["selfplay"] });
    expect([c.build.build_id, c.checkpoint.model, "variant" in c.checkpoint]).toEqual([s.def, "ckpt/vsp/1/model.onnx", false]);
    await checkDefinitionLease(cpu, c);
    expect((await env.DB.prepare(`SELECT variant FROM leases WHERE lease_id = ?`).bind(c.lease_id).first<any>()).variant).toBeNull();
    // an NVIDIA PC that may not use the GPU now gets the default build, so the default model
    const busy = await enroll("busy", { profile: NV });
    const d = await lease(busy, { agent: NEW, caps: { gpu: false, gpu_allowed: false }, kinds: ["selfplay"] });
    expect([d.build.build_id, d.checkpoint.model]).toEqual([s.def, "ckpt/vsp/1/model.onnx"]);
  });

  it("an alternate without model gets the default model, though the gen has variants", async () => {
    const { s } = await variantRun("vnomodel", { model: null });
    const nv = await enroll("nv", { profile: NV });
    const l = await lease(nv, { agent: NEW, caps: GPU_OK, kinds: ["selfplay"] });
    expect([l.runtime.build_id, l.checkpoint.model, "variant" in l.checkpoint]).toEqual([s.cuda, "ckpt/vnomodel/1/model.onnx", false]);
    expect(ok(await admin("GET", "/v1/admin/run/vnomodel")).alternates).toEqual([{ ...altEntry(s), problems: [] }]);
  });

  it("older agents get nothing from a run whose definition signs variants: update when the channel offers 0.3.5, else other work", async () => {
    await variantRun("vold"); // (no run min_agent: the definition alone counts as 0.3.5)
    await makeCampaign("vfz");
    const nv = await enroll("nv", { profile: NV });
    // no 0.3.5 on its channel: the run is skipped, the agent fuzzes
    for (const agent of [OLD, "0.2.10"]) {
      const l = await lease(nv, { agent, caps: GPU_OK, kinds: ["selfplay", "fuzz"] });
      expect([l.kind, l.campaign], agent).toEqual(["fuzz", "vfz"]);
    }
    expect(ok(await ask(nv, { agent: OLD, caps: GPU_OK, kinds: ["selfplay"] })).reason).toBe("no_work");
    // 0.3.5 released: told to update
    await release(NEW, "0.2.5");
    expect(ok(await ask(nv, { agent: OLD, caps: GPU_OK, kinds: ["selfplay", "fuzz"] }))).toEqual({ update: { min_agent: NEW } });
    expect((await lease(nv, { agent: NEW, caps: GPU_OK, kinds: ["selfplay", "fuzz"] })).checkpoint.variant).toBe("gpu");
  });

  it("a run whose definition signs no variant is unaffected: older agents keep its leases (an alternate's model then has nothing to give)", async () => {
    const s = await packs();
    ok(await admin("PUT", "/v1/admin/run/vnone", { ...runBody(s.def, { quota: 100000 }), runtime: s.cpuRt, alternates: [altEntry(s, "gpu")] }));
    ok(await register("vnone", 1));
    await define("run", "vnone", (d) => d.builds.push(s.alt));
    const nv = await enroll("nv", { profile: NV });
    for (const agent of [OLD, NEW]) {
      const l = await lease(nv, { agent, caps: GPU_OK, kinds: ["selfplay"] });
      expect([l.runtime.build_id, l.checkpoint.model, "variant" in l.checkpoint], agent).toEqual([s.cuda, "ckpt/vnone/1/model.onnx", false]);
    }
    // the note says why the alternate's machines play the default model
    const alts = ok(await admin("GET", "/v1/admin/run/vnone")).alternates;
    expect(alts[0].variant_notes).toEqual(["gen 1: no variant gpu registered: its default model is leased"]);
    expect(alts[0].problems).toEqual([]);
  });

  it("a gen without the variant (not registered, not signed) gives the alternate's machines its default model, with a note", async () => {
    const { s } = await variantRun("vgap", { min_agent: NEW });
    // gen 2 registered without a variant
    ok(await register("vgap", 2));
    await define("run", "vgap", (d) => d.builds.push(s.alt));
    const nv = await enroll("nv", { profile: NV });
    let l = await lease(nv, { agent: NEW, caps: GPU_OK, kinds: ["selfplay"] });
    expect([l.checkpoint.gen, l.checkpoint.model, "variant" in l.checkpoint]).toEqual([2, "ckpt/vgap/2/model.onnx", false]);
    await checkDefinitionLease(nv, l);
    let run = ok(await admin("GET", "/v1/admin/run/vgap"));
    expect(run.alternates[0].variant_notes).toEqual(["gen 2: no variant gpu registered: its default model is leased"]);
    const stats = ok(await admin("GET", "/v1/admin/stats"));
    expect(stats.runs.find((r: any) => r.run === "vgap").alternates[0].variant_notes).toHaveLength(1);
    // registered later, but the current definition doesn't sign it yet: still the default
    ok(await register("vgap", 2, ["gpu"]));
    l = await lease(nv, { agent: NEW, caps: GPU_OK, kinds: ["selfplay"] });
    expect(l.checkpoint.model).toBe("ckpt/vgap/2/model.onnx");
    run = ok(await admin("GET", "/v1/admin/run/vgap"));
    expect(run.alternates[0].variant_notes[0]).toMatch(/^gen 2: definition v\d+ doesn't sign variant gpu: its default model is leased$/);
    // the next definition signs it: the variant
    await define("run", "vgap", (d) => d.builds.push(s.alt));
    l = await lease(nv, { agent: NEW, caps: GPU_OK, kinds: ["selfplay"] });
    expect([l.checkpoint.model, l.checkpoint.variant]).toEqual(["ckpt/vgap/2/model-gpu.onnx", "gpu"]);
    expect(ok(await admin("GET", "/v1/admin/run/vgap")).alternates[0].variant_notes).toEqual([]);
  });

  it("eval leases carry each gen's variant on its own", async () => {
    const s = await packs();
    const body = (ev: unknown) => ({ ...runBody(s.def, { selfplay: false }), runtime: s.cpuRt, alternates: [altEntry(s, "gpu")], eval: ev });
    ok(await admin("PUT", "/v1/admin/run/vev", body(null)));
    ok(await register("vev", 1));
    ok(await register("vev", 2, ["gpu"]));
    const ev = { batch: "b1", games: 400, checkpoints: [2, 1], games_per_lease: "auto", job: { settings: { sims: 32 }, seats: [{ net: 2 }, { net: 1 }], paired: true } };
    ok(await admin("PUT", "/v1/admin/run/vev", body(ev)));
    await define("run", "vev", (d) => d.builds.push(s.alt));
    const nv = await enroll("nv", { profile: NV });
    const l = await lease(nv, { agent: NEW, caps: GPU_OK, kinds: ["eval"] });
    expect(l.kind).toBe("eval");
    expect(l.checkpoints.map((c: any) => [c.gen, c.model, c.variant ?? null])).toEqual([
      [2, "ckpt/vev/2/model-gpu.onnx", "gpu"],
      [1, "ckpt/vev/1/model.onnx", null],
    ]);
    await checkDefinitionLease(nv, l);
    expect(ok(await admin("GET", "/v1/admin/run/vev")).alternates[0].variant_notes).toEqual(["gen 1: no variant gpu registered: its default model is leased"]);
    // a CPU PC: every gen's default
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    const c = await lease(cpu, { agent: NEW, kinds: ["eval"] });
    expect(c.checkpoints.map((x: any) => x.variant ?? null)).toEqual([null, null]);
  });

  it("a rolling lease's in-place switch carries the variant for its alternate only, and stops once the alternate drops model", async () => {
    const { s, body } = await variantRun("vroll", { rolling: true, min_agent: NEW });
    const nv = await enroll("nv", { profile: NV });
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    const g = ok(await rollingAsk(nv, NEW, GPU_OK)).lease;
    const c = ok(await rollingAsk(cpu, NEW, GPU_OK)).lease;
    expect([g.rolling !== undefined, g.runtime.build_id, g.checkpoint.variant]).toEqual([true, s.cuda, "gpu"]);
    expect([c.rolling !== undefined, c.runtime.build_id, "variant" in c.checkpoint]).toEqual([true, s.cpuRt, false]);
    // gen 2 with its variant, signed: each lease switches to its own file
    ok(await register("vroll", 2, ["gpu"]));
    const d = await define("run", "vroll", (x) => x.builds.push(s.alt));
    let a = ok(await beat(nv, g.lease_id, "running", { gen: 1 }));
    expect([a.checkpoint.gen, a.checkpoint.model, a.checkpoint.variant, a.definition.key]).toEqual([2, "ckpt/vroll/2/model-gpu.onnx", "gpu", d.key]);
    const b = ok(await beat(cpu, c.lease_id, "running", { gen: 1 }));
    expect([b.checkpoint.gen, b.checkpoint.model, "variant" in b.checkpoint]).toEqual([2, "ckpt/vroll/2/model.onnx", false]);
    // gen 3 without a variant: the default
    ok(await register("vroll", 3));
    await define("run", "vroll", (x) => x.builds.push(s.alt));
    a = ok(await beat(nv, g.lease_id, "running", { gen: 2 }));
    expect([a.checkpoint.gen, a.checkpoint.model, "variant" in a.checkpoint]).toEqual([3, "ckpt/vroll/3/model.onnx", false]);
    // gen 4 with a variant, but the owner dropped the alternate's model: the default for running leases too
    ok(await register("vroll", 4, ["gpu"]));
    await define("run", "vroll", (x) => x.builds.push(s.alt));
    a = ok(await beat(nv, g.lease_id, "running", { gen: 3 }));
    expect(a.checkpoint.variant).toBe("gpu");
    const plain = body();
    plain.alternates = [altEntry(s)];
    ok(await admin("PUT", "/v1/admin/run/vroll", plain));
    a = ok(await beat(nv, g.lease_id, "running", { gen: 3 }));
    expect([a.checkpoint.gen, a.checkpoint.model, "variant" in a.checkpoint]).toEqual([4, "ckpt/vroll/4/model.onnx", false]);
  });

  it("a rolling lease granted while its gen had no variant picks up a later gen's", async () => {
    const s = await packs();
    const b: any = { ...runBody(s.def, { quota: 100000 }), runtime: s.cpuRt, alternates: [altEntry(s, "gpu")], min_agent: NEW };
    b.selfplay.rolling = { inflight: 8, gpu_inflight: 32, block: 1000 };
    ok(await admin("PUT", "/v1/admin/run/vlate", b));
    ok(await register("vlate", 1));
    await define("run", "vlate", (d) => d.builds.push(s.alt));
    const nv = await enroll("nv", { profile: NV });
    const g = ok(await rollingAsk(nv, NEW, GPU_OK)).lease;
    expect([g.runtime.build_id, "variant" in g.checkpoint]).toEqual([s.cuda, false]);
    ok(await register("vlate", 2, ["gpu"]));
    await define("run", "vlate", (d) => d.builds.push(s.alt));
    const a = ok(await beat(nv, g.lease_id, "running", { gen: 1 }));
    expect([a.checkpoint.gen, a.checkpoint.variant]).toEqual([2, "gpu"]);
  });

  it("a classic self-play lease's switch to a gen its own definition signs carries the variant", async () => {
    const s = await packs();
    ok(await admin("PUT", "/v1/admin/run/vcl", { ...runBody(s.def, { quota: 100000 }), runtime: s.cpuRt, alternates: [altEntry(s, "gpu")], min_agent: NEW }));
    ok(await register("vcl", 1, ["gpu"]));
    // the definition signs gen 2 (and its variant) before gen 2 is registered, so a lease of it may switch there
    const len = (x: string) => new TextEncoder().encode(x).length;
    const [m2, r2, v2] = ["onnx vcl 2", "reference vcl 2", "onnx vcl 2 variant gpu"];
    const doc = await definitionFor("run", "vcl");
    doc.builds.push(s.alt);
    doc.checkpoints["2"] = {
      model_sha256: await sha256(m2),
      model_size: len(m2),
      reference_sha256: await sha256(r2),
      reference_size: len(r2),
      variants: { gpu: { model_sha256: await sha256(v2), model_size: len(v2) } },
    };
    ok(await putDefinition(doc));
    const nv = await enroll("nv", { profile: NV });
    const cpu = await enroll("cpu", { profile: CPU_ONLY });
    const l = await lease(nv, { agent: NEW, caps: GPU_OK, kinds: ["selfplay"] });
    const c = await lease(cpu, { agent: NEW, kinds: ["selfplay"] });
    expect([l.checkpoint.gen, l.checkpoint.variant, "rolling" in l]).toEqual([1, "gpu", false]);
    ok(await register("vcl", 2, ["gpu"]));
    const a = ok(await beat(nv, l.lease_id));
    expect([a.checkpoint.gen, a.checkpoint.model, a.checkpoint.variant, "definition" in a]).toEqual([2, "ckpt/vcl/2/model-gpu.onnx", "gpu", false]);
    const b = ok(await beat(cpu, c.lease_id));
    expect([b.checkpoint.gen, b.checkpoint.model, "variant" in b.checkpoint]).toEqual([2, "ckpt/vcl/2/model.onnx", false]);
  });

  it("an alternate's model is checked for its syntax only", async () => {
    const s = await packs();
    const put = (model: unknown) => admin("PUT", "/v1/admin/run/vsyn", { ...runBody(s.def), runtime: s.cpuRt, alternates: [{ ...altEntry(s), model }] });
    for (const bad of ["GPU", "", "a/b", "x".repeat(33), 5]) err(await put(bad), 400, "bad_request");
    ok(await put("fp16-fixed"));
    expect(ok(await admin("GET", "/v1/admin/run/vsyn")).alternates[0].model).toBe("fp16-fixed");
    ok(await put(null));
    expect("model" in ok(await admin("GET", "/v1/admin/run/vsyn")).alternates[0]).toBe(false);
  });
});
