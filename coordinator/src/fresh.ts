// Rev 26 (PROTOCOL 18): the freshness budget of a run's rolling self-play leases.
//
// The trainer keeps a farm row only while the network that searched it is at most `max_stale` generations behind the
// newest (zero-core's pool counts the lag from the newest gen, so a row survives about (max_stale + 1) generation
// periods after its network was registered). A game's first row is the oldest: it waits the whole game, then up to
// `flush_s` in the worker, and its network reached the agent up to a beat after it was registered. So a game that lasts
// longer than the budget below loses its first rows, and an agent that raises its games in flight past what keeps its
// games that short only makes rows the trainer drops. The coordinator works the budget out per run - never per machine -
// from the run's own pace, and the agent caps its games in flight at about its completed-games rate x the budget
// (Little's law).
//
//   period  = the median of the gaps between the run's last FRESH_GENS checkpoint registrations (by gen; only positive
//             gaps), with the open gap (now - the newest registration) counted as one more once it is longer than that
//             median (the trainer slowed down: its window is wider now); at least FRESH_MIN_GAPS gaps, else no budget;
//   fresh_s = floor(FRESH_MARGIN x (max_stale + 1) x period - flush_s - FRESH_DELAY_S), at least FRESH_MIN_S and at most
//             FRESH_MAX_S.
//
// legacy-meta3-r1 (max_stale 4, ~90-126 s a generation, flush_s 120): 0.75 x 5 x 90 - 180 = 157 s to 0.75 x 5 x 126 - 180
// = 292 s. Its measured drops of 2026-11-22 (pool `by_machine`): a laptop GPU 72 %, a Mac 25 %, CUDA desktops 12-14 %,
// with implied game latencies of 2-25 minutes - every one of them above the budget.

import type { RollingCfg, SelfplayCfg } from "./book";

export const FRESH_MARGIN = 0.75; // of the window: games vary in length around the mean Little's law caps
export const FRESH_DELAY_S = 60; // a new generation reaches the agent with its next beat (every 60 s)
export const FRESH_MIN_S = 120; // never a budget below two minutes (a cap that tight would starve every GPU)
export const FRESH_MAX_S = 86_400;
export const FRESH_GENS = 9; // registrations read: 8 gaps
export const FRESH_MIN_GAPS = 4; // fewer: the run has too little history, no budget

// The run's generation period (seconds) from its registration times, newest gen first; null with too little history.
export function genPeriod(ts: number[], now: number): number | null {
  const gaps = ts
    .slice(1)
    .map((t, i) => ts[i] - t)
    .filter((g) => g > 0);
  if (gaps.length < FRESH_MIN_GAPS) return null;
  const med = median(gaps);
  const open = ts.length ? now - ts[0] : 0;
  return open > med ? median([...gaps, open]) : med;
}

function median(xs: number[]): number {
  const s = [...xs].sort((a, b) => a - b);
  const m = s.length >> 1;
  return s.length % 2 ? s[m] : (s[m - 1] + s[m]) / 2;
}

// The budget (seconds) for a run's games of `max_stale` and `flush_s`, from its registration times (newest gen first).
export function freshBudget(ts: number[], maxStale: number, flushS: number, now: number): number | null {
  const period = genPeriod(ts, now);
  if (period === null) return null;
  const s = Math.floor(FRESH_MARGIN * (Math.max(0, maxStale) + 1) * period - flushS - FRESH_DELAY_S);
  return Math.min(FRESH_MAX_S, Math.max(FRESH_MIN_S, s));
}

// The budget of a run's rolling self-play now (null: not rolling, or too little history).
export async function runFresh(db: D1Database, run: string, sp: Pick<SelfplayCfg, "max_stale"> & { rolling?: RollingCfg } | null, now: number): Promise<number | null> {
  if (!sp || !sp.rolling) return null;
  const { results } = await db.prepare(`SELECT t FROM checkpoints WHERE run = ? ORDER BY gen DESC LIMIT ?`).bind(run, FRESH_GENS).all<{ t: number }>();
  return freshBudget(
    results.map((r) => r.t),
    sp.max_stale ?? 1,
    sp.rolling.flush_s,
    now,
  );
}
