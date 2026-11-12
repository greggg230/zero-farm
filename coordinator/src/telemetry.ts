import type { Json } from "./util";

// Per-beat performance samples (DESIGN 5.4) go to Workers Analytics Engine as a time series;
// the dashboard reads D1 summaries instead (a Worker can write to AE but can only query it
// through the account's SQL API). Without the binding (or if a write throws) it does nothing.

export interface Sample {
  machine_id: string;
  class: string | null;
  lease_id: string;
  kind: string;
  scope: string;
  event: "beat" | "done";
  stats: Record<string, unknown>;
}

export interface Telemetry {
  write(s: Sample): void;
}

// Numeric Stats fields, in the order they become AE doubles (append only: the order is the schema;
// AE takes at most 20).
export const DOUBLES = [
  "games",
  "decisions",
  "leaves",
  "infer_batches",
  "infer_ms_p50",
  "infer_ms_p95",
  "search_ms",
  "infer_ms",
  "wait_ms",
  "cpu_pct",
  "gpu_pct",
  "vram_mb",
  "rss_mb",
  "threads",
  "concurrency",
  "gpu_temp_c",
  "yields",
  "yield_ms_max",
  "selftest_max_abs",
  "starved_ms", // the worker got no CPU (the PC was busy) while not paused; counts toward slack
] as const;

// Numeric Stats fields kept in D1 (the lease's last Stats) but not sent to AE.
export const EXTRA_NUMS = ["paused_ms", "resume_ms_max"] as const;

const noop: Telemetry = { write() {} };

export function telemetry(ds: AnalyticsEngineDataset | undefined): Telemetry {
  if (!ds || typeof ds.writeDataPoint !== "function") return noop;
  return {
    write(s) {
      try {
        ds.writeDataPoint({
          indexes: [s.machine_id],
          blobs: [
            s.event,
            s.machine_id,
            s.class ?? "",
            s.kind,
            s.scope,
            s.lease_id,
            typeof s.stats.provider === "string" ? s.stats.provider : "",
            Array.isArray(s.stats.throttle) ? s.stats.throttle.join(",") : "",
          ],
          doubles: DOUBLES.map((k) => (typeof s.stats[k] === "number" ? (s.stats[k] as number) : 0)),
        });
      } catch {
        // Telemetry must never fail a beat.
      }
    },
  };
}

// Keeps only the Stats fields PROTOCOL 3 defines, with the right types, so a misbehaving agent
// can't fill D1 with junk.
export function cleanStats(v: unknown): Record<string, Json> | undefined {
  if (v === undefined || v === null || typeof v !== "object" || Array.isArray(v)) return undefined;
  const src = v as Record<string, unknown>;
  const out: Record<string, Json> = {};
  for (const k of [...DOUBLES, ...EXTRA_NUMS]) {
    const x = src[k];
    if (typeof x === "number" && Number.isFinite(x)) out[k] = x;
    else if (x === null) out[k] = null;
  }
  if (typeof src.provider === "string") out.provider = src.provider.slice(0, 32);
  if (Array.isArray(src.throttle)) out.throttle = src.throttle.filter((t) => typeof t === "string").slice(0, 16).map((t) => (t as string).slice(0, 32));
  return out;
}
