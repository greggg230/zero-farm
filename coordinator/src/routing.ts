// Capability routing (PROTOCOL 8, rev 7; DESIGN 5.2): which machines a run's or campaign's jobs
// go to, from the job's `gpu` setting and `providers` allowlist and each machine's verified
// execution providers. Pure functions: friend.ts `lease()` asks them for each candidate.

import { type Platform, storedPlatform } from "./platform";
import { bad } from "./util";

// Execution providers by their wire names, and the GPU vendor each serves (`dml`: vendor-neutral, any DirectX 12
// GPU). Every name but `cpu` is a GPU provider. Rev 22: AMD's own providers (`migraphx`, `amdgpu`) are never used on AMD
// GPUs either (VENDOR_PROVIDERS below): they stay known names, for profiles and allowlists.
export const PROVIDER_VENDOR = {
  cpu: null,
  tensorrt_rtx: "nvidia",
  cuda: "nvidia",
  migraphx: "amd",
  amdgpu: "amd",
  openvino: "intel",
  dml: "any",
} as const;
export type Provider = keyof typeof PROVIDER_VENDOR;
export const PROVIDERS = Object.keys(PROVIDER_VENDOR) as Provider[];
export const isProvider = (p: unknown): p is Provider => typeof p === "string" && Object.hasOwn(PROVIDER_VENDOR, p);
export const isGpuProvider = (p: unknown): boolean => isProvider(p) && p !== "cpu";

export const GPU_MODES = ["any", "preferred", "required"] as const;
export type GpuMode = (typeof GPU_MODES)[number];

// A job's routing settings (run `selfplay` / `eval`, or a campaign).
export interface Routing {
  gpu: GpuMode;
  providers: Provider[] | null; // null: any provider
}

// Profile limits (PROTOCOL 3, rev 7): an adapter's LUID in hex, and per runtime build the
// providers that passed this PC's probe with that runtime pack.
export const LUID_RE = /^[0-9a-f]{1,16}$/;
export const MAX_RUNTIMES = 16;
export const MAX_RUNTIME_PROVIDERS = 16;

// `providers` of a run's `selfplay`/`eval` or of a campaign: absent or null (any provider), or a
// non-empty list of known provider names without repeats.
export function providersOf(v: unknown, name: string): Provider[] | null {
  if (v === undefined || v === null) return null;
  if (!Array.isArray(v) || v.length === 0) throw bad(`${name} must be a non-empty list of providers or null (any)`);
  for (const p of v) if (!isProvider(p)) throw bad(`${name}: unknown provider ${JSON.stringify(p)} (known: ${PROVIDERS.join(", ")})`);
  if (new Set(v).size !== v.length) throw bad(`${name} names a provider twice`);
  return v as Provider[];
}

// `gpu` and `providers` together. A job that prefers or requires a GPU needs a GPU provider in its
// allowlist: otherwise no machine could ever be eligible.
export function routingOf(gpu: GpuMode, providersV: unknown, prefix: string): Routing {
  const providers = providersOf(providersV, `${prefix}providers`);
  if (gpu !== "any" && providers && !providers.some(isGpuProvider)) {
    throw bad(`${prefix}gpu is "${gpu}" but ${prefix}providers allows no GPU provider`);
  }
  return { gpu, providers };
}

// What routing reads of a machine's stored profile (validated by friend.ts cleanProfile).
export interface RoutingProfile {
  providers?: unknown;
  runtime_providers?: unknown;
}

const strings = (v: unknown): string[] => (Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : []);

// The providers this machine verified for a job that uses `runtime` (a runtime build id, or null):
// its probe results with that runtime pack when the profile has them, else the union of every
// provider that passed on it (`profile.providers`).
export function verifiedProviders(profile: RoutingProfile | null, runtime: string | null): string[] {
  if (!profile) return [];
  const per = profile.runtime_providers;
  if (runtime && per && typeof per === "object" && !Array.isArray(per) && Object.hasOwn(per, runtime)) {
    return strings((per as Record<string, unknown>)[runtime]);
  }
  return strings(profile.providers);
}

