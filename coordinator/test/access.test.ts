// Cloudflare Access logins as admin (security review H1): with ACCESS_TEAM_DOMAIN and ACCESS_AUD
// set, a valid Cf-Access-Jwt-Assertion (RS256, the team's JWKS, aud, exp, iss, optional
// ACCESS_EMAILS) counts as the admin on /v1/admin/*; the bearer admin token keeps working. The
// JWKS comes from a locally generated RSA key, served through a stubbed fetch.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { clearAccessCache, teamOrigin } from "../src/access";
import { ADMIN, advance, call, clock, enroll, err, ok } from "./helpers";

const TEAM = "farmteam";
const ISS = "https://farmteam.cloudflareaccess.com";
const CERTS = `${ISS}/cdn-cgi/access/certs`;
const AUD = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const ENV = { ACCESS_TEAM_DOMAIN: TEAM, ACCESS_AUD: AUD };

const enc = new TextEncoder();
const b64url = (b: Uint8Array | string) =>
  btoa(String.fromCharCode(...(typeof b === "string" ? enc.encode(b) : b)))
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");

async function rsaKey() {
  return (await crypto.subtle.generateKey(
    { name: "RSASSA-PKCS1-v1_5", modulusLength: 2048, publicExponent: new Uint8Array([1, 0, 1]), hash: "SHA-256" },
    true,
    ["sign", "verify"],
  )) as CryptoKeyPair;
}

let key: CryptoKeyPair;
let jwks: { keys: object[] };
let fetches: string[];

async function jwt(claims: Record<string, unknown>, o: { kid?: string; alg?: string; signWith?: CryptoKey } = {}) {
  const head = b64url(JSON.stringify({ alg: o.alg ?? "RS256", kid: o.kid ?? "k1", typ: "JWT" }));
  const body = b64url(JSON.stringify(claims));
  const sig = new Uint8Array(await crypto.subtle.sign("RSASSA-PKCS1-v1_5", o.signWith ?? key.privateKey, enc.encode(`${head}.${body}`)));
  return `${head}.${body}.${b64url(sig)}`;
}

const claims = (o: Record<string, unknown> = {}) => ({
  aud: [AUD],
  email: "owner@example.com",
  exp: clock.now + 3600,
  iat: clock.now,
  nbf: clock.now,
  iss: ISS,
  type: "app",
  sub: "u1",
  ...o,
});

const withJwt = async (j: string, env: Record<string, string> = ENV, extra: Record<string, string> = {}) =>
  call("GET", "/v1/admin/stats", { headers: { "cf-access-jwt-assertion": j, ...extra }, env, agent: null });

beforeEach(async () => {
  clearAccessCache();
  key = await rsaKey();
  const pub = (await crypto.subtle.exportKey("jwk", key.publicKey)) as JsonWebKey;
  jwks = { keys: [{ kty: "RSA", n: pub.n, e: pub.e, kid: "k1", alg: "RS256", use: "sig" }] };
  fetches = [];
  vi.spyOn(globalThis, "fetch").mockImplementation(async (input: RequestInfo | URL) => {
    const url = input instanceof Request ? input.url : String(input);
    fetches.push(url);
    if (url === CERTS) return new Response(JSON.stringify(jwks), { headers: { "content-type": "application/json" } });
    return new Response("not found", { status: 404 });
  });
});
afterEach(() => vi.restoreAllMocks());

