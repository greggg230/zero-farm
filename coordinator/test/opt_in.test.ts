// Rev 27 (PROTOCOL 19): opt-in runtimes per machine (torch). A runtime pack with `opt_in` (the torch pack: provider
// torch-cuda) is leased only to agents from TORCH_AGENT on machines whose `opt_in` list - set by the owner, admin
// `machine <id> opt-in torch` - holds its name, default or alternate alike; a run alternate with `when.opt_in` qualifies
// only there, on top of every rule of 9.4; `when.opt_in` must equal the pack's `opt_in`. Everyone else falls through to
// the next alternate (CUDA) or the default, exactly as before.

import { describe, expect, it } from "vitest";
import { OPT_IN_RE, machineOptIns, optInOf } from "../src/routing";
import { PROFILE, admin, ask, define, enroll, err, lease, makeBuild, ok, publishCkpt, runBody } from "./helpers";

const TORCH = "0.3.19"; // TORCH_AGENT's default
const BEFORE = "0.3.18";
const NV = PROFILE; // an RTX 4070, NVIDIA driver 610.88
const GPU_OK = { gpu: false, gpu_allowed: true };

interface Setup {
  cpuRt: string;
  cuda: string;
  torch: string;
  def: string;
  cudaAlt: string;
  torchAlt: string;
}

const torchGpu = { vendor: "nvidia", min_driver: "570.65", providers: ["torch-cuda"] };

// The CPU pack + its worker (default), the CUDA pack + its worker and the torch pack + the torch worker (alternates).
async function packs(): Promise<Setup> {
  const cpuRt = await makeBuild({ kind: "runtime", tag: "cpu" });
  const cuda = await makeBuild({ kind: "runtime", tag: "cuda", min_agent: "0.2.10", gpu: { vendor: "nvidia", min_driver: "527.41", providers: ["cuda"] } });
  const torch = await makeBuild({ kind: "runtime", tag: "torch", min_agent: TORCH, gpu: torchGpu, opt_in: "torch" });
  const def = await makeBuild({ tag: "w-cpu", requires_runtime: cpuRt });
  const cudaAlt = await makeBuild({ tag: "w-cuda", requires_runtime: cuda, min_agent: "0.2.10" });
  const torchAlt = await makeBuild({ tag: "w-torch", requires_runtime: torch, min_agent: TORCH });
  return { cpuRt, cuda, torch, def, cudaAlt, torchAlt };
}

const torchEntry = (s: Setup) => ({ build_id: s.torchAlt, runtime: s.torch, when: { vendor: "nvidia", opt_in: "torch" }, model: "torch" });
const cudaEntry = (s: Setup) => ({ build_id: s.cudaAlt, runtime: s.cuda, when: { vendor: "nvidia" } });

async function torchRun(run: string, providers?: string[]): Promise<Setup> {
  const s = await packs();
  const sp = { ...runBody(s.def).selfplay, quota: 100000, games_per_lease: "auto", ...(providers ? { providers } : {}) };
  ok(await admin("PUT", `/v1/admin/run/${run}`, { ...runBody(s.def), runtime: s.cpuRt, selfplay: sp, alternates: [torchEntry(s), cudaEntry(s)] }));
  await publishCkpt(run, 1);
  await define("run", run, (d) => d.builds.push(s.cudaAlt, s.torchAlt));
  return s;
}

const pick = (l: any) => [l.build.build_id, l.runtime.build_id];

