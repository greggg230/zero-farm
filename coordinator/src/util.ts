// Shared helpers: errors, JSON answers, ids, hashing, input validation, semver.

export type Code =
  | "bad_request"
  | "unauthorized"
  | "revoked"
  | "not_found"
  | "conflict"
  | "gone"
  | "too_large"
  | "update_required"
  | "rate_limited"
  | "internal";

// An expected failure, answered as {"error": code, "message": text, ...extra}.
// A JSON value (what crosses the Durable Object RPC boundary and what jobs/stats carry).
export type Json = string | number | boolean | null | Json[] | { [k: string]: Json };

export class HttpError extends Error {
  constructor(
    public status: number,
    public code: Code,
    message: string,
    public extra: Record<string, Json> = {},
  ) {
    super(message);
  }
  toJSON(): Failure {
    return { status: this.status, code: this.code, message: this.message, extra: this.extra };
  }
}

export interface Failure {
  status: number;
  code: Code;
  message: string;
  extra: Record<string, Json>;
}

// Durable Object RPC loses custom error fields, so LeaseBook methods return this instead of throwing.
export type Outcome<T> = { ok: T } | { err: Failure };

export function unwrap<T>(o: Outcome<T>): T {
  if ("err" in o) throw new HttpError(o.err.status, o.err.code, o.err.message, o.err.extra);
  return o.ok;
}

export const bad = (message: string, extra: Record<string, Json> = {}) =>
  new HttpError(400, "bad_request", message, extra);
export const notFound = (what = "not found") => new HttpError(404, "not_found", what);
export const conflict = (message: string, extra: Record<string, Json> = {}) =>
  new HttpError(409, "conflict", message, extra);
export const gone = (message: string) => new HttpError(410, "gone", message);

export function json(body: unknown, status = 200, headers: Record<string, string> = {}): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json", "cache-control": "no-store", ...headers },
  });
}

export function errorResponse(e: HttpError): Response {
  const headers: Record<string, string> = {};
  if (typeof e.extra.retry_after === "number") headers["retry-after"] = String(e.extra.retry_after);
  return json({ error: e.code, message: e.message, ...e.extra }, e.status, headers);
}

// ---------- ids and secrets ----------

const CROCKFORD = "0123456789abcdefghjkmnpqrstvwxyz";
const INVITE_ALPHABET = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789"; // base32 without 0/1/O/I

function randomChars(n: number, alphabet: string): string {
  const bytes = crypto.getRandomValues(new Uint8Array(n));
  let s = "";
  for (const b of bytes) s += alphabet[b & 31];
  return s;
}

export const newMachineId = () => "m_" + randomChars(26, CROCKFORD);
export const newLeaseId = () => "l_" + randomChars(26, CROCKFORD);
export function newInviteCode(): string {
  const s = randomChars(12, INVITE_ALPHABET);
  return `${s.slice(0, 4)}-${s.slice(4, 8)}-${s.slice(8)}`;
}
export const INVITE_RE = /^[A-HJ-NP-Z2-9]{4}-[A-HJ-NP-Z2-9]{4}-[A-HJ-NP-Z2-9]{4}$/;
export const MACHINE_RE = /^m_[0-9a-hjkmnp-tv-z]{26}$/;
export const LEASE_RE = /^l_[0-9a-hjkmnp-tv-z]{26}$/;
export const SLUG_RE = /^[a-z0-9][a-z0-9-]{0,62}$/;
export const HEX64_RE = /^[0-9a-f]{64}$/;
export const BUILD_RE = /^[0-9a-f]{24}$/;

export function newToken(): string {
  return base64url(crypto.getRandomValues(new Uint8Array(32)));
}

