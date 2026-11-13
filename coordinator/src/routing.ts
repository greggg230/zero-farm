// Capability routing (PROTOCOL 8, rev 7; DESIGN 5.2): which machines a run's or campaign's jobs
// go to, from the job's `gpu` setting and `providers` allowlist and each machine's verified
// execution providers. Pure functions: friend.ts `lease()` asks them for each candidate.

import { bad } from "./util";

// Execution providers by their wire names, and the GPU vendor each serves (`dml`: any DirectX 12
// GPU). Every name but `cpu` is a GPU provider.
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

// A stored run job (`selfplay`/`eval` JSON) or campaign's routing settings; configs put before
// rev 7 have neither field (any GPU setting, any provider).
export function storedRouting(gpu: unknown, providers: unknown): Routing {
  const mode = (GPU_MODES as readonly unknown[]).includes(gpu) ? (gpu as GpuMode) : "any";
  const list = Array.isArray(providers) && providers.length ? (providers.filter(isProvider) as Provider[]) : null;
  return { gpu: mode, providers: list };
}
