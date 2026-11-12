// Worker builds Windows refused to start on a machine (PROTOCOL 3, `/v1/crash` kind "blocked",
// rev 5): Smart App Control / WDAC (os error 4551), AppLocker / SRP (1260), Defender (225). Found
// on a real PC on 2026-11-12: agent 0.2.1 leased the same blocked build 521 times in 45 minutes.
// The coordinator stops offering that build to that machine for a day, or until the machine asks
// with a newer agent than the one that reported it. A newer build has another build_id, so it is
// offered at once.

import { cmpSemver, isSemver } from "./util";

export const BLOCK_S = 24 * 3600;

export interface BlockRow {
  machine_id: string;
  build_id: string;
  agent_version: string | null;
  os_error: number | null;
  message: string | null;
  t: number;
  until: number;
}

// True when `agent` (the machine's current agent) is newer than the one that reported the block.
export function blockLifted(b: Pick<BlockRow, "agent_version">, agent: string | null | undefined): boolean {
  return !!agent && !!b.agent_version && isSemver(agent) && isSemver(b.agent_version) && cmpSemver(agent, b.agent_version) > 0;
}

// The builds blocked for this machine now. Blocks a newer agent lifted, and expired ones, are
// deleted.
export async function blockedBuilds(db: D1Database, machineId: string, agent: string, now: number): Promise<Set<string>> {
  const { results } = await db.prepare(`SELECT * FROM blocks WHERE machine_id = ?`).bind(machineId).all<BlockRow>();
  const blocked = new Set<string>();
  const lifted: string[] = [];
  for (const b of results) {
    if (b.until <= now || blockLifted(b, agent)) lifted.push(b.build_id);
    else blocked.add(b.build_id);
  }
  if (lifted.length) {
    await db
      .prepare(`DELETE FROM blocks WHERE machine_id = ? AND build_id IN (${lifted.map(() => "?").join(",")})`)
      .bind(machineId, ...lifted)
      .run();
  }
  return blocked;
}

export function recordBlock(
  db: D1Database,
  b: { machine_id: string; build_id: string; agent_version: string | null; os_error: number | null; message: string | null; now: number },
): D1PreparedStatement {
  return db
    .prepare(
      `INSERT INTO blocks (machine_id, build_id, agent_version, os_error, message, t, until) VALUES (?, ?, ?, ?, ?, ?, ?)
       ON CONFLICT (machine_id, build_id) DO UPDATE SET agent_version = excluded.agent_version, os_error = excluded.os_error,
         message = excluded.message, t = excluded.t, until = excluded.until`,
    )
    .bind(b.machine_id, b.build_id, b.agent_version, b.os_error, b.message, b.now, b.now + BLOCK_S);
}