// GPU-eligible: the agent says it may use the GPU (`caps.gpu`) and the machine verified a GPU
// provider that the job's allowlist allows. Provider names this coordinator doesn't know (a newer
// agent's) never count.
export function gpuEligible(capsGpu: boolean, verified: readonly string[], allow: readonly string[] | null): boolean {
  return capsGpu && verified.some((p) => isGpuProvider(p) && (!allow || allow.includes(p)));
}

// Where a job lands for this machine: tier 1 = a run's games, tier 2 = the backfill (fuzz, or a
// GPU-preferring job on a machine that isn't GPU-eligible); `fit` multiplies the score. Null: not
// for this machine (`required` and not eligible). `any` never filters.
export interface Placement {
  tier: 1 | 2;
  fit: number;
}
export function place(kind: "selfplay" | "eval" | "fuzz", r: Routing, eligible: boolean, capsGpu: boolean): Placement | null {
  if (r.gpu === "required" && !eligible) return null;
  if (kind === "fuzz") {
    // By default fuzzing (no network at all) goes preferentially to CPU-only PCs.
    if (r.gpu === "any") return { tier: 2, fit: capsGpu ? 0.5 : 1 };
    return { tier: 2, fit: eligible ? 1 : 0.25 };
  }
  if (r.gpu === "preferred" && !eligible) return { tier: 2, fit: 0.25 };
  return { tier: 1, fit: 1 };
}

// Within a tier, evaluation goes before self-play (PROTOCOL 8); self-play and fuzz compare by score.
export const kindRank = (kind: string): number => (kind === "eval" ? 0 : 1);

// Rev 13 (PROTOCOL 5.3): a candidate's rank within its tier. A run job with a `share` (a background job: an analysis
// campaign's eval batch or self-play) ranks first (0) only while this lease keeps its share of the runs' active leases
// (its own active leases of that kind + 1 at most `share` x (all runs' active self-play and eval leases + 1));
// otherwise after every other run job (2). The lease loop takes the first candidate that grants, so a background job
// still gets every machine whose other run jobs have nothing to give (a self-play quota filled, no run at all): it
// never idles a PC.
// Rev 14 (PROTOCOL 5.4): a fuzz campaign with a `share` takes part the same way (its candidate is then in tier 1); its
// own leases are not among the runs' self-play and eval leases, so they are added to the total it is a share of.
export function jobRank(kind: string, share: number | undefined, activeScope: number, activeRuns: number): number {
  if (share === undefined) return kindRank(kind);
  const total = kind === "fuzz" ? activeRuns + activeScope : activeRuns;
  return activeScope + 1 <= share * (total + 1) + 1e-9 ? 0 : 2;
}

// A stored run job (`selfplay`/`eval` JSON) or campaign's routing settings; configs put before
// rev 7 have neither field (any GPU setting, any provider).
export function storedRouting(gpu: unknown, providers: unknown): Routing {
  const mode = (GPU_MODES as readonly unknown[]).includes(gpu) ? (gpu as GpuMode) : "any";
  const list = Array.isArray(providers) && providers.length ? (providers.filter(isProvider) as Provider[]) : null;
  return { gpu: mode, providers: list };
}

// ---------- rev 11 (PROTOCOL 9): runtime packs per GPU vendor ----------

// A runtime pack's hard hardware requirement (its manifest's `gpu`, PROTOCOL 9.2).
export interface GpuNeed {
  vendor: string;
  min_driver: string | null;
  providers: Provider[];
}

// A run alternate (PROTOCOL 9.4): a worker build, the runtime pack it requires and the vendor condition. Rev 17
// (PROTOCOL 11.4): the platform it is for (stored only when not windows-x64, so a run put before rev 17 reads the same),
// and no vendor for a CPU alternate (offered to every machine of its platform, no GPU condition). Rev 20 (PROTOCOL 14.3):
// `model`, a model variant its machines play instead of each gen's default model (absent: the default).
export interface Alternate {
  build_id: string;
  runtime: string;
  when: { platform?: Platform; vendor?: string; min_driver?: string | null };
  model?: string;
}
export const altPlatform = (a: Alternate): Platform => storedPlatform(a.when.platform);

