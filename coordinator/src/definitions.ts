// Signed definitions (PROTOCOL 7, rev 6). Everything a lease tells a PC to download or run is fixed
// by a definition the desktop signed with a data key (never held here): worker builds, the runtime
// pack, checkpoint files (sha256 + size) and the exact job templates. The coordinator registers a
// definition only if its signature verifies against DATA_PUBKEYS, and issues a lease for a run or
// campaign only with builds, runtime, checkpoints and a job kind that its current definition
// names. A definition lease's `job` carries only the per-lease fields; the agent builds the job
// line itself from the signed template.
//
// Rollout: agents older than DEFINITION_AGENT get legacy leases (no definition, the coordinator's
// own job object) and only for a run or campaign that has no definition yet. Agents from
// DEFINITION_AGENT on get leases only with a definition.

import type { Json } from "./util";
import { BUILD_RE, HEX64_RE, SLUG_RE, bad, parseJson } from "./util";

export const DEF_KINDS = ["run", "campaign"] as const;
export type DefKind = (typeof DEF_KINDS)[number];
export type JobKind = "selfplay" | "eval" | "fuzz";

// The same rules as the agent's (agent/common/src/definition.rs, `Definition::validate` and
// `check_template`), so the coordinator never registers a definition every agent would refuse.
export const MAX_DEFINITION = 1 << 20; // bytes of definition.json
export const MAX_SIGNATURE = 4096; // bytes of its .minisig
const MAX_TEMPLATE = 64 * 1024; // bytes of one job template
const MAX_CHECKPOINTS = 1024;
const MAX_FILE = 1 << 30; // a checkpoint file: 1 byte to 1 GiB
export const MAX_SEATS = 16;
const BOT_RE = /^[A-Za-z0-9][A-Za-z0-9_.:-]{0,63}$/;
const GEN_RE = /^(0|[1-9]\d{0,8})$/;
const MODE_RE = /^[A-Za-z0-9_.-]{1,128}$/;
const ENV_RE = /^[A-Za-z0-9_]{1,64}$/;
const TOP_KEYS = ["definition", "kind", "id", "version", "created", "expires", "game", "builds", "runtime", "checkpoints", "jobs"];
const CKPT_KEYS = ["model_sha256", "model_size", "reference_sha256", "reference_size"];
// The keys a job template may have, per kind. Per-lease fields (`gid_base`, `games`, `seed`,
// `first`, `count`) never: the coordinator sets those on each lease.
const TEMPLATE_KEYS: Record<JobKind, string[]> = {
  selfplay: ["settings"],
  eval: ["settings", "seats", "paired"],
  fuzz: ["mode", "env", "announce_result"],
};

// A string a worker could take for a file path: separators, "..", a drive ("C:"). Templates
// carry none (the agent refuses them: paths on a job line are only ever its own).
export function looksLikePath(s: string): boolean {
  return s.includes("/") || s.includes("\\") || s.includes("..") || /^[A-Za-z]:/.test(s);
}

function noPaths(v: unknown, at: string, out: string[], depth = 0): void {
  if (depth > 32) {
    out.push(`${at}: nested too deeply`);
    return;
  }
  if (typeof v === "string") {
    if (looksLikePath(v)) out.push(`${at}: ${JSON.stringify(v.slice(0, 60))} looks like a file path`);
  } else if (Array.isArray(v)) v.forEach((x, i) => noPaths(x, `${at}[${i}]`, out, depth + 1));
  else if (v && typeof v === "object") {
    for (const [k, x] of Object.entries(v)) {
      if (looksLikePath(k)) out.push(`${at}: key ${JSON.stringify(k.slice(0, 60))} looks like a file path`);
      noPaths(x, `${at}.${k}`, out, depth + 1);
    }
  }
}

export const defKey = (kind: DefKind, id: string, version: number) => `defs/${kind}/${id}/${version}/definition.json`;
export const DEF_KEY_RE = /^defs\/(run|campaign)\/([a-z0-9][a-z0-9-]{0,62})\/([1-9]\d{0,14})\/definition\.json(\.minisig)?$/;