export function base64url(bytes: Uint8Array): string {
  let bin = "";
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

export function hex(buf: ArrayBuffer | Uint8Array): string {
  const bytes = buf instanceof Uint8Array ? buf : new Uint8Array(buf);
  let s = "";
  for (const b of bytes) s += b.toString(16).padStart(2, "0");
  return s;
}

export async function sha256Hex(data: ArrayBuffer | Uint8Array | string): Promise<string> {
  const bytes = typeof data === "string" ? new TextEncoder().encode(data) : data;
  return hex(await crypto.subtle.digest("SHA-256", bytes));
}

// Compares two secrets without leaking where they differ.
export function safeEqual(a: string, b: string): boolean {
  const x = new TextEncoder().encode(a);
  const y = new TextEncoder().encode(b);
  let diff = x.length ^ y.length;
  for (let i = 0; i < Math.max(x.length, y.length); i++) diff |= (x[i] ?? 0) ^ (y[i] ?? 0);
  return diff === 0;
}

// R2 keys we accept from callers: relative, no "." or ".." segments, a plain character set.
export function checkKey(key: string): string {
  if (
    key.length === 0 ||
    key.length > 512 ||
    !/^[A-Za-z0-9._\-/]+$/.test(key) ||
    key.startsWith("/") ||
    key.endsWith("/") ||
    key.split("/").some((seg) => seg === "" || seg === "." || seg === "..")
  ) {
    throw bad(`bad key: ${JSON.stringify(key)}`);
  }
  return key;
}

// ---------- request bodies ----------

const MAX_JSON = 1 << 20;

export async function readJson(req: Request, max = MAX_JSON): Promise<Record<string, any>> {
  const bytes = await readBounded(req, max);
  let v: unknown;
  try {
    v = JSON.parse(new TextDecoder().decode(bytes));
  } catch {
    throw bad("body is not JSON");
  }
  if (!v || typeof v !== "object" || Array.isArray(v)) throw bad("body must be a JSON object");
  return v as Record<string, any>;
}

// Reads a body of at most `max` bytes; larger -> 413 too_large (checked before and while reading).
export async function readBounded(req: Request, max: number): Promise<Uint8Array> {
  const declared = req.headers.get("content-length");
  if (declared !== null && Number(declared) > max) {
    throw new HttpError(413, "too_large", `body larger than ${max} bytes`);
  }
  if (!req.body) return new Uint8Array(0);
  const reader = req.body.getReader();
  const parts: Uint8Array[] = [];
  let total = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    total += value.byteLength;
    if (total > max) {
      await reader.cancel();
      throw new HttpError(413, "too_large", `body larger than ${max} bytes`);
    }
    parts.push(value);
  }
  const out = new Uint8Array(total);
  let off = 0;
  for (const p of parts) {
    out.set(p, off);
    off += p.byteLength;
  }
  return out;
}

// ---------- validation ----------

type Opt = { optional?: boolean };

export function str(v: unknown, name: string, o: Opt & { max?: number; re?: RegExp; min?: number } = {}): string {
  if (v === undefined || v === null) {
    if (o.optional) return undefined as unknown as string;
    throw bad(`${name} is required`);
  }
  if (typeof v !== "string") throw bad(`${name} must be a string`);
  if (v.length < (o.min ?? 0)) throw bad(`${name} is too short`);
  if (v.length > (o.max ?? 256)) throw bad(`${name} is too long`);
  if (o.re && !o.re.test(v)) throw bad(`${name} is malformed: ${JSON.stringify(v)}`);
  return v;
}

export function int(v: unknown, name: string, o: Opt & { min?: number; max?: number } = {}): number {
  if (v === undefined || v === null) {
    if (o.optional) return undefined as unknown as number;
    throw bad(`${name} is required`);
  }
  if (typeof v !== "number" || !Number.isSafeInteger(v)) throw bad(`${name} must be an integer`);
  if (o.min !== undefined && v < o.min) throw bad(`${name} must be >= ${o.min}`);
  if (o.max !== undefined && v > o.max) throw bad(`${name} must be <= ${o.max}`);
  return v;
}