// Rev 17 (PROTOCOL 11.4): a fuzz campaign's worker build for one platform (at most one per platform).
export interface CampaignAlternate {
  build_id: string;
  when: { platform: Platform };
}

export const MAX_ALTERNATES = 4;
export const VENDOR_RE = /^[a-z0-9]{1,32}$/;

// Rev 22 (PROTOCOL 9.8): GPU vendors whose GPUs run only some providers - the owner's decision of 2026-11-20: AMD GPUs
// with DirectML only, never AMD's own Windows ML providers (amdgpu, migraphx: they failed on a farm PC's AMD GPU, and a
// probe of them very likely took that PC down; rev 12-21 ran no GPU work on AMD GPUs at all). A run alternate for such a
// vendor needs a runtime pack whose `gpu` requirement (PROTOCOL 9.2) names that vendor and exactly these providers, and
// that doesn't ship Windows ML's catalog (WINML_CATALOG_DLL, which can register the vendors' catalog providers); it is
// offered only to agents of at least AMD_DML_AGENT (which report AMD adapters again, DirectML only).
export const VENDOR_PROVIDERS: Readonly<Record<string, readonly Provider[]>> = { amd: ["dml"] };
export const WINML_CATALOG_DLL = "Microsoft.Windows.AI.MachineLearning.dll";
export const vendorRestricted = (vendor: string): boolean => Object.hasOwn(VENDOR_PROVIDERS, vendor);

// Rev 22 (PROTOCOL 9.8): the owner's kill switch - GPU vendors turned off without an agent release (settings
// `gpu_vendors_off`, POST /v1/admin/gpu_vendors_off). At most this many names.
export const MAX_VENDORS_OFF = 8;

// Rev 12 (PROTOCOL 2): at most this many build ids in the evict list (`settings.evict_builds`).
export const MAX_EVICT = 64;
export const DRIVER_RE = /^\d{1,9}(\.\d{1,9}){0,5}$/;

// `have` >= `min` for dotted driver versions ("560.94" >= "527.41"), numeric parts in order, missing parts 0. An
// empty or non-numeric version is unknown: false (the agent's rule, manifest.rs `driver_at_least`).
export function driverAtLeast(have: unknown, min: string): boolean {
  if (typeof have !== "string" || !DRIVER_RE.test(have.trim()) || !DRIVER_RE.test(min)) return false;
  const h = have.trim().split(".").map(Number);
  const m = min.split(".").map(Number);
  for (let i = 0; i < Math.max(h.length, m.length); i++) {
    const a = h[i] ?? 0;
    const b = m[i] ?? 0;
    if (a !== b) return a > b;
  }
  return true;
}

// The `gpu` of a runtime manifest (its stored JSON text), or null (none, or not a runtime manifest).
export function gpuNeedOf(manifestText: string | null | undefined): GpuNeed | null {
  if (!manifestText) return null;
  let m: any;
  try {
    m = JSON.parse(manifestText);
  } catch {
    return null;
  }
  const g = m?.gpu;
  if (m?.kind !== "runtime" || !g || typeof g !== "object" || typeof g.vendor !== "string") return null;
  return {
    vendor: g.vendor,
    min_driver: typeof g.min_driver === "string" ? g.min_driver : null,
    providers: Array.isArray(g.providers) ? (g.providers.filter(isProvider) as Provider[]) : [],
  };
}

// The file paths of a manifest (its stored JSON text); [] when it can't be read.
export function packFiles(manifestText: string | null | undefined): string[] {
  try {
    const m = JSON.parse(manifestText ?? "");
    return Array.isArray(m?.files) ? m.files.map((f: any) => f?.path).filter((x: unknown): x is string => typeof x === "string") : [];
  } catch {
    return [];
  }
}