export interface DefCheckpoint {
  model_sha256: string;
  model_size: number;
  reference_sha256: string | null;
  reference_size: number | null;
}

export interface DefJobs {
  selfplay: Record<string, Json> | null;
  eval: Record<string, Json> | null;
  fuzz: Record<string, Json> | null;
}

// definition/1 as signed.
export interface DefinitionDoc {
  definition: 1;
  kind: DefKind;
  id: string;
  version: number;
  created: number;
  expires: number | null;
  game: string;
  builds: string[];
  runtime: string | null;
  checkpoints: Record<string, DefCheckpoint>;
  jobs: DefJobs;
}

// A registered definition (a `definitions` row, parsed).
export interface Def extends Omit<DefinitionDoc, "definition"> {
  key: string;
  sha256: string;
  size: number;
  key_id: string;
  registered_at: number;
}

export interface DefinitionRow {
  kind: DefKind;
  id: string;
  version: number;
  key: string;
  sha256: string;
  size: number;
  created: number;
  expires: number | null;
  game: string;
  builds: string;
  runtime: string | null;
  checkpoints: string;
  jobs: string;
  key_id: string;
  registered_at: number;
}

export const defRef = (d: Pick<Def, "key" | "sha256">) => ({ key: d.key, sha256: d.sha256 });

export function defOf(r: DefinitionRow): Def {
  return {
    kind: r.kind,
    id: r.id,
    version: r.version,
    key: r.key,
    sha256: r.sha256,
    size: r.size,
    created: r.created,
    expires: r.expires,
    game: r.game,
    builds: parseJson<string[]>(r.builds, []),
    runtime: r.runtime,
    checkpoints: parseJson<Record<string, DefCheckpoint>>(r.checkpoints, {}),
    jobs: parseJson<DefJobs>(r.jobs, { selfplay: null, eval: null, fuzz: null }),
    key_id: r.key_id,
    registered_at: r.registered_at,
  };
}

// ---------- reading ----------

export async function currentDef(db: D1Database, kind: DefKind, id: string): Promise<Def | null> {
  const r = await db.prepare(`SELECT * FROM definitions WHERE kind = ? AND id = ? ORDER BY version DESC LIMIT 1`).bind(kind, id).first<DefinitionRow>();
  return r ? defOf(r) : null;
}

export async function defAt(db: D1Database, kind: DefKind, id: string, version: number): Promise<Def | null> {
  const r = await db.prepare(`SELECT * FROM definitions WHERE kind = ? AND id = ? AND version = ?`).bind(kind, id, version).first<DefinitionRow>();
  return r ? defOf(r) : null;
}

// The current definition of every run and campaign that has one, by "<kind>:<id>".
export async function currentDefs(db: D1Database): Promise<Map<string, Def>> {
  const { results } = await db
    .prepare(
      `SELECT d.* FROM definitions d JOIN (SELECT kind, id, MAX(version) AS v FROM definitions GROUP BY kind, id) m
       ON d.kind = m.kind AND d.id = m.id AND d.version = m.v`,
    )
    .all<DefinitionRow>();
  return new Map(results.map((r) => [`${r.kind}:${r.id}`, defOf(r)]));
}

export const scopeDefKey = (scope: string): string | null => {
  const m = /^(run|campaign):(.+)$/.exec(scope);
  return m ? `${m[1]}:${m[2]}` : null;
};

// ---------- checking a definition ----------

const isObj = (v: unknown): v is Record<string, any> => !!v && typeof v === "object" && !Array.isArray(v);
const isInt = (v: unknown, min = 0): v is number => typeof v === "number" && Number.isSafeInteger(v) && v >= min;

