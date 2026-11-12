// Per-machine request rate limits (PROTOCOL 1): a buggy agent must not be able to hammer the
// coordinator. On 2026-11-11 an agent bug on a real PC re-leased about 8 times a second for 9
// minutes; with these limits it would have got 429s after its burst.
//
// GCRA (the token bucket written as one timestamp per key): `interval` seconds per request on
// average, up to `burst` requests at once. State is one D1 row per (key, bucket), changed by one
// conditional upsert, so concurrent requests can't both take the last slot. A limited request
// changes nothing. Answer: 429 `rate_limited` with `retry-after` (seconds) and `retry_after`.

import type { Ctx } from "./ctx";
import { HttpError, base64url } from "./util";

export interface Limit {
  interval: number; // seconds per request, on average
  burst: number; // requests allowed at once
}

// A well-behaved agent asks for a lease once per answer's wait_s (300 s) or per finished lease
// (~450 s of work), plus a quick retry after a 409 need_profile; it beats every 60 s per lease.
export const LIMITS = {
  lease: { interval: 5, burst: 6 },
  beat: { interval: 10, burst: 12 },
  crash: { interval: 10, burst: 30 }, // an agent sends up to 20 saved reports at start
  enroll: { interval: 60, burst: 10 }, // per client IP: a few typos are fine, guessing is not
  diag_user: { interval: 3600, burst: 1 }, // the friend's "Send diagnostics": once an hour
} as const satisfies Record<string, Limit>;

export type Bucket = keyof typeof LIMITS;

// Takes one request from `key`'s `bucket`; throws 429 when there is none left.
export async function take(c: Ctx, key: string, bucket: Bucket, what: string): Promise<void> {
  const { interval, burst } = LIMITS[bucket];
  const tau = (burst - 1) * interval;
  const db = c.env.DB;
  const [up, cur] = await db.batch<{ tat: number }>([
    db
      .prepare(
        `INSERT INTO rate_limits (key, bucket, tat) VALUES (?1, ?2, ?3 + ?4)
         ON CONFLICT (key, bucket) DO UPDATE SET tat = MAX(tat, ?3) + ?4 WHERE MAX(tat, ?3) - ?3 <= ?5
         RETURNING tat`,
      )
      .bind(key, bucket, c.now, interval, tau),
    db.prepare(`SELECT tat FROM rate_limits WHERE key = ?1 AND bucket = ?2`).bind(key, bucket),
  ]);
  if (up.results.length) return;
  const tat = cur.results[0]?.tat ?? c.now;
  const retry = Math.max(1, Math.ceil(tat - c.now - tau));
  throw new HttpError(429, "rate_limited", `too many ${what} requests; retry in ${retry} s`, { retry_after: retry });
}

// ---------- client IPs (enroll) ----------
//
// An IP is never stored. Its key is "ip:" + HMAC-SHA256(IP_SALT, address) (IPv6: the /64, which one
// household or server usually gets whole), first 18 bytes, base64url. IP_SALT is a Worker secret;
// without it each Worker instance draws a random one when it starts (so a new one with every
// deploy), and instances then don't share enroll limits. Rows are deleted by the cron as soon as
// they carry no state (within minutes), and never later than a day after the last request.

let fallbackSalt: Uint8Array | null = null;
const keyCache = new Map<string, CryptoKey>();

async function hmacKey(salt: string | undefined): Promise<CryptoKey> {
  const id = salt ?? "";
  let k = keyCache.get(id);
  if (!k) {
    const raw = salt ? new TextEncoder().encode(salt) : (fallbackSalt ??= crypto.getRandomValues(new Uint8Array(32)));
    k = await crypto.subtle.importKey("raw", raw, { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
    keyCache.set(id, k);
  }
  return k;
}

// The part of an address that identifies a client: IPv4 as is, IPv6 its /64 (lowercase hex groups).
export function ipPrefix(ip: string): string {
  const a = ip.trim().toLowerCase();
  if (!a.includes(":")) return a;
  const [head, tail] = a.split("::");
  const hs = head ? head.split(":") : [];
  const ts = tail !== undefined && tail ? tail.split(":") : [];
  // An embedded IPv4 tail (::ffff:192.0.2.1) counts as two groups; it is past the /64 anyway.
  const n = (xs: string[]) => xs.reduce((s, x) => s + (x.includes(".") ? 2 : 1), 0);
  const groups = tail === undefined ? hs : [...hs, ...Array(Math.max(0, 8 - n(hs) - n(ts))).fill("0"), ...ts];
  return groups
    .slice(0, 4)
    .map((g) => (/^[0-9a-f]{1,4}$/.test(g) ? g.replace(/^0+(?=.)/, "") : g))
    .join(":") + "::/64";
}

export async function ipKey(ip: string, salt: string | undefined): Promise<string> {
  const mac = new Uint8Array(await crypto.subtle.sign("HMAC", await hmacKey(salt?.trim() || undefined), new TextEncoder().encode(ipPrefix(ip))));
  return "ip:" + base64url(mac.subarray(0, 18));
}

// Deletes rows that carry no state any more: a row whose `tat` has passed is equivalent to no row
// (the next request starts a fresh burst either way). Client-IP rows go as soon as that happens;
// machine rows after a day (cheap to keep, and fewer writes for a machine that keeps asking).
export async function prune(db: D1Database, now: number): Promise<number> {
  const r = await db.prepare(`DELETE FROM rate_limits WHERE tat < ? OR (key LIKE 'ip:%' AND tat < ?)`).bind(now - 86400, now).run();
  return r.meta.changes ?? 0;
}