// Rev 22 (PROTOCOL 9.8): why a runtime pack (its stored manifest text) may not serve an alternate of `vendor`, or null.
// Only vendors with a provider restriction (VENDOR_PROVIDERS: amd) have a rule: the pack's `gpu` requirement must name
// that vendor and exactly its providers (["dml"]), and the pack must not ship Windows ML's catalog.
export function vendorPackProblem(vendor: string, manifestText: string | null | undefined): string | null {
  if (!vendorRestricted(vendor)) return null;
  const only = VENDOR_PROVIDERS[vendor];
  let raw: unknown = null;
  try {
    raw = JSON.parse(manifestText ?? "")?.gpu ?? null;
  } catch {
    raw = null;
  }
  const g = raw && typeof raw === "object" ? (raw as Record<string, unknown>) : null;
  if (!g) return `${vendor} GPUs run ${only.join(", ")} only: the runtime needs a gpu requirement naming ${vendor} and exactly ${JSON.stringify(only)} (PROTOCOL 9.8)`;
  if (g.vendor !== vendor) return `the runtime is for ${String(g.vendor)} GPUs, not ${vendor}`;
  const provs = Array.isArray(g.providers) ? g.providers : [];
  if (provs.length !== only.length || !only.every((p) => provs.includes(p)) || new Set(provs).size !== provs.length) {
    return `${vendor} GPUs run ${only.join(", ")} only, but the runtime brings ${JSON.stringify(provs)} (PROTOCOL 9.8)`;
  }
  const cat = WINML_CATALOG_DLL.toLowerCase();
  if (packFiles(manifestText).some((f) => (f.split("/").pop() ?? "").toLowerCase() === cat)) {
    return `the runtime ships ${WINML_CATALOG_DLL}, which can register other providers on ${vendor} GPUs (PROTOCOL 9.8)`;
  }
  return null;
}

// The `requires_runtime` of a worker manifest (its stored JSON text), or null.
export function requiresRuntimeOf(manifestText: string | null | undefined): string | null {
  if (!manifestText) return null;
  try {
    const m = JSON.parse(manifestText);
    return m?.kind === "worker" && typeof m.requires_runtime === "string" ? m.requires_runtime : null;
  } catch {
    return null;
  }
}

// What vendor matching reads of a machine's stored profile.
export interface VendorProfile {
  gpus?: unknown;
}

// The machine has an adapter of `vendor` whose driver is at least every given minimum (null: none).
export function hasAdapter(profile: VendorProfile | null, vendor: string, mins: (string | null | undefined)[]): boolean {
  const gpus = Array.isArray(profile?.gpus) ? (profile!.gpus as Record<string, unknown>[]) : [];
  return gpus.some((g) => g && g.vendor === vendor && mins.every((m) => !m || driverAtLeast(g.driver, m)));
}

// The machine's probe result for one runtime pack: undefined when it has none (never probed there), else
// the providers that passed (possibly none: rev 11 sends [] for a pack that failed).
export function runtimeProvidersOf(profile: RoutingProfile | null, runtime: string): string[] | undefined {
  const per = profile?.runtime_providers;
  if (!per || typeof per !== "object" || Array.isArray(per) || !Object.hasOwn(per, runtime)) return undefined;
  return strings((per as Record<string, unknown>)[runtime]);
}

// Rev 22: a GPU provider can serve an adapter of `vendor` (its own vendor's, or vendor-neutral DirectML), and the vendor
// has no restriction that leaves it out (VENDOR_PROVIDERS: an AMD GPU runs dml only).
export function servesVendor(p: Provider, vendor: string): boolean {
  const v = PROVIDER_VENDOR[p];
  if (v === null || (v !== "any" && v !== vendor)) return false;
  return !vendorRestricted(vendor) || VENDOR_PROVIDERS[vendor].includes(p);
}

// The GPU providers an alternate's pack brings that the job's allowlist allows (a pack without `gpu`: every GPU
// provider the allowlist allows; rev 22, given the alternate's vendor: every one that can serve that vendor's GPUs).
export function altProviders(need: GpuNeed | null, allow: readonly string[] | null, vendor?: string): Provider[] {
  const brings = need ? need.providers.filter(isGpuProvider) : PROVIDERS.filter((p) => isGpuProvider(p) && (!vendor || servesVendor(p, vendor)));
  return brings.filter((p) => !allow || allow.includes(p));
}