// Seats of an eval job (PROTOCOL 5.2): 2 to 16, each exactly one of {"net": gen} and {"bot": name},
// no `ext:` bots. Returns the gens they name; problems go to `out`.
export function seatGens(seats: unknown, name: string, out: string[]): number[] {
  const gens: number[] = [];
  if (!Array.isArray(seats) || seats.length < 2 || seats.length > MAX_SEATS) {
    out.push(`${name} must be a list of 2 to ${MAX_SEATS} seats ({"net": gen} or {"bot": name})`);
    return gens;
  }
  seats.forEach((s, i) => {
    if (!isObj(s)) return out.push(`${name}[${i}] must be an object`);
    const hasNet = s.net !== undefined && s.net !== null;
    const hasBot = s.bot !== undefined && s.bot !== null;
    if (hasNet === hasBot) return out.push(`${name}[${i}] needs exactly one of net and bot`);
    if (hasNet) {
      if (!isInt(s.net) || s.net > 999_999_999) return out.push(`${name}[${i}].net must be a checkpoint gen`);
      gens.push(s.net);
    } else if (typeof s.bot !== "string" || !BOT_RE.test(s.bot)) out.push(`${name}[${i}].bot is malformed`);
    else if (s.bot.toLowerCase().startsWith("ext:")) out.push(`${name}[${i}]: ext: bots can't run on the farm (${s.bot})`);
  });
  return gens;
}

function template(kind: JobKind, v: unknown, out: string[]): Record<string, Json> | null {
  const name = `jobs.${kind}`;
  if (v === undefined || v === null) return null;
  if (!isObj(v)) {
    out.push(`${name} must be an object or null`);
    return null;
  }
  if (JSON.stringify(v).length > MAX_TEMPLATE) out.push(`${name} is larger than ${MAX_TEMPLATE} bytes`);
  for (const k of Object.keys(v)) {
    if (!TEMPLATE_KEYS[kind].includes(k)) out.push(`${name}.${k} is not allowed (a ${kind} template has ${TEMPLATE_KEYS[kind].join(", ")}; per-lease fields are the coordinator's)`);
  }
  if (kind === "selfplay" || kind === "eval") {
    if (!isObj(v.settings)) out.push(`${name}.settings must be an object`);
  }
  if (kind === "eval" && typeof v.paired !== "boolean") out.push(`${name}.paired must be true or false`);
  if (kind === "fuzz") {
    if (typeof v.mode !== "string" || !MODE_RE.test(v.mode)) out.push(`${name}.mode must be a mode name`);
    if (v.env !== undefined) {
      if (!isObj(v.env) || Object.keys(v.env).length > 64) out.push(`${name}.env must be an object of at most 64 variables`);
      else {
        for (const [k, x] of Object.entries(v.env)) {
          if (!ENV_RE.test(k) || typeof x !== "string" || new TextEncoder().encode(x).length > 4096) {
            out.push(`${name}.env.${k} must be a variable of at most 4096 bytes`);
          }
        }
      }
    }
    if (v.announce_result !== undefined && typeof v.announce_result !== "boolean") out.push(`${name}.announce_result must be true or false`);
  }
  noPaths(v, name, out);
  return v as Record<string, Json>;
}

