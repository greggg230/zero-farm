// The cron trigger (wrangler.jsonc: every minute).

import { bookFor } from "./leasebook";
import { prune } from "./ratelimit";

// Every minute: expire silent leases (each scope's LeaseBook also does this before every grant
// and beat), delete result objects 7 days after the trainer acked them, and drop rate-limit rows
// that carry no state any more.
export async function runCron(env: Env, now: number): Promise<{ swept: number; deleted: number }> {
  let swept = 0;
  const { results } = await env.DB.prepare(`SELECT DISTINCT scope FROM leases WHERE state = 'active' AND expires_at < ?`)
    .bind(now)
    .all<{ scope: string }>();
  for (const { scope } of results) {
    const o = await bookFor(env, scope).sweep(scope, now);
    if ("ok" in o) swept += o.ok;
  }
  const old = await env.DB.prepare(
    `SELECT id, key FROM results WHERE acked_at IS NOT NULL AND acked_at < ? AND deleted_at IS NULL ORDER BY id LIMIT 500`,
  )
    .bind(now - 7 * 86400)
    .all<{ id: number; key: string }>();
  if (old.results.length) {
    await env.BUCKET.delete(old.results.map((r) => r.key));
    await env.DB.prepare(`UPDATE results SET deleted_at = ? WHERE id IN (${old.results.map(() => "?").join(",")})`)
      .bind(now, ...old.results.map((r) => r.id))
      .run();
  }
  await prune(env.DB, now);
  return { swept, deleted: old.results.length };
}