// Rev 22: the stored kill switch (`settings.gpu_vendors_off`, its JSON text, or null: never set) as a set of names.
export function vendorsOffOf(text: string | null | undefined): Set<string> {
  try {
    const v = text ? JSON.parse(text) : [];
    return new Set(Array.isArray(v) ? v.filter((x): x is string => typeof x === "string" && VENDOR_RE.test(x)).slice(0, MAX_VENDORS_OFF) : []);
  } catch {
    return new Set();
  }
}

// GPU leases of `auto` jobs are at least this many games (PROTOCOL 9.4): a batch never has more rows than games
// in flight, and a GPU only pays with hundreds of rows per batch.
export const GPU_LEASE_MIN = { selfplay: 64, eval: 128 } as const;

// ---------- rev 18 (PROTOCOL 12): hardware routing ----------

// The owner's switch and knobs (settings key `hw_routing`, `GET/POST /v1/admin/routing`). `on` false: the order of
// before rev 18 (eval before self-play everywhere, CPU self-play in tier 1). `eval_horizon_s`: a PC that plays a
// run's self-play on its GPU takes an eval batch ahead of that self-play only while the CPU PCs online could not
// finish the batch's unissued games within this many seconds (0: always, the order of before). `eval_games_per_thread`:
// an `auto` eval lease on the CPU gets at least this many games per caps.thread (0: no floor).
// Rev 24 (PROTOCOL 15): `priority_lane` - "off": the behaviour of rev 23 exactly; "canary": waiting eval work preempts
// fuzz and rolling self-play (and the deferral reads capacity as 15.5 says) only on machines of the canary channel, and
// an idle machine asks again after LANE_WAIT_S while eval work is about to need it (every machine); "all": every
// machine. `eval_stall_s`: under the lane, a batch nobody has taken or worked on for this long is behind (0: no such rule).
export const PRIORITY_LANES = ["off", "canary", "all"] as const;
export type PriorityLane = (typeof PRIORITY_LANES)[number];
export interface HwRouting {
  on: boolean;
  eval_horizon_s: number;
  eval_games_per_thread: number;
  priority_lane: PriorityLane; // rev 24
  eval_stall_s: number; // rev 24
  fresh_cap: FreshCap; // rev 26
}
// Rev 26 (PROTOCOL 18.4): `fresh_cap` - which machines' rolling self-play leases are sent the run's freshness budget
// (`fresh_s`): "off" none (as before rev 26), "canary" the canary channel's, "all" every machine's (agents from FRESH_AGENT).
export const FRESH_CAPS = ["off", "canary", "all"] as const;
export type FreshCap = (typeof FRESH_CAPS)[number];
export const HW_ROUTING_DEFAULT: HwRouting = { on: true, eval_horizon_s: 1800, eval_games_per_thread: 2, priority_lane: "canary", eval_stall_s: 180, fresh_cap: "canary" };
export const MAX_EVAL_HORIZON_S = 7 * 86400;
export const MAX_EVAL_GAMES_PER_THREAD = 16;
export const MAX_EVAL_STALL_S = 86400;

// The stored setting (its JSON text, or null: never set) with every missing or unreadable field at its default.
export function hwRoutingOf(text: string | null | undefined): HwRouting {
  let v: any = null;
  try {
    v = text ? JSON.parse(text) : null;
  } catch {
    v = null;
  }
  const o = v && typeof v === "object" && !Array.isArray(v) ? v : {};
  const n = (x: unknown, max: number, def: number) => (typeof x === "number" && Number.isFinite(x) && x >= 0 && x <= max ? x : def);
  return {
    on: typeof o.on === "boolean" ? o.on : HW_ROUTING_DEFAULT.on,
    eval_horizon_s: n(o.eval_horizon_s, MAX_EVAL_HORIZON_S, HW_ROUTING_DEFAULT.eval_horizon_s),
    eval_games_per_thread: n(o.eval_games_per_thread, MAX_EVAL_GAMES_PER_THREAD, HW_ROUTING_DEFAULT.eval_games_per_thread),
    priority_lane: (PRIORITY_LANES as readonly unknown[]).includes(o.priority_lane) ? (o.priority_lane as PriorityLane) : HW_ROUTING_DEFAULT.priority_lane,
    eval_stall_s: n(o.eval_stall_s, MAX_EVAL_STALL_S, HW_ROUTING_DEFAULT.eval_stall_s),
    fresh_cap: (FRESH_CAPS as readonly unknown[]).includes(o.fresh_cap) ? (o.fresh_cap as FreshCap) : HW_ROUTING_DEFAULT.fresh_cap,
  };
}