describe("opt-in runtimes per machine (rev 27)", () => {
  it("names and stored lists", () => {
    expect(OPT_IN_RE.test("torch")).toBe(true);
    expect(OPT_IN_RE.test("Torch")).toBe(false);
    expect(OPT_IN_RE.test("x".repeat(33))).toBe(false);
    expect(optInOf(JSON.stringify({ kind: "runtime", opt_in: "torch" }))).toBe("torch");
    expect(optInOf(JSON.stringify({ kind: "worker", opt_in: "torch" }))).toBe(null);
    expect(optInOf(null)).toBe(null);
    expect([...machineOptIns('["torch","BAD",3]')]).toEqual(["torch"]);
    expect([...machineOptIns("not json")]).toEqual([]);
    expect([...machineOptIns(null)]).toEqual([]);
  });

  it("the admin CLI's opt-in / opt-out: add, remove, replace, validation; machines and stats show the list", async () => {
    const m = await enroll("owner-pc", { profile: NV });
    expect(ok(await admin("POST", `/v1/admin/machine/${m.machine_id}`, { opt_in_add: "torch" })).opt_in).toEqual(["torch"]);
    expect(ok(await admin("POST", `/v1/admin/machine/${m.machine_id}`, { opt_in_add: "torch" })).opt_in).toEqual(["torch"]);
    expect(ok(await admin("POST", `/v1/admin/machine/${m.machine_id}`, { opt_in_add: "other" })).opt_in).toEqual(["other", "torch"]);
    expect(ok(await admin("POST", `/v1/admin/machine/${m.machine_id}`, { opt_in_remove: "other" })).opt_in).toEqual(["torch"]);
    const listed = ok(await admin("GET", "/v1/admin/machines")).machines.find((x: any) => x.machine_id === m.machine_id);
    expect(listed.opt_in).toEqual(["torch"]);
    const stats = ok(await admin("GET", "/v1/admin/stats"));
    expect(stats.machines.find((x: any) => x.machine_id === m.machine_id).opt_in).toEqual(["torch"]);
    expect(stats.limits.torch_agent).toBe(TORCH);
    // other machines: none
    const f = await enroll("friend", { profile: NV });
    expect(ok(await admin("GET", "/v1/admin/machines")).machines.find((x: any) => x.machine_id === f.machine_id).opt_in).toEqual([]);
    // replace / clear
    expect(ok(await admin("POST", `/v1/admin/machine/${m.machine_id}`, { opt_in: ["a", "b"] })).opt_in).toEqual(["a", "b"]);
    expect(ok(await admin("POST", `/v1/admin/machine/${m.machine_id}`, { opt_in: [] })).opt_in).toEqual([]);
    expect(ok(await admin("POST", `/v1/admin/machine/${m.machine_id}`, { opt_in_remove: "torch" })).opt_in).toEqual([]);
    // the other fields don't touch it, nor answer it
    expect(ok(await admin("POST", `/v1/admin/machine/${m.machine_id}`, { name: "renamed" }))).toEqual({ ok: true });
    // 400s
    err(await admin("POST", `/v1/admin/machine/${m.machine_id}`, { opt_in_add: "Torch" }), 400, "bad_request");
    err(await admin("POST", `/v1/admin/machine/${m.machine_id}`, { opt_in_add: "" }), 400, "bad_request");
    err(await admin("POST", `/v1/admin/machine/${m.machine_id}`, { opt_in: "torch" }), 400, "bad_request");
    err(await admin("POST", `/v1/admin/machine/${m.machine_id}`, { opt_in: ["torch", "torch"] }), 400, "bad_request");
    err(await admin("POST", `/v1/admin/machine/${m.machine_id}`, { opt_in: ["a", "b", "c", "d", "e", "f", "g", "h", "i"] }), 400, "bad_request");
    err(await admin("POST", `/v1/admin/machine/${m.machine_id}`, { opt_in: ["torch"], opt_in_add: "x" }), 400, "bad_request");
    err(await admin("POST", "/v1/admin/machine/m_nonexistent0000", { opt_in_add: "torch" }), 404, "not_found");
  });

  it("only an opted-in machine with an agent from TORCH_AGENT gets the torch alternate; the rest fall through to CUDA", async () => {
    const s = await torchRun("torch-sp");
    const owner = await enroll("owner", { profile: NV });
    const friend = await enroll("friend", { profile: NV });
    ok(await admin("POST", `/v1/admin/machine/${owner.machine_id}`, { opt_in_add: "torch" }));
    // opted in, new agent: the torch alternate, its variant
    const l = await lease(owner, { agent: TORCH, caps: GPU_OK, kinds: ["selfplay"] });
    expect(pick(l)).toEqual([s.torchAlt, s.torch]);
    // opted in, an agent before TORCH_AGENT: the CUDA alternate (it would refuse an opt-in pack)
    expect(pick(await lease(owner, { agent: BEFORE, caps: GPU_OK, kinds: ["selfplay"] }))).toEqual([s.cudaAlt, s.cuda]);
    // a friend's NVIDIA PC, any agent: never the torch pack
    for (const agent of [BEFORE, TORCH, "9.9.9"]) {
      expect(pick(await lease(friend, { agent, caps: GPU_OK, kinds: ["selfplay"] })), agent).toEqual([s.cudaAlt, s.cuda]);
    }
    // the owner opts the machine out: from its next lease on, CUDA
    ok(await admin("POST", `/v1/admin/machine/${owner.machine_id}`, { opt_in_remove: "torch" }));
    expect(pick(await lease(owner, { agent: TORCH, caps: GPU_OK, kinds: ["selfplay"] }))).toEqual([s.cudaAlt, s.cuda]);
  });

  it("every other rule of 9.4 still holds for an opted-in machine: GPU allowed, driver, probe, allowlist", async () => {
    const s = await torchRun("torch-sp");
    const optIn = async (name: string, profile: object) => {
      const m = await enroll(name, { profile });
      ok(await admin("POST", `/v1/admin/machine/${m.machine_id}`, { opt_in_add: "torch" }));
      return m;
    };
    // GPU work not allowed now: the default
    const a = await optIn("a", NV);
    expect(pick(await lease(a, { agent: TORCH, caps: { gpu: false, gpu_allowed: false }, kinds: ["selfplay"] }))).toEqual([s.def, s.cpuRt]);
    // a driver older than the torch pack's minimum: CUDA
    const old = await optIn("old", { ...NV, gpus: [{ vendor: "nvidia", name: "RTX 3060", vram_mb: 12288, driver: "560.94" }] });
    expect(pick(await lease(old, { agent: TORCH, caps: GPU_OK, kinds: ["selfplay"] }))).toEqual([s.cudaAlt, s.cuda]);
    // the torch probe failed here ([]): CUDA; passed (["torch-cuda"]): torch
    const failed = await optIn("failed", { ...NV, runtime_providers: { [s.torch]: [] } });
    expect(pick(await lease(failed, { agent: TORCH, caps: GPU_OK, kinds: ["selfplay"] }))).toEqual([s.cudaAlt, s.cuda]);
    const passed = await optIn("passed", { ...NV, runtime_providers: { [s.torch]: ["torch-cuda"] } });
    expect(pick(await lease(passed, { agent: TORCH, caps: GPU_OK, kinds: ["selfplay"] }))).toEqual([s.torchAlt, s.torch]);
  });

  it("a run whose providers allowlist lacks torch-cuda never offers the torch alternate, and the admin view says so", async () => {
    const s = await torchRun("torch-cu", ["cuda", "cpu"]);
    const owner = await enroll("owner", { profile: NV });
    ok(await admin("POST", `/v1/admin/machine/${owner.machine_id}`, { opt_in_add: "torch" }));
    expect(pick(await lease(owner, { agent: TORCH, caps: GPU_OK, kinds: ["selfplay"] }))).toEqual([s.cudaAlt, s.cuda]);
    const run = ok(await admin("GET", "/v1/admin/run/torch-cu"));
    expect(run.alternates[0].problems.join("\n")).toMatch(/selfplay: its providers allowlist allows none of the pack's GPU providers \(torch-cuda\)/);
    expect(run.alternates[1].problems).toEqual([]);
    // the allowlist with torch-cuda: offered
    const ok2 = await torchRun("torch-ok", ["torch-cuda", "cuda", "cpu"]);
    expect(pick(await lease(owner, { agent: TORCH, caps: GPU_OK, kinds: ["selfplay"] }))[1]).toBe(ok2.torch);
  });

  it("GET run lists the opted-in machines with the alternate (opt_in_notes)", async () => {
    await torchRun("torch-sp");
    let run = ok(await admin("GET", "/v1/admin/run/torch-sp"));
    expect(run.alternates[0].when).toEqual({ vendor: "nvidia", opt_in: "torch" });
    expect(run.alternates[0].opt_in_notes[0]).toMatch(/offered only to machines opted in to torch .* agents from 0\.3\.19: none yet/);
    expect(run.alternates[1].opt_in_notes).toBeUndefined();
    const owner = await enroll("owner-desktop", { profile: NV });
    ok(await admin("POST", `/v1/admin/machine/${owner.machine_id}`, { opt_in_add: "torch" }));
    run = ok(await admin("GET", "/v1/admin/run/torch-sp"));
    expect(run.alternates[0].opt_in_notes[0]).toContain(`owner-desktop (${owner.machine_id})`);
  });

  it("an opt-in pack as a run's default goes only to opted-in machines with a new agent", async () => {
    const torch = await makeBuild({ kind: "runtime", tag: "torch-d", min_agent: TORCH, gpu: torchGpu, opt_in: "torch" });
    const w = await makeBuild({ tag: "w-torch-d", requires_runtime: torch, min_agent: TORCH });
    ok(await admin("PUT", "/v1/admin/run/torch-d", { ...runBody(w), runtime: torch, selfplay: { ...runBody(w).selfplay, quota: 100000 } }));
    await publishCkpt("torch-d", 1);
    await define("run", "torch-d");
    const owner = await enroll("owner", { profile: NV });
    const friend = await enroll("friend", { profile: NV });
    ok(await admin("POST", `/v1/admin/machine/${owner.machine_id}`, { opt_in_add: "torch" }));
    expect(pick(await lease(owner, { agent: TORCH, caps: GPU_OK, kinds: ["selfplay"] }))).toEqual([w, torch]);
    for (const [m, agent] of [[friend, TORCH], [owner, BEFORE]] as const) {
      expect(ok(await ask(m, { agent, caps: GPU_OK, kinds: ["selfplay"] })).lease ?? null, `${m.name} ${agent}`).toBe(null);
    }
  });

  it("PUT run: when.opt_in must equal the pack's opt_in (400 otherwise)", async () => {
    const s = await packs();
    const put = (alternates: unknown[]) => admin("PUT", "/v1/admin/run/bad", { ...runBody(s.def), runtime: s.cpuRt, alternates });
    // an opt-in pack without when.opt_in
    err(await put([{ build_id: s.torchAlt, runtime: s.torch, when: { vendor: "nvidia" } }]), 400, "bad_request");
    // another name
    err(await put([{ build_id: s.torchAlt, runtime: s.torch, when: { vendor: "nvidia", opt_in: "other" } }]), 400, "bad_request");
    // when.opt_in on a pack without one
    err(await put([{ build_id: s.cudaAlt, runtime: s.cuda, when: { vendor: "nvidia", opt_in: "torch" } }]), 400, "bad_request");
    // a malformed name
    err(await put([{ build_id: s.torchAlt, runtime: s.torch, when: { vendor: "nvidia", opt_in: "Torch!" } }]), 400, "bad_request");
    // the pack's own name: accepted; when.vendor may come from the pack
    ok(await put([{ build_id: s.torchAlt, runtime: s.torch, when: { opt_in: "torch" } }, cudaEntry(s)]));
    expect(ok(await admin("GET", "/v1/admin/run/bad")).alternates[0].when).toEqual({ vendor: "nvidia", opt_in: "torch" });
  });

  it("build registration: opt_in on runtime packs with a GPU requirement only; torch-cuda only in an opt-in pack; many files", async () => {
    const reg = async (o: Parameters<typeof makeBuild>[0]) => {
      const id = await makeBuild({ ...o, register: false });
      return admin("POST", "/v1/admin/build", { build_id: id });
    };
    err(await reg({ kind: "runtime", tag: "t1", gpu: torchGpu }), 400, "bad_request"); // torch-cuda without opt_in
    err(await reg({ kind: "runtime", tag: "t2", opt_in: "torch" }), 400, "bad_request"); // opt_in without gpu
    err(await reg({ tag: "t3", opt_in: "torch" }), 400, "bad_request"); // a worker with opt_in
    err(await reg({ kind: "runtime", tag: "t4", gpu: torchGpu, opt_in: "Torch" }), 400, "bad_request");
    ok(await reg({ kind: "runtime", tag: "t5", gpu: torchGpu, opt_in: "torch", min_agent: TORCH }));
    // an opt-in pack may list more than 2,000 files (a python's site-packages), checked with R2 list calls; a missing file
    // is still caught
    const many = Array.from({ length: 2100 }, (_, i) => `python/Lib/site-packages/pkg/m${i}.py`);
    ok(await reg({ kind: "runtime", tag: "t6", gpu: torchGpu, opt_in: "torch", min_agent: TORCH, extra_files: many }));
    err(await reg({ kind: "runtime", tag: "t7", gpu: { vendor: "nvidia", providers: ["cuda"] }, extra_files: many }), 400, "bad_request");
  }, 120_000);
});
