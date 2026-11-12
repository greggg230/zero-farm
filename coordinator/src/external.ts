// Contributions of the owner's own machines that don't run the farm agent: the desktop playing
// locally, machines reached over SSH, rented boxes. A desktop reporter pushes their numbers
// (POST /v1/admin/external, PROTOCOL 4) about every 5 minutes; each POST replaces all sources of
// its reporter. A source whose reporter hasn't posted for 30 minutes shows as offline; rows are
// deleted 7 days after the last POST that named them. Shown on the admin page only, merged with
// the farm's PCs (never on a public page).

import { bad, int, num, obj, oneOf, str } from "./util";

export const EXTERNAL_KINDS = ["local", "ssh", "vast", "other"] as const;
export const OFFLINE_S = 30 * 60;
export const KEEP_S = 7 * 86400;
const MAX_SOURCES = 64;
const MAX_BODY = 64 * 1024;
const ID_RE = /^[a-z0-9][a-z0-9._-]{0,63}$/;
const WORK_RE = /^[A-Za-z0-9][A-Za-z0-9:._/-]{0,127}$/;
const MAX_COUNT = 1e12;

export interface Window {
  h1: number;
  h24: number;
}

export interface ExternalSource {
  id: string;
  name: string;
  kind: (typeof EXTERNAL_KINDS)[number];
  hardware: string | null;
  working_on: string[];
  selfplay: Window;
  eval: Window;
  fuzz: Window;
  busy_h_24h: number | null;
}

export interface ExternalReport {
  reporter: string;
  t: number;
  sources: ExternalSource[];
}

function window(v: unknown, name: string): Window {
  if (v === undefined || v === null) return { h1: 0, h24: 0 };
  const o = obj(v, name);
  for (const k of Object.keys(o)) if (k !== "1h" && k !== "24h") throw bad(`${name}.${k}: only "1h" and "24h"`);
  const h1 = int(o["1h"], `${name}.1h`, { optional: true, min: 0, max: MAX_COUNT }) ?? 0;
  const h24 = int(o["24h"], `${name}.24h`, { optional: true, min: 0, max: MAX_COUNT }) ?? 0;
  return { h1, h24 };
}

export const MAX_EXTERNAL_BODY = MAX_BODY;

export function parseReport(b: Record<string, any>): ExternalReport {
  const reporter = str(b.reporter, "reporter", { re: ID_RE });
  const t = int(b.t, "t", { min: 0, max: 2 ** 40 });
  if (!Array.isArray(b.sources)) throw bad("sources must be a list");
  if (b.sources.length > MAX_SOURCES) throw bad(`sources: at most ${MAX_SOURCES}`);
  const seen = new Set<string>();
  const sources = b.sources.map((v: unknown, i: number): ExternalSource => {
    const n = `sources[${i}]`;
    const s = obj(v, n);
    const id = str(s.id, `${n}.id`, { re: ID_RE });
    if (seen.has(id)) throw bad(`${n}.id ${id} is listed twice`);
    seen.add(id);
    const name = str(s.name, `${n}.name`, { min: 1, max: 64 }).trim();
    if (!name || /[\u0000-\u001f\u007f]/.test(name)) throw bad(`${n}.name must be printable text`);
    const hardware = str(s.hardware, `${n}.hardware`, { optional: true, max: 200 }) ?? null;
    if (hardware !== null && /[\u0000-\u001f\u007f]/.test(hardware)) throw bad(`${n}.hardware must be printable text`);
    const working = s.working_on ?? [];
    if (!Array.isArray(working) || working.length > 16 || working.some((w: unknown) => typeof w !== "string" || !WORK_RE.test(w))) {
      throw bad(`${n}.working_on must be a list of at most 16 names like "run:<run>" or "campaign:<id>"`);
    }
    return {
      id,
      name,
      kind: oneOf(s.kind, `${n}.kind`, EXTERNAL_KINDS),
      hardware,
      working_on: working,
      selfplay: window(s.selfplay_games, `${n}.selfplay_games`),
      eval: window(s.eval_games, `${n}.eval_games`),
      fuzz: window(s.fuzz_seeds, `${n}.fuzz_seeds`),
      busy_h_24h: num(s.busy_h_24h, `${n}.busy_h_24h`, { optional: true, min: 0, max: 24 }) ?? null,
    };
  });
  return { reporter, t, sources };
}