// ---------- rev 24 (PROTOCOL 15): the priority lane ----------

// The lane's preemption (15.3, 15.4) and its reading of eval capacity (15.5) apply to a machine of this channel.
export function laneApplies(hw: HwRouting, channel: string): boolean {
  return hw.priority_lane === "all" || (hw.priority_lane === "canary" && channel === "canary");
}

// Rev 26 (PROTOCOL 18.4): a machine of this channel is sent its rolling leases' freshness budget.
export function freshApplies(hw: HwRouting, channel: string): boolean {
  return hw.fresh_cap === "all" || (hw.fresh_cap === "canary" && channel === "canary");
}

// A `no_work` answer while eval work is about to need the machine (15.2): ask again after this many seconds (300
// otherwise). The lease bucket allows one request per 5 s on average (ratelimit.ts), so this is 1/12 of it.
export const LANE_WAIT_S = 60;
// A run without self-play or a current batch, put this recently, is being set up (a campaign about to register its
// batch, 15.2).
export const LANE_SOON_S = 600;
// A fuzz lease that will be done within this many seconds at its own pace so far is not given back (15.3).
export const FUZZ_TAIL_S = 120;

// `leases.pending` of a lease on its way to an eval batch (15.3, 15.4): "yield:" + the batch's scope ("run:<run>").
export const YIELD_PREFIX = "yield:";
export const yieldTarget = (pending: string | null | undefined): string | null =>
  typeof pending === "string" && pending.startsWith(YIELD_PREFIX) ? pending.slice(YIELD_PREFIX.length) : null;

// Where a candidate sorts in a machine's order (friend.ts offer): tier, then rank, then score (higher first).
export interface SortKey {
  tier: number;
  rank: number;
  score: number;
}
export const sortsBefore = (a: SortKey, b: SortKey): boolean =>
  a.tier < b.tier || (a.tier === b.tier && (a.rank < b.rank || (a.rank === b.rank && a.score > b.score)));

// The most machines an eval batch can use now (15.3, 15.4): its unissued games over one machine's lease size.
export const batchMachines = (unissued: number, leaseSize: number): number => Math.ceil(unissued / Math.max(1, leaseSize));

// A deferred eval batch (a PC that plays self-play on its GPU, the batch not behind) ranks after that self-play
// (rank 1) and before a background job past its share (rank 2), still in tier 1 (before fuzz).
export const DEFERRED_EVAL_RANK = 1.5;
// Self-play a PC would play on its CPU while the run has a GPU version that other PCs play: after fuzz (tier 2).
export const CPU_SELFPLAY_TIER = 3;

// The memory a CPU eval lease's games take (PROTOCOL 10.6's CPU plan: 600 MB + 45 MB per game in flight).
export const CPU_BASE_MB = 600;
export const CPU_GAME_MB = 45;

// The fewest games an `auto` eval lease on the CPU gets (rev 18): `per_thread` x caps.threads, no more than the RAM the
// agent allows holds, within the eval lease limits. 0: no floor.
export function evalCpuFloor(perThread: number, threads: number, ramMb: number, min: number, max: number): number {
  if (perThread <= 0) return 0;
  const byRam = ramMb > 0 ? Math.floor((ramMb - CPU_BASE_MB) / CPU_GAME_MB) : Infinity;
  const n = Math.min(Math.floor(perThread * threads), byRam, max);
  return n >= min ? n : 0;
}

// Behind (rev 18): the CPU PCs online can't finish the batch's unissued games within the horizon at their eval rates
// (games per second, summed). No CPU capacity: behind while anything is unissued.
export function evalBehind(unissued: number, cpuRate: number, horizonS: number): boolean {
  return unissued > cpuRate * horizonS;
}
