// Worker settings: vars from wrangler.jsonc (public values), optional secrets (wrangler secret put),
// and the defaults the code falls back to when a var is unset or malformed.

import { isSemver } from "./util";

// Values that are never in wrangler.jsonc (secrets, or test-only switches), so `wrangler types`
// doesn't know them.
export interface Secrets {
  ADMIN_TOKEN?: string;
  IP_SALT?: string; // HMAC key for client IPs in rate-limit rows (ratelimit.ts)
  ACCESS_TEAM_DOMAIN?: string; // Cloudflare Access: "<team>" or "<team>.cloudflareaccess.com"
  ACCESS_AUD?: string; // the Access application's AUD tag (comma separated for several)
  ACCESS_EMAILS?: string; // optional allowlist of Access identities (comma separated)
  TEST_CLOCK?: string; // "1" in vitest only: honour x-test-now
}

export type AnyEnv = Env & Secrets;

export const DEFAULT_GITHUB_REPO = "greggg230/zero-farm";
export const DEFAULT_DEFINITION_AGENT = "0.2.5";
// Rev 11 (PROTOCOL 9): the first agent that takes run alternates and runtime packs with a GPU requirement.
export const DEFAULT_ALT_RUNTIME_AGENT = "0.2.10";
// Rev 15 (PROTOCOL 10.2): the first agent that takes rolling self-play leases (with `features: ["rolling"]`).
export const DEFAULT_ROLLING_AGENT = "0.2.13";
// Rev 17 (PROTOCOL 11): the first agent that knows platforms (x-farm-platform; manifests naming a platform need it).
export const DEFAULT_PLATFORM_AGENT = "0.3.0";
// Rev 20 (PROTOCOL 14): the first agent that knows model variants (it can read a definition that signs them).
export const DEFAULT_VARIANT_AGENT = "0.3.5";
// Rev 22 (PROTOCOL 9.8): the first agent that reports AMD adapters again and runs only DirectML on them; run alternates
// for AMD GPUs go only to it and newer.
export const DEFAULT_AMD_DML_AGENT = "0.3.11";
// Rev 26 (PROTOCOL 18): the first agent that caps its games in flight by the run's freshness budget (`fresh_s`); only it
// and newer are sent the budget.
export const DEFAULT_FRESH_AGENT = "0.3.17";
// Rev 27 (PROTOCOL 19): the first agent that takes opt-in runtime packs (a manifest `opt_in`, the `torch-cuda` provider,
// a python worker behind `zf-pyshim.exe`); opt-in packs and alternates with `when.opt_in` go only to it and newer.
export const DEFAULT_TORCH_AGENT = "0.3.19";

export interface Config {
  githubRepo: string; // owner/repo whose agent-v<version> releases installers may come from
  dataPubkeys: string[]; // minisign public keys that may sign definitions (PROTOCOL 7)
  definitionAgent: string; // the first agent that verifies definitions
  altRuntimeAgent: string; // rev 11: the first agent that takes run alternates (PROTOCOL 9)
  rollingAgent: string; // rev 15: the first agent that takes rolling self-play leases (PROTOCOL 10)
  platformAgent: string; // rev 17: the first agent that knows platforms (PROTOCOL 11)
  variantAgent: string; // rev 20: the first agent that knows model variants (PROTOCOL 14)
  amdDmlAgent: string; // rev 22: the first agent that runs AMD GPUs with DirectML only (PROTOCOL 9.8)
  freshAgent: string; // rev 26: the first agent sent the freshness budget (PROTOCOL 18)
  torchAgent: string; // rev 27: the first agent that takes opt-in runtime packs (PROTOCOL 19)
  maxActiveLeases: number; // per machine
  maxLeaseChunks: number; // result chunks per lease
  maxLeaseBytes: number; // result bytes per lease
  lateResultsS: number; // results are taken until this long after a lease closed
  maxCrashesPerDay: number; // crash rows kept per machine per 24 h
  crashRetentionDays: number;
  diagRetentionDays: number;
}

function intVar(v: unknown, def: number, min: number, max: number): number {
  const n = typeof v === "string" && /^\d{1,15}$/.test(v.trim()) ? Number(v.trim()) : typeof v === "number" ? v : NaN;
  return Number.isSafeInteger(n) && n >= min && n <= max ? n : def;
}

export const list = (v: unknown): string[] =>
  String(v ?? "")
    .split(",")
    .map((x) => x.trim())
    .filter(Boolean);

export function config(env: object): Config {
  const e = env as Record<string, unknown>;
  const repo = String(e.GITHUB_REPO ?? "").trim();
  const defAgent = String(e.DEFINITION_AGENT ?? "").trim();
  const altAgent = String(e.ALT_RUNTIME_AGENT ?? "").trim();
  const rollAgent = String(e.ROLLING_AGENT ?? "").trim();
  const platAgent = String(e.PLATFORM_AGENT ?? "").trim();
  const varAgent = String(e.VARIANT_AGENT ?? "").trim();
  const amdAgent = String(e.AMD_DML_AGENT ?? "").trim();
  const freshAgent = String(e.FRESH_AGENT ?? "").trim();
  const torchAgent = String(e.TORCH_AGENT ?? "").trim();
  return {
    githubRepo: /^[A-Za-z0-9_.-]{1,100}\/[A-Za-z0-9_.-]{1,100}$/.test(repo) ? repo : DEFAULT_GITHUB_REPO,
    dataPubkeys: list(e.DATA_PUBKEYS),
    definitionAgent: isSemver(defAgent) ? defAgent : DEFAULT_DEFINITION_AGENT,
    altRuntimeAgent: isSemver(altAgent) ? altAgent : DEFAULT_ALT_RUNTIME_AGENT,
    rollingAgent: isSemver(rollAgent) ? rollAgent : DEFAULT_ROLLING_AGENT,
    platformAgent: isSemver(platAgent) ? platAgent : DEFAULT_PLATFORM_AGENT,
    variantAgent: isSemver(varAgent) ? varAgent : DEFAULT_VARIANT_AGENT,
    amdDmlAgent: isSemver(amdAgent) ? amdAgent : DEFAULT_AMD_DML_AGENT,
    freshAgent: isSemver(freshAgent) ? freshAgent : DEFAULT_FRESH_AGENT,
    torchAgent: isSemver(torchAgent) ? torchAgent : DEFAULT_TORCH_AGENT,
    maxActiveLeases: intVar(e.MAX_ACTIVE_LEASES, 2, 1, 1000),
    maxLeaseChunks: intVar(e.MAX_LEASE_CHUNKS, 2000, 1, 1_000_000),
    maxLeaseBytes: intVar(e.MAX_LEASE_BYTES, 1024 ** 3, 1, 2 ** 50),
    lateResultsS: intVar(e.LATE_RESULTS_S, 86400, 0, 365 * 86400),
    maxCrashesPerDay: intVar(e.MAX_CRASHES_PER_DAY, 100, 1, 1_000_000),
    crashRetentionDays: intVar(e.CRASH_RETENTION_DAYS, 30, 1, 3650),
    diagRetentionDays: intVar(e.DIAG_RETENTION_DAYS, 14, 1, 3650),
  };
}