describe("Cloudflare Access", () => {
  it("a valid Access JWT is the admin; the JWKS is fetched once and cached", async () => {
    const j = await jwt(claims());
    const s = ok(await withJwt(j));
    expect(s.auth).toEqual({ via: "access", email: "owner@example.com", common_name: null });
    expect(s.limits.access).toBe(true);
    ok(await withJwt(j));
    ok(await call("POST", "/v1/admin/invites", { body: { count: 1 }, headers: { "cf-access-jwt-assertion": j }, env: ENV }));
    expect(fetches).toEqual([CERTS]); // cached
    // A service token's JWT (no email, a common_name) is accepted too.
    expect(ok(await withJwt(await jwt(claims({ email: undefined, common_name: "abc.access" })))).auth).toMatchObject({ via: "access", common_name: "abc.access" });
    // The team domain may be given in full.
    expect(teamOrigin("farmteam.cloudflareaccess.com")).toBe(ISS);
    expect(teamOrigin("https://farmteam.cloudflareaccess.com/")).toBe(ISS);
  });

  it("the bearer admin token keeps working with Access configured; neither -> 401", async () => {
    expect(ok(await call("GET", "/v1/admin/stats", { token: ADMIN(), env: ENV })).auth).toEqual({ via: "token" });
    expect(err(await call("GET", "/v1/admin/stats", { env: ENV }), 401, "unauthorized").message).toMatch(/Cloudflare Access/);
    // Without Access configured a JWT means nothing.
    err(await withJwt(await jwt(claims()), {}), 401, "unauthorized");
    expect(fetches).toEqual([]);
  });

  it("refuses wrong aud, iss, expiry, algorithm, signer and unknown keys", async () => {
    const no = async (j: string, env = ENV) => err(await withJwt(j, env), 401, "unauthorized");
    await no(await jwt(claims({ aud: ["someone-else"] })));
    await no(await jwt(claims({ aud: AUD.slice(1) })));
    await no(await jwt(claims({ iss: "https://evil.cloudflareaccess.com" })));
    await no(await jwt(claims({ exp: clock.now - 120 })));
    await no(await jwt(claims({ nbf: clock.now + 600 })));
    await no(await jwt(claims({ email: undefined })));
    await no(await jwt(claims(), { alg: "HS256" }));
    await no((await jwt(claims())).split(".").slice(0, 2).join(".") + ".");
    await no("not.a.jwt");
    // Signed by another key under the same kid.
    const other = await rsaKey();
    await no(await jwt(claims(), { signWith: other.privateKey }));
    // A tampered payload.
    const [h, , s] = (await jwt(claims())).split(".");
    await no(`${h}.${b64url(JSON.stringify(claims({ email: "attacker@example.com" })))}.${s}`);
    // An unknown kid refetches the JWKS (key rotation), at most once a minute.
    fetches.length = 0;
    await no(await jwt(claims(), { kid: "k2" }));
    expect(fetches).toEqual([]); // fetched less than a minute ago
    advance(61);
    await no(await jwt(claims(), { kid: "k2" }));
    expect(fetches).toEqual([CERTS]);
    await no(await jwt(claims(), { kid: "k2" }));
    expect(fetches).toEqual([CERTS]);
    // ACCESS_AUD may list several applications.
    ok(await withJwt(await jwt(claims({ aud: "second-app" })), { ...ENV, ACCESS_AUD: `${AUD}, second-app` }));
  });

  it("ACCESS_EMAILS limits who may sign in", async () => {
    const env = { ...ENV, ACCESS_EMAILS: "Owner@Example.com, abc.access" };
    ok(await withJwt(await jwt(claims()), env)); // case-insensitive
    ok(await withJwt(await jwt(claims({ email: undefined, common_name: "abc.access" })), env));
    err(await withJwt(await jwt(claims({ email: "friend@example.com" })), env), 401, "unauthorized");
  });

  it("an Access JWT is no machine token, and the admin page works without a token box", async () => {
    const m = await enroll("pc");
    const j = await jwt(claims());
    err(await call("GET", "/v1/agent/latest", { headers: { "cf-access-jwt-assertion": j }, env: ENV }), 401, "unauthorized");
    expect((await call("GET", "/v1/agent/latest", { token: m.token, env: ENV })).status).toBe(404); // nothing released
    const page = (await call("GET", "/admin")).body as string;
    expect(page).toContain("signed in through Cloudflare Access");
    expect(page).toContain('id="tokenbox" hidden');
    expect(page).toContain("sessionStorage");
    expect(page).not.toContain("localStorage.setItem");
  });
});
