// Rev 16 (PROTOCOL 3, POST /v1/presence): an agent that may not work right now (its owner is using
// the PC, a fullscreen app, battery, outside its hours, paused from the tray, low memory) says so
// about every 5 minutes. Before rev 16 such an agent was silent, so the coordinator could not tell
// a PC in use from one switched off.

import { authMachine, type Ctx } from "./ctx";
import { take } from "./ratelimit";
import { json, readJson, str } from "./util";

// The reasons agents send today (agent policy.rs Block). Any slug is stored, so a newer agent's
// reason needs no coordinator change.
export const PRESENCE_REASONS = ["user_active", "fullscreen", "outside_hours", "on_battery", "battery_saver", "low_memory", "paused"] as const;
const REASON_RE = /^[a-z][a-z0-9_]{0,31}$/;

// A presence older than this, or older than the machine's last other contact, says nothing.
export const PRESENCE_FRESH_S = 600;

export async function presence(c: Ctx): Promise<Response> {
  const m = await authMachine(c);
  await take(c, m.machine_id, "presence", "presence");
  const b = await readJson(c.req, 4096);
  const reason = str(b.reason, "reason", { re: REASON_RE });
  await c.env.DB.prepare(`UPDATE machines SET presence = ?, presence_t = ?, last_seen = ? WHERE machine_id = ?`)
    .bind(reason, c.now, c.now, m.machine_id)
    .run();
  return json({ ok: true });
}

// The machine's current presence reason, or null: fresh, and not older than its last other contact
// (authMachine moves last_seen at most every 30 s, so a contact within 30 s of the presence can't
// be told apart from it; a lease request after the presence moves last_seen past it).
export function presenceOf(m: { presence?: string | null; presence_t?: number | null; last_seen: number | null }, now: number): string | null {
  if (!m.presence || m.presence_t == null) return null;
  if (now - m.presence_t >= PRESENCE_FRESH_S) return null;
  if (m.last_seen !== null && m.last_seen - m.presence_t > 30) return null;
  return m.presence;
}
