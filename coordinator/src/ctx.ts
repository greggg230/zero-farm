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
// or a Cloudflare Access login / service token.
export interface AdminAuth {
  via: "token" | "access";
  email?: string | null;
  common_name?: string | null;
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
// when Cloudflare Access is configured (access.ts), a valid Cf-Access-Jwt-Assertion.
export async function authAdmin(c: Ctx): Promise<AdminAuth> {
  const token = bearer(c.req);
  const want = c.env.ADMIN_TOKEN;
  if (want && token && safeEqual(token, want)) return (c.admin = { via: "token" });
  const jwt = c.req.headers.get("cf-access-jwt-assertion");
  if (jwt && accessEnabled(c.env)) {
    const v = await verifyAccess(c.env, jwt, c.now);
    if ("id" in v) return (c.admin = { via: "access", ...v.id });
    console.warn("admin: Cloudflare Access JWT refused:", v.why);
  }
  throw new HttpError(401, "unauthorized", accessEnabled(c.env) ? "admin token or Cloudflare Access login required" : "admin token required");
}
