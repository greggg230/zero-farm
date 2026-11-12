// Cloudflare Access logins as admin (security review H1). When the secrets ACCESS_TEAM_DOMAIN and
// ACCESS_AUD are set, a request to /v1/admin/* that carries a valid `Cf-Access-Jwt-Assertion`
// (Access adds it after a login, or after a service token's headers) counts as the admin. The
// bearer admin token keeps working (the CLI, the trainer, farm-release).
//
// Checks (Cloudflare's "validate JWTs" guidance): RS256 only, signed by a key from the team's
// JWKS (https://<team>.cloudflareaccess.com/cdn-cgi/access/certs, cached), `iss` = the team URL,
// `aud` contains one of ACCESS_AUD, `exp` in the future, `nbf`/`iat` not in the future. With
// ACCESS_EMAILS set, the identity (`email`, or a service token's `common_name`) must be listed.

import { list, type AnyEnv } from "./config";

export interface AccessIdentity {
  email: string | null; // a person's login
  common_name: string | null; // a service token's client id
}

const LEEWAY_S = 60;
const JWKS_TTL_S = 3600; // keys are refetched after an hour...
const JWKS_RETRY_S = 60; // ...or at once for an unknown kid, at most once a minute

interface KeySet {
  keys: Map<string, CryptoKey>;
  fetched: number;
}
const cache = new Map<string, KeySet>();

export function accessEnabled(env: AnyEnv): boolean {
  return !!(env.ACCESS_TEAM_DOMAIN?.trim() && list(env.ACCESS_AUD).length);
}

// "myteam" or "myteam.cloudflareaccess.com" (with or without https://) -> the team's origin.
export function teamOrigin(domain: string): string {
  const d = domain.trim().replace(/^https?:\/\//, "").replace(/\/+$/, "");
  return `https://${d.includes(".") ? d : `${d}.cloudflareaccess.com`}`;
}

function b64urlBytes(s: string): Uint8Array {
  const b = atob(s.replace(/-/g, "+").replace(/_/g, "/") + "=".repeat((4 - (s.length % 4)) % 4));
  const out = new Uint8Array(b.length);
  for (let i = 0; i < b.length; i++) out[i] = b.charCodeAt(i);
  return out;
}

const b64urlJson = (s: string): any => JSON.parse(new TextDecoder().decode(b64urlBytes(s)));

async function fetchKeys(certsUrl: string, now: number): Promise<KeySet> {
  const res = await fetch(certsUrl, { headers: { accept: "application/json" } });
  if (!res.ok) throw new Error(`JWKS ${certsUrl}: HTTP ${res.status}`);
  const body = (await res.json()) as { keys?: JsonWebKey[] };
  const keys = new Map<string, CryptoKey>();
  for (const jwk of body.keys ?? []) {
    const kid = (jwk as { kid?: string }).kid;
    if (jwk.kty !== "RSA" || !kid) continue;
    try {
      const k = await crypto.subtle.importKey("jwk", { kty: "RSA", n: jwk.n, e: jwk.e, alg: "RS256", ext: true }, { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" }, false, ["verify"]);
      keys.set(kid, k);
    } catch {
      // skip a key we can't use
    }
  }
  const set = { keys, fetched: now };
  cache.set(certsUrl, set);
  return set;
}

async function keyFor(certsUrl: string, kid: string, now: number): Promise<CryptoKey | null> {
  let set = cache.get(certsUrl);
  if (!set || now - set.fetched >= JWKS_TTL_S || now < set.fetched) set = await fetchKeys(certsUrl, now);
  else if (!set.keys.has(kid) && now - set.fetched >= JWKS_RETRY_S) set = await fetchKeys(certsUrl, now);
  return set.keys.get(kid) ?? null;
}

// The identity behind a valid Access JWT, else null (and why, for the log). `now` in Unix seconds.
export async function verifyAccess(env: AnyEnv, jwt: string, now: number): Promise<{ id: AccessIdentity } | { why: string }> {
  if (!accessEnabled(env)) return { why: "Access is not configured" };
  const parts = jwt.split(".");
  if (parts.length !== 3 || parts.some((p) => !/^[A-Za-z0-9_-]*$/.test(p))) return { why: "not a JWT" };
  let header: any;
  let claims: any;
  try {
    header = b64urlJson(parts[0]);
    claims = b64urlJson(parts[1]);
  } catch {
    return { why: "JWT is not base64url JSON" };
  }
  if (!header || header.alg !== "RS256" || typeof header.kid !== "string") return { why: "JWT alg must be RS256 with a kid" };
  const origin = teamOrigin(env.ACCESS_TEAM_DOMAIN!);
  let key: CryptoKey | null;
  try {
    key = await keyFor(`${origin}/cdn-cgi/access/certs`, header.kid, now);
  } catch (e) {
    return { why: `JWKS: ${e}` };
  }
  if (!key) return { why: `unknown key id ${header.kid}` };
  const signed = new TextEncoder().encode(`${parts[0]}.${parts[1]}`);
  let sig: Uint8Array;
  try {
    sig = b64urlBytes(parts[2]);
  } catch {
    return { why: "bad signature encoding" };
  }
  if (!(await crypto.subtle.verify("RSASSA-PKCS1-v1_5", key, sig, signed))) return { why: "signature does not verify" };
  if (!claims || typeof claims !== "object") return { why: "no claims" };
  if (claims.iss !== origin) return { why: `iss ${claims.iss} is not ${origin}` };
  const auds = Array.isArray(claims.aud) ? claims.aud : [claims.aud];
  const want = list(env.ACCESS_AUD);
  if (!auds.some((a: unknown) => typeof a === "string" && want.includes(a))) return { why: "aud does not match ACCESS_AUD" };
  if (typeof claims.exp !== "number" || claims.exp <= now - LEEWAY_S) return { why: "expired" };
  if (typeof claims.nbf === "number" && claims.nbf > now + LEEWAY_S) return { why: "not valid yet" };
  if (typeof claims.iat === "number" && claims.iat > now + LEEWAY_S) return { why: "issued in the future" };
  const email = typeof claims.email === "string" && claims.email ? claims.email : null;
  const cn = typeof claims.common_name === "string" && claims.common_name ? claims.common_name : null;
  if (!email && !cn) return { why: "no email or common_name" };
  const allow = list(env.ACCESS_EMAILS).map((x) => x.toLowerCase());
  if (allow.length && !allow.includes((email ?? cn)!.toLowerCase())) return { why: `${email ?? cn} is not in ACCESS_EMAILS` };
  return { id: { email, common_name: cn } };
}

// Tests only: forget cached keys.
export function clearAccessCache(): void {
  cache.clear();
}
