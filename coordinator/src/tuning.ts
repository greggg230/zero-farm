// Rev 19 (PROTOCOL 13): layout tuning. Agents from 0.3.1 measure the CPU layout of their rolling self-play jobs
// (search threads, inference sessions x threads, groups, games in flight) on their own machine and report what they
// chose with each lease request (`tuning`); the coordinator keeps the latest report per machine, shows it per machine
// and per hardware class, and gives a rolling self-play lease to an agent that lists the feature "tuning" the best
// layout known for its hardware class and the lease's worker build and runtime pack (`tuning_hint`). A hint is
// unsigned and only a starting point: it changes how fast an agent plays, never what (the agent checks its bounds).

import { BUILD_RE, type Json, SLUG_RE, bad, int, num, obj, oneOf, parseJson, str } from "./util";

export const TUNING_FEATURE = "tuning";
export const MAX_TUNING = 4; // reports per lease request
export const NET_RE = /^[0-9a-f]{16}$/;
// A layout's bounds (the agent's own, tune.rs): threads and threads per session 1..512, sessions 1..64, groups and
// games in flight 1..4096, groups <= games in flight; worker processes 1..8 (13.5; absent: 1).
export const MAX_THREADS = 512;
export const MAX_SESSIONS = 64;
export const MAX_INFLIGHT = 4096;
export const MAX_WORKERS = 8;

export interface Layout {
  workers?: number; // worker processes serving the lease, each with the rest (13.5); absent: 1
  threads: number;
  infer_workers: number;
  infer_threads: number;
  groups: number;
  conc: number;
}

export interface TuningReport {
  run: string;
  build: string;
  runtime: string | null;
  net: string; // the agent's fingerprint of the network and its settings
  layout: Layout;
  leaves_s: number;
  trials: number;
  state: "tuned" | "searching";
  t: number;
}

export interface Hint {
  layout: Layout;
  leaves_s: number;
  machines: number; // machines of the class whose report has this build, runtime and network
  net: string;
}

export function cleanLayout(v: unknown, what: string): Layout {
  const o = obj(v, what);
  const l: Layout = {
    threads: int(o.threads, `${what}.threads`, { min: 1, max: MAX_THREADS }),
    infer_workers: int(o.infer_workers, `${what}.infer_workers`, { min: 1, max: MAX_SESSIONS }),
    infer_threads: int(o.infer_threads, `${what}.infer_threads`, { min: 1, max: MAX_THREADS }),
    groups: int(o.groups, `${what}.groups`, { min: 1, max: MAX_INFLIGHT }),
    conc: int(o.conc, `${what}.conc`, { min: 1, max: MAX_INFLIGHT }),
  };
  if (l.groups > l.conc) throw bad(`${what}: more groups than games in flight`);
  // (13.5: kept only when given, so a one-worker report reads as it always did)
  if (o.workers !== undefined && o.workers !== null) return { workers: int(o.workers, `${what}.workers`, { min: 1, max: MAX_WORKERS }), ...l };
  return l;
}

// POST /v1/lease's optional `tuning` (null or absent: nothing reported).
export function cleanTuning(v: unknown): TuningReport[] | null {
  if (v === undefined || v === null) return null;
  if (!Array.isArray(v) || v.length > MAX_TUNING) throw bad(`tuning must be a list of at most ${MAX_TUNING} reports`);
  return v.map((x, i) => {
    const w = `tuning[${i}]`;
    const o = obj(x, w);
    const runtime = o.runtime === null || o.runtime === undefined ? null : str(o.runtime, `${w}.runtime`, { re: BUILD_RE });
    return {
      run: str(o.run, `${w}.run`, { re: SLUG_RE }),
      build: str(o.build, `${w}.build`, { re: BUILD_RE }),
      runtime,
      net: str(o.net, `${w}.net`, { re: NET_RE }),
      layout: cleanLayout(o.layout, `${w}.layout`),
      leaves_s: num(o.leaves_s, `${w}.leaves_s`, { min: 0, max: 1e9 }),
      trials: int(o.trials, `${w}.trials`, { min: 0, max: 10000 }),
      state: oneOf(o.state, `${w}.state`, ["tuned", "searching"] as const),
      t: int(o.t, `${w}.t`, { min: 0 }),
    };
  });
}

// A stored report (machines.tuning); anything unreadable is nothing.
export function storedTuning(text: string | null): TuningReport[] {
  const v = parseJson<unknown>(text, null);
  try {
    return cleanTuning(v) ?? [];
  } catch {
    return [];
  }
}

// The best layout the reports of a hardware class know for a lease's build and runtime: reports of that build and
// runtime, the lease's run first (its network), tuned before searching, then the highest rate.
export function hintFor(reports: { machine_id: string; reports: TuningReport[] }[], build: string, runtime: string | null, run: string | null): Hint | null {
  const all = reports.flatMap((m) => m.reports.filter((r) => r.build === build && r.runtime === runtime).map((r) => ({ m: m.machine_id, r })));
  if (!all.length) return null;
  const score = (x: { r: TuningReport }) => [x.r.run === run ? 1 : 0, x.r.state === "tuned" ? 1 : 0, x.r.leaves_s] as const;
  const better = (a: readonly number[], b: readonly number[]) => a[0] - b[0] || a[1] - b[1] || a[2] - b[2];
  const best = all.reduce((b, x) => (better(score(x), score(b)) > 0 ? x : b));
  const machines = new Set(all.filter((x) => x.r.net === best.r.net).map((x) => x.m)).size;
  return { layout: best.r.layout, leaves_s: best.r.leaves_s, machines, net: best.r.net };
}

// Per hardware class (and platform), build, runtime and network: the best reported layout, its rate, how many machines
// report that key and which runs (the admin page and stats).
export function tuningClasses(rows: { machine_id: string; class: string | null; platform: string; tuning: string | null }[]): Json[] {
  const groups = new Map<string, { class: string | null; platform: string; build: string; runtime: string | null; net: string; best: TuningReport; machines: Set<string>; runs: Set<string> }>();
  for (const m of rows) {
    for (const r of storedTuning(m.tuning)) {
      const k = JSON.stringify([m.class, m.platform, r.build, r.runtime, r.net]);
      const g = groups.get(k) ?? { class: m.class, platform: m.platform, build: r.build, runtime: r.runtime, net: r.net, best: r, machines: new Set<string>(), runs: new Set<string>() };
      if (r.leaves_s > g.best.leaves_s) g.best = r;
      g.machines.add(m.machine_id);
      g.runs.add(r.run);
      groups.set(k, g);
    }
  }
  return [...groups.values()]
    .map((g) => ({
      class: g.class,
      platform: g.platform,
      build: g.build,
      runtime: g.runtime,
      net: g.net,
      layout: g.best.layout as unknown as Json,
      leaves_s: g.best.leaves_s,
      state: g.best.state,
      machines: g.machines.size,
      runs: [...g.runs].sort(),
    }))
    .sort((a, b) => String(a.class).localeCompare(String(b.class)) || a.build.localeCompare(b.build) || b.leaves_s - a.leaves_s);
}

// "8 + 4x2 thr, 2304 in 6 groups"; several workers (13.5): "2 x (12 + 4x3 thr, 1152 in 6 groups)".
export function describeLayout(l: Layout): string {
  const one = `${l.threads} + ${l.infer_workers}x${l.infer_threads} thr, ${l.conc} in ${l.groups} groups`;
  return (l.workers ?? 1) > 1 ? `${l.workers} x (${one})` : one;
}