// Parses and checks definition.json (PROTOCOL 7). Throws 400 with every problem found.
export function parseDefinition(text: string): DefinitionDoc {
  const p: string[] = [];
  let d: any;
  try {
    d = JSON.parse(text);
  } catch {
    throw bad("definition.json is not JSON");
  }
  if (!isObj(d)) throw bad("definition.json must be an object");
  for (const k of Object.keys(d)) if (!TOP_KEYS.includes(k)) p.push(`${k}: unknown field`);
  if (d.definition !== 1) p.push("definition must be 1");
  if (!(DEF_KINDS as readonly unknown[]).includes(d.kind)) p.push("kind must be run or campaign");
  if (typeof d.id !== "string" || !SLUG_RE.test(d.id)) p.push("id must be a run or campaign slug");
  if (!isInt(d.version, 1)) p.push("version must be a positive integer");
  if (!isInt(d.created, 1)) p.push("created must be Unix seconds");
  if (d.expires !== undefined && d.expires !== null && (!isInt(d.expires) || (isInt(d.created) && d.expires <= d.created))) {
    p.push("expires must be Unix seconds after created, or null");
  }
  if (typeof d.game !== "string" || !SLUG_RE.test(d.game)) p.push("game must be a slug");
  if (!Array.isArray(d.builds) || !d.builds.length || d.builds.length > 64 || d.builds.some((b: unknown) => typeof b !== "string" || !BUILD_RE.test(b))) {
    p.push("builds must be a list of 1 to 64 worker build ids");
  } else if (new Set(d.builds).size !== d.builds.length) p.push("builds lists a build twice");
  if (d.runtime !== undefined && d.runtime !== null && (typeof d.runtime !== "string" || !BUILD_RE.test(d.runtime))) p.push("runtime must be a build id or null");

  const checkpoints: Record<string, DefCheckpoint> = {};
  if (d.checkpoints !== undefined && d.checkpoints !== null) {
    if (!isObj(d.checkpoints)) p.push("checkpoints must be an object {gen: {...}}");
    else {
      const entries = Object.entries(d.checkpoints);
      if (entries.length > MAX_CHECKPOINTS) p.push(`checkpoints has more than ${MAX_CHECKPOINTS} gens`);
      for (const [gen, c] of entries) {
        const n = `checkpoints.${gen}`;
        if (!GEN_RE.test(gen)) p.push(`${n}: a gen must be a non-negative integer`);
        if (!isObj(c)) {
          p.push(`${n} must be an object`);
          continue;
        }
        for (const k of Object.keys(c)) if (!CKPT_KEYS.includes(k)) p.push(`${n}.${k}: unknown field`);
        const size = (x: unknown) => isInt(x, 1) && x <= MAX_FILE;
        if (typeof c.model_sha256 !== "string" || !HEX64_RE.test(c.model_sha256)) p.push(`${n}.model_sha256 must be lowercase hex sha256`);
        if (!size(c.model_size)) p.push(`${n}.model_size must be 1 to ${MAX_FILE} bytes`);
        const ref = c.reference_sha256 ?? null;
        const refSize = c.reference_size ?? null;
        if (ref !== null && (typeof ref !== "string" || !HEX64_RE.test(ref))) p.push(`${n}.reference_sha256 must be lowercase hex sha256 or null`);
        if (ref !== null && !size(refSize)) p.push(`${n}.reference_size must be 1 to ${MAX_FILE} bytes`);
        if (ref === null && refSize !== null) p.push(`${n}.reference_size without reference_sha256`);
        checkpoints[gen] = { model_sha256: c.model_sha256, model_size: c.model_size, reference_sha256: ref, reference_size: refSize };
      }
    }
  }

  const jobs: DefJobs = { selfplay: null, eval: null, fuzz: null };
  if (!isObj(d.jobs)) p.push("jobs must be an object {selfplay, eval, fuzz}");
  else {
    for (const k of Object.keys(d.jobs)) if (!["selfplay", "eval", "fuzz"].includes(k)) p.push(`jobs.${k}: unknown job kind`);
    jobs.selfplay = template("selfplay", d.jobs.selfplay, p);
    jobs.eval = template("eval", d.jobs.eval, p);
    jobs.fuzz = template("fuzz", d.jobs.fuzz, p);
    if (jobs.eval) {
      const gens = seatGens(jobs.eval.seats, "jobs.eval.seats", p);
      for (const g of gens) if (!checkpoints[String(g)]) p.push(`jobs.eval.seats name gen ${g}, which checkpoints doesn't list`);
    }
    if (!jobs.selfplay && !jobs.eval && !jobs.fuzz) p.push("jobs names no job (selfplay, eval or fuzz)");
    if (d.kind === "run" && jobs.fuzz) p.push("a run's definition has no fuzz job (campaigns fuzz)");
    if (d.kind === "campaign" && (jobs.selfplay || jobs.eval)) p.push("a campaign's definition has only a fuzz job");
  }
  if (d.kind === "campaign" && Object.keys(checkpoints).length) p.push("a campaign's definition has no checkpoints");
  if (d.kind === "campaign" && d.runtime) p.push("a campaign's definition has no runtime (fuzz leases carry none)");
  if (p.length) throw bad("definition.json is not a valid definition/1", { problems: p });
  return {
    definition: 1,
    kind: d.kind,
    id: d.id,
    version: d.version,
    created: d.created,
    expires: d.expires ?? null,
    game: d.game,
    builds: d.builds,
    runtime: d.runtime ?? null,
    checkpoints,
    jobs,
  };
}