// Replaces every source of the report's reporter (one D1 batch: all or nothing).
export async function storeReport(db: D1Database, r: ExternalReport, now: number): Promise<number> {
  const stmts = [
    db.prepare(`DELETE FROM external_sources WHERE reporter = ?`).bind(r.reporter),
    ...r.sources.map((s) =>
      db
        .prepare(
          `INSERT INTO external_sources (reporter, id, name, kind, hardware, working_on, selfplay_1h, selfplay_24h, eval_1h, eval_24h,
             fuzz_1h, fuzz_24h, busy_h_24h, t, seen_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
        )
        .bind(
          r.reporter,
          s.id,
          s.name,
          s.kind,
          s.hardware,
          JSON.stringify(s.working_on),
          s.selfplay.h1,
          s.selfplay.h24,
          s.eval.h1,
          s.eval.h24,
          s.fuzz.h1,
          s.fuzz.h24,
          s.busy_h_24h,
          r.t,
          now,
        ),
    ),
  ];
  await db.batch(stmts);
  return r.sources.length;
}

export async function pruneExternal(db: D1Database, now: number): Promise<number> {
  return (await db.prepare(`DELETE FROM external_sources WHERE seen_at < ?`).bind(now - KEEP_S).run()).meta.changes ?? 0;
}

// ---------- the admin page's Contributors table ----------

export interface Contributor {
  origin: "farm" | "own";
  kind: "farm" | (typeof EXTERNAL_KINDS)[number];
  id: string; // machine_id, or "<reporter>/<source id>"
  name: string;
  hardware: string | null;
  status: "working" | "online" | "offline" | "paused" | "revoked";
  last_seen: number | null;
  working_on: string[];
  selfplay: Window;
  eval: Window;
  fuzz: Window;
  busy_h_24h: number | null;
}

export interface Totals {
  selfplay: Window;
  eval: Window;
  fuzz: Window;
}

const zero = (): Totals => ({ selfplay: { h1: 0, h24: 0 }, eval: { h1: 0, h24: 0 }, fuzz: { h1: 0, h24: 0 } });
const add = (a: Totals, b: Totals): Totals => ({
  selfplay: { h1: a.selfplay.h1 + b.selfplay.h1, h24: a.selfplay.h24 + b.selfplay.h24 },
  eval: { h1: a.eval.h1 + b.eval.h1, h24: a.eval.h24 + b.eval.h24 },
  fuzz: { h1: a.fuzz.h1 + b.fuzz.h1, h24: a.fuzz.h24 + b.fuzz.h24 },
});

export interface ExternalRow {
  reporter: string;
  id: string;
  name: string;
  kind: (typeof EXTERNAL_KINDS)[number];
  hardware: string | null;
  working_on: string;
  selfplay_1h: number;
  selfplay_24h: number;
  eval_1h: number;
  eval_24h: number;
  fuzz_1h: number;
  fuzz_24h: number;
  busy_h_24h: number | null;
  t: number;
  seen_at: number;
}

export function ownContributor(r: ExternalRow, now: number): Contributor {
  const online = now - r.seen_at <= OFFLINE_S;
  let working: string[] = [];
  try {
    working = JSON.parse(r.working_on);
  } catch {
    working = [];
  }
  return {
    origin: "own",
    kind: r.kind,
    id: `${r.reporter}/${r.id}`,
    name: r.name,
    hardware: r.hardware,
    status: online ? (working.length ? "working" : "online") : "offline",
    last_seen: r.seen_at,
    working_on: working,
    selfplay: { h1: r.selfplay_1h, h24: r.selfplay_24h },
    eval: { h1: r.eval_1h, h24: r.eval_24h },
    fuzz: { h1: r.fuzz_1h, h24: r.fuzz_24h },
    busy_h_24h: r.busy_h_24h,
  };
}

// Totals by origin. A source's last-hour numbers count while it is online; its 24-hour numbers
// while its last report is under a day old (an older report says nothing about the last day).
export function totals(cs: Contributor[], now: number): { farm: Totals; own: Totals; all: Totals } {
  let farm = zero();
  let own = zero();
  for (const c of cs) {
    if (c.origin === "farm") farm = add(farm, { selfplay: c.selfplay, eval: c.eval, fuzz: c.fuzz });
    else {
      const fresh = c.last_seen !== null && now - c.last_seen <= 86400;
      const on = c.status !== "offline";
      const w = (x: Window) => ({ h1: on ? x.h1 : 0, h24: fresh ? x.h24 : 0 });
      own = add(own, { selfplay: w(c.selfplay), eval: w(c.eval), fuzz: w(c.fuzz) });
    }
  }
  return { farm, own, all: add(farm, own) };
}
