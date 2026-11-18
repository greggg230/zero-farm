// Rev 17 (PROTOCOL 11): platforms. Until rev 17 every agent, worker build and runtime pack was Windows x64 and nothing
// on the wire said so; an agent, manifest, release or alternate that names no platform is still `windows-x64`.

import { bad } from "./util";

export const PLATFORMS = ["windows-x64", "macos-arm64"] as const;
export type Platform = (typeof PLATFORMS)[number];
export const DEFAULT_PLATFORM: Platform = "windows-x64";
// Every request of an agent from PLATFORM_AGENT (0.3.0) carries it; absent: windows-x64 (every older agent).
export const PLATFORM_HEADER = "x-farm-platform";

export const isPlatform = (v: unknown): v is Platform => typeof v === "string" && (PLATFORMS as readonly string[]).includes(v);

// A platform field of a body or query (absent: windows-x64); an unknown one -> 400.
export function platformOf(v: unknown, name: string): Platform {
  if (v === undefined || v === null) return DEFAULT_PLATFORM;
  if (!isPlatform(v)) throw bad(`${name}: unknown platform ${JSON.stringify(String(v).slice(0, 64))} (known: ${PLATFORMS.join(", ")})`);
  return v;
}

// The platform a request's agent runs on (its x-farm-platform header); an unknown one -> 400.
export const requestPlatform = (req: Request): Platform => platformOf(req.headers.get(PLATFORM_HEADER)?.trim() ?? undefined, PLATFORM_HEADER);

// A stored platform column (rows from before migration 0013 read as windows-x64).
export const storedPlatform = (v: unknown): Platform => (isPlatform(v) ? v : DEFAULT_PLATFORM);