// ---------- what a definition allows ----------

// Why a `kind` lease with this build and runtime can't be issued under `d` (null: it can). The
// checkpoints are checked separately (checkpointProblem, evalProblem).
export function leaseProblem(d: Def, kind: JobKind, build: string, runtime: string | null, now: number): string | null {
  if (d.expires !== null && d.expires <= now) return `definition v${d.version} expired`;
  if (!d.jobs[kind]) return `definition v${d.version} has no ${kind} job`;
  if (!d.builds.includes(build)) return `build ${build} is not in definition v${d.version}`;
  if ((d.runtime ?? null) !== (runtime ?? null)) return `runtime ${runtime ?? "none"} is not definition v${d.version}'s (${d.runtime ?? "none"})`;
  return null;
}

export interface CkptFacts {
  gen: number;
  sha256: string;
  size: number | null;
  reference_sha256: string | null;
  reference_size: number | null;
}

// Why checkpoint `c` (as registered) doesn't match what `d` signs for its gen (null: it does).
export function checkpointProblem(d: Def, c: CkptFacts): string | null {
  const e = d.checkpoints[String(c.gen)];
  if (!e) return `checkpoint ${c.gen} is not in definition v${d.version}`;
  if (e.model_sha256 !== c.sha256 || (c.size !== null && e.model_size !== c.size)) return `checkpoint ${c.gen}'s model differs from definition v${d.version}`;
  if ((e.reference_sha256 ?? null) !== (c.reference_sha256 ?? null)) return `checkpoint ${c.gen}'s reference differs from definition v${d.version}`;
  if (c.reference_size !== null && e.reference_size !== null && e.reference_size !== c.reference_size) {
    return `checkpoint ${c.gen}'s reference size differs from definition v${d.version}`;
  }
  return null;
}

// Why an eval batch can't be leased under `d`: its networks must be the ones the signed seats name
// (the agent seats them from the definition) and pairing must agree (the coordinator credits pairs).
export function evalProblem(d: Def, batch: { checkpoints: number[]; paired: boolean }): string | null {
  const t = d.jobs.eval;
  if (!t) return `definition v${d.version} has no eval job`;
  const gens = [...new Set(seatGens(t.seats, "seats", []))].sort((a, b) => a - b).join(",");
  const want = [...new Set(batch.checkpoints)].sort((a, b) => a - b).join(",");
  if (gens !== want) return `the batch's networks [${want}] are not the ones definition v${d.version}'s seats name [${gens}]`;
  if ((t.paired === true) !== batch.paired) return `the batch is ${batch.paired ? "" : "not "}paired, definition v${d.version}'s eval job is ${t.paired === true ? "" : "not "}`;
  return null;
}

// The base seed of a run's leases (PROTOCOL 5): the template's `seed` if it sets one, else derived
// from the run's name. The same for every lease of the run.
export function baseSeed(name: string, job: Record<string, Json> | null | undefined): number {
  if (job && typeof job.seed === "number" && Number.isSafeInteger(job.seed)) return job.seed;
  let h = 0x811c9dc5;
  for (let i = 0; i < name.length; i++) {
    h ^= name.charCodeAt(i);
    h = Math.imul(h, 0x01000193) >>> 0;
  }
  return h;
}
