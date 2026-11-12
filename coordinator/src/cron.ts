// The cron trigger (wrangler.jsonc: every minute).

import { config } from "./config";
import { pruneExternal } from "./external";
import { bookFor } from "./leasebook";
import { prune } from "./ratelimit";

const BATCH = 500; // rows (and R2 objects) per sweep per minute

export interface CronResult {
  swept: number; // leases expired
  deleted: number; // acked result objects deleted
  crashes: number; // crash rows past their retention
  diag: number; // diagnostics uploads and diag-lease results past their retention (objects + rows)
}

// Every minute: expire silent leases (each scope's LeaseBook also does this before every grant
// and beat), delete result objects 7 days after the trainer acked them, delete crash rows after
// CRASH_RETENTION_DAYS and diagnostics (uploads and diag-lease results: R2 objects and rows) after
// DIAG_RETENTION_DAYS, drop rate-limit rows that carry no state any more (client-IP rows within
// minutes) and external sources unseen for 7 days.
export async function runCron(env: Env, now: number): Promise<CronResult> {
  const cfg = config(env);
  const db = env.DB;
  let swept = 0;
  const { results } = await db.prepare(`SELECT DISTINCT scope FROM leases WHERE state = 'active' AND expires_at < ?`)
    .bind(now)
    .all<{ scope: string }>();
  for (const { scope } of results) {
    const o = await bookFor(env, scope).sweep(scope, now);
    if ("ok" in o) swept += o.ok;
  }
  const old = await db.prepare(
    `SELECT id, key FROM results WHERE acked_at IS NOT NULL AND acked_at < ? AND deleted_at IS NULL AND scope != 'diag' ORDER BY id LIMIT ?`,
  )
    .bind(now - 7 * 86400, BATCH)
    .all<{ id: number; key: string }>();
  if (old.results.length) {
    await env.BUCKET.delete(old.results.map((r) => r.key));
    await db.prepare(`UPDATE results SET deleted_at = ? WHERE id IN (${old.results.map(() => "?").join(",")})`)
      .bind(now, ...old.results.map((r) => r.id))
      .run();
  }

  // Retention (security review M3).
  const crashes =
    (
      await db
        .prepare(`DELETE FROM crashes WHERE id IN (SELECT id FROM crashes WHERE t < ? ORDER BY id LIMIT ?)`)
        .bind(now - cfg.crashRetentionDays * 86400, BATCH)
        .run()
    ).meta.changes ?? 0;
  const diagCut = now - cfg.diagRetentionDays * 86400;
  const [ups, res] = await db.batch<{ id: number; key: string }>([
    db.prepare(`SELECT id, key FROM diag_uploads WHERE t < ? ORDER BY id LIMIT ?`).bind(diagCut, BATCH),
    db.prepare(`SELECT id, key FROM results WHERE scope = 'diag' AND t < ? ORDER BY id LIMIT ?`).bind(diagCut, BATCH),
  ]);
  const keys = [...ups.results, ...res.results].map((r) => r.key);
  if (keys.length) {
    await env.BUCKET.delete(keys);
    const stmts = [];
    if (ups.results.length) {
      stmts.push(db.prepare(`DELETE FROM diag_uploads WHERE id IN (${ups.results.map(() => "?").join(",")})`).bind(...ups.results.map((r) => r.id)));
    }
    if (res.results.length) {
      stmts.push(db.prepare(`DELETE FROM results WHERE id IN (${res.results.map(() => "?").join(",")})`).bind(...res.results.map((r) => r.id)));
    }
    await db.batch(stmts);
  }
  // Diagnostics requests answered (or asked) longer ago than that, unless a diag lease still runs.
  await db
    .prepare(
      `DELETE FROM diag_requests WHERE id IN (SELECT d.id FROM diag_requests d WHERE COALESCE(d.done_at, d.created_at) < ?
         AND NOT EXISTS (SELECT 1 FROM leases l WHERE l.lease_id = d.lease_id AND l.state = 'active') LIMIT ?)`,
    )
    .bind(diagCut, BATCH)
    .run();

  await prune(db, now);
  await pruneExternal(db, now);
  return { swept, deleted: old.results.length, crashes, diag: keys.length };
}
