// Sets of integer ids (game ids, fuzz seeds) as sorted, disjoint, half-open ranges [lo, hi).

import { bad } from "./util";

export type Range = [number, number];

export const total = (rs: Range[]) => rs.reduce((n, [lo, hi]) => n + (hi - lo), 0);

// Sorted, with overlapping and touching ranges merged; empty ranges dropped.
export function normalize(rs: Range[]): Range[] {
  const s = rs.filter(([lo, hi]) => hi > lo).sort((a, b) => a[0] - b[0] || a[1] - b[1]);
  const out: Range[] = [];
  for (const [lo, hi] of s) {
    const last = out[out.length - 1];
    if (last && lo <= last[1]) last[1] = Math.max(last[1], hi);
    else out.push([lo, hi]);
  }
  return out;
}

// a minus b (both normalized).
export function subtract(a: Range[], b: Range[]): Range[] {
  const out: Range[] = [];
  let j = 0;
  for (const [lo0, hi] of a) {
    let lo = lo0;
    while (j < b.length && b[j][1] <= lo) j++;
    let k = j;
    while (k < b.length && b[k][0] < hi) {
      if (b[k][0] > lo) out.push([lo, b[k][0]]);
      lo = Math.max(lo, b[k][1]);
      if (lo >= hi) break;
      k++;
    }
    if (lo < hi) out.push([lo, hi]);
  }
  return out;
}

// a intersect b (both normalized).
export function intersect(a: Range[], b: Range[]): Range[] {
  const out: Range[] = [];
  let i = 0;
  let j = 0;
  while (i < a.length && j < b.length) {
    const lo = Math.max(a[i][0], b[j][0]);
    const hi = Math.min(a[i][1], b[j][1]);
    if (lo < hi) out.push([lo, hi]);
    if (a[i][1] < b[j][1]) i++;
    else j++;
  }
  return out;
}

export const MAX_RANGES = 1024;

// Parses a caller's ranges ([[lo, hi], ...], half-open). Must lie inside `within` if given.
export function parseRanges(v: unknown, name: string, within?: Range): Range[] {
  if (!Array.isArray(v)) throw bad(`${name} must be a list of [lo, hi] ranges`);
  if (v.length > MAX_RANGES) throw bad(`${name} has more than ${MAX_RANGES} ranges`);
  const rs: Range[] = [];
  for (const r of v) {
    if (
      !Array.isArray(r) ||
      r.length !== 2 ||
      !Number.isSafeInteger(r[0]) ||
      !Number.isSafeInteger(r[1]) ||
      r[0] < 0 ||
      r[1] <= r[0]
    ) {
      throw bad(`${name}: ${JSON.stringify(r)} is not a [lo, hi) range with 0 <= lo < hi`);
    }
    if (within && (r[0] < within[0] || r[1] > within[1])) {
      throw bad(`${name}: ${JSON.stringify(r)} is outside this lease's ids [${within[0]}, ${within[1]})`);
    }
    rs.push([r[0], r[1]]);
  }
  const n = normalize(rs);
  if (total(n) !== total(rs)) throw bad(`${name} has overlapping ranges`);
  return n;
}