export function num(v: unknown, name: string, o: Opt & { min?: number; max?: number } = {}): number {
  if (v === undefined || v === null) {
    if (o.optional) return undefined as unknown as number;
    throw bad(`${name} is required`);
  }
  if (typeof v !== "number" || !Number.isFinite(v)) throw bad(`${name} must be a number`);
  if (o.min !== undefined && v < o.min) throw bad(`${name} must be >= ${o.min}`);
  if (o.max !== undefined && v > o.max) throw bad(`${name} must be <= ${o.max}`);
  return v;
}

export function bool(v: unknown, name: string, o: Opt = {}): boolean {
  if (v === undefined || v === null) {
    if (o.optional) return undefined as unknown as boolean;
    throw bad(`${name} is required`);
  }
  if (typeof v !== "boolean") throw bad(`${name} must be true or false`);
  return v;
}

export function obj(v: unknown, name: string, o: Opt = {}): Record<string, any> {
  if (v === undefined || v === null) {
    if (o.optional) return undefined as unknown as Record<string, any>;
    throw bad(`${name} is required`);
  }
  if (typeof v !== "object" || Array.isArray(v)) throw bad(`${name} must be an object`);
  return v as Record<string, any>;
}

export function oneOf<T extends string>(v: unknown, name: string, values: readonly T[], o: Opt = {}): T {
  if (v === undefined || v === null) {
    if (o.optional) return undefined as unknown as T;
    throw bad(`${name} is required`);
  }
  if (typeof v !== "string" || !values.includes(v as T)) throw bad(`${name} must be one of ${values.join(", ")}`);
  return v as T;
}

export function strList(v: unknown, name: string, o: Opt & { max?: number; itemMax?: number } = {}): string[] {
  if (v === undefined || v === null) {
    if (o.optional) return [];
    throw bad(`${name} is required`);
  }
  if (!Array.isArray(v) || v.some((x) => typeof x !== "string" || x.length > (o.itemMax ?? 256))) {
    throw bad(`${name} must be a list of strings`);
  }
  if (v.length > (o.max ?? 1000)) throw bad(`${name} has too many entries`);
  return v as string[];
}

// A JSON value we keep but don't interpret (job settings, env), bounded in size.
export function opaque(v: unknown, name: string, max = 64 * 1024): Json {
  const s = JSON.stringify(v);
  if (s === undefined) throw bad(`${name} must be JSON`);
  if (s.length > max) throw bad(`${name} is larger than ${max} bytes`);
  return v as Json;
}

// ---------- semver ----------

const SEMVER_RE = /^(\d{1,6})\.(\d{1,6})\.(\d{1,6})(?:-([0-9A-Za-z.-]{1,64}))?$/;

export function isSemver(v: string): boolean {
  return SEMVER_RE.test(v);
}

// -1, 0, 1. A pre-release sorts before its release (0.4.0-rc1 < 0.4.0).
export function cmpSemver(a: string, b: string): number {
  const x = SEMVER_RE.exec(a);
  const y = SEMVER_RE.exec(b);
  if (!x || !y) throw new Error(`not semver: ${a} / ${b}`);
  for (let i = 1; i <= 3; i++) {
    const d = Number(x[i]) - Number(y[i]);
    if (d) return Math.sign(d);
  }
  if (x[4] === y[4]) return 0;
  if (x[4] === undefined) return 1;
  if (y[4] === undefined) return -1;
  return x[4] < y[4] ? -1 : 1;
}

export const maxSemver = (vs: (string | null | undefined)[]): string | null =>
  vs.reduce<string | null>((m, v) => (v && (!m || cmpSemver(v, m) > 0) ? v : m), null);

export const clamp = (x: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, x));

export function parseJson<T>(s: string | null | undefined, fallback: T): T {
  if (s === null || s === undefined) return fallback;
  try {
    return JSON.parse(s) as T;
  } catch {
    return fallback;
  }
}
