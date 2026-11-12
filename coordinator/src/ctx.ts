// Request context and authentication.

import { accessEnabled, verifyAccess } from "./access";
import type { AnyEnv } from "./config";
import { HttpError, isSemver, safeEqual, sha256Hex } from "./util";

export interface Ctx {
  req: Request;
  env: AnyEnv;
  url: URL;
  now: number;
  exec: ExecutionContext;
  admin?: AdminAuth; // set by authAdmin
}

// How the admin authenticated: the bearer token (CLI, trainer, release tool, dashboard fallback)
// or a Cloudflare Access login: its JWT in the Cf-Access-Jwt-Assertion header (a path behind
// Access) or in the CF_Authorization cookie (the browser's Access session from /admin).
export interface AdminAuth {
  via: "token" | "access";
  cookie?: boolean;
  email?: string | null;
  common_name?: string | null;
}

// The header the admin page sends on every API call. A cross-site form can't set it, and a
// cross-site script can't send it without a CORS preflight, which the coordinator never grants.
export const ADMIN_PAGE_HEADER = "x-farm-admin";
const SAFE_METHODS = ["GET", "HEAD"];

// CSRF checks for a request authenticated only by the Access cookie (which the browser attaches
// to any request to this host): it must come from this site. Reads: a `Sec-Fetch-Site` other than
// same-origin (or none: the owner typed the URL) is refused. Changes: `x-farm-admin: 1`, and
// `Origin` (if sent) this origin, `Sec-Fetch-Site` (if sent) same-origin, and at least one of the
// two sent.
export function cookieRequestProblem(c: Ctx): string | null {
  const site = c.req.headers.get("sec-fetch-site");
  const origin = c.req.headers.get("origin");
  if (SAFE_METHODS.includes(c.req.method)) {
    return site !== null && site !== "same-origin" && site !== "none" ? `a ${site} request` : null;
  }
  if (c.req.headers.get(ADMIN_PAGE_HEADER) !== "1") return `no ${ADMIN_PAGE_HEADER}: 1 header`;
  if (site !== null && site !== "same-origin") return `a ${site} request`;
  if (origin !== null && origin !== c.url.origin) return `origin ${origin}`;
  if (site === null && origin === null) return "no Origin or Sec-Fetch-Site header";
  return null;
}

// The values of a cookie (a name can appear more than once).
function cookies(req: Request, name: string): string[] {
  const out: string[] = [];
  for (const part of (req.headers.get("cookie") ?? "").split(";")) {
    const i = part.indexOf("=");
    if (i > 0 && part.slice(0, i).trim() === name) out.push(part.slice(i + 1).trim());
  }
  return out.slice(0, 4);
}

// Unix seconds. Tests move the clock with an `x-test-now` header, honoured only when the
// TEST_CLOCK var is "1" (set by vitest.config.ts, never in wrangler.jsonc).
export function nowOf(req: Request, env: object): number {
  if ((env as { TEST_CLOCK?: string }).TEST_CLOCK === "1") {
    const h = req.headers.get("x-test-now");
    if (h && /^\d+$/.test(h)) return Number(h);
  }
  return Math.floor(Date.now() / 1000);
}

export interface MachineRow {
  machine_id: string;
  token_hash: string;
  name: string;
  channel: string;
  invite: string | null;
  created_at: number;
  last_seen: number | null;
  agent_version: string | null;
  profile: string | null;
  profile_digest: string | null;
  class: string | null;
  caps: string | null;
  paused: number;
  revoked_at: number | null;
}

function bearer(req: Request): string | null {
  const m = /^Bearer\s+(\S+)$/i.exec(req.headers.get("authorization") ?? "");
  return m ? m[1] : null;
}

// The machine behind a machine token. Unknown -> 401 unauthorized; revoked -> 403 revoked.
export async function authMachine(c: Ctx): Promise<MachineRow> {
  const token = bearer(c.req);
  if (!token) throw new HttpError(401, "unauthorized", "missing bearer token");
  const row = await c.env.DB.prepare(`SELECT * FROM machines WHERE token_hash = ?`)
    .bind(await sha256Hex(token))
    .first<MachineRow>();
  if (!row) throw new HttpError(401, "unauthorized", "unknown machine token");
  if (row.revoked_at !== null) throw new HttpError(403, "revoked", "this machine was revoked");
  const agent = c.req.headers.get("x-farm-agent");
  const version = agent && isSemver(agent) ? agent : row.agent_version;
  if (row.last_seen === null || c.now - row.last_seen >= 30 || version !== row.agent_version) {
    await c.env.DB.prepare(`UPDATE machines SET last_seen = ?, agent_version = ? WHERE machine_id = ?`)
      .bind(c.now, version, row.machine_id)
      .run();
    row.last_seen = c.now;
    row.agent_version = version;
  }
  return row;
}

// The admin: the bearer admin token (Worker secret ADMIN_TOKEN; unset -> no token is admin), or,
// when Cloudflare Access is configured (access.ts), a valid Access JWT: in Cf-Access-Jwt-Assertion,
// or in the CF_Authorization cookie, the session Access gives the browser for /admin (Access
// protects only that page; /v1/admin stays outside it). The cookie also passes the CSRF checks
// above. The same JWT checks apply either way.
export async function authAdmin(c: Ctx): Promise<AdminAuth> {
  const token = bearer(c.req);
  const want = c.env.ADMIN_TOKEN;
  if (want && token && safeEqual(token, want)) return (c.admin = { via: "token" });
  if (accessEnabled(c.env)) {
    const jwt = c.req.headers.get("cf-access-jwt-assertion");
    if (jwt) {
      const v = await verifyAccess(c.env, jwt, c.now);
      if ("id" in v) return (c.admin = { via: "access", ...v.id });
      console.warn("admin: Cloudflare Access JWT refused:", v.why);
    }
    for (const value of cookies(c.req, "CF_Authorization")) {
      const v = await verifyAccess(c.env, value, c.now);
      if (!("id" in v)) {
        console.warn("admin: Cloudflare Access cookie refused:", v.why);
        continue;
      }
      const csrf = cookieRequestProblem(c);
      if (csrf) {
        throw new HttpError(403, "unauthorized", `an admin request signed in by the Access cookie must come from the admin page (${csrf})`);
      }
      return (c.admin = { via: "access", cookie: true, ...v.id });
    }
  }
  throw new HttpError(401, "unauthorized", accessEnabled(c.env) ? "admin token or Cloudflare Access login required" : "admin token required");
}
