// LeaseBook: one Durable Object per scope ("run:<run>", "campaign:<id>", "diag"). It holds no
// state of its own; it is the one place a scope's leases, id pool, claims and quota counters
// change, one call at a time. D1 is the store.
//
// A Durable Object only blocks other requests while it waits on its *own* storage. While it
// awaits D1 or R2, another request can start running. So every method goes through `serial`, a
// promise chain that runs one call to the end before the next begins.

import { DurableObject } from "cloudflare:workers";
import * as book from "./book";
import type { DefinitionRow } from "./definitions";
import { HttpError, type Outcome } from "./util";

// What callers see. Spelled out (rather than derived from the class through DurableObjectStub)
// because the RPC type mapping can't handle the recursive Json type; the class implements it,
// so the two can't drift apart.
export interface LeaseBookApi {
  grant(req: book.GrantReq): Promise<Outcome<book.Lease | null>>;
  beat(req: book.BeatReq): Promise<Outcome<book.BeatAnswer>>;
  putResult(req: book.ResultReq): Promise<Outcome<book.ResultCommit>>;
  done(req: book.DoneReq): Promise<Outcome<book.DoneAnswer>>;
  sweep(scope: string, now: number): Promise<Outcome<number>>;
  cancelLease(leaseId: string, reason: string, now: number): Promise<Outcome<boolean>>;
  putRun(cfg: book.RunCfg, now: number): Promise<Outcome<{ ok: true; created: boolean }>>;
  putCampaign(cfg: book.CampaignCfg, now: number): Promise<Outcome<{ ok: true; created: boolean; cursor: number }>>;
  publishCheckpoint(req: book.CheckpointReq): Promise<Outcome<{ ok: true; unchanged: boolean }>>;
  accept(run: string, gen: number, games: number, now: number): Promise<Outcome<book.SelfplayState | null>>;
  ack(scope: string, cursor: number, now: number): Promise<Outcome<{ ok: true; acked: number }>>;
  selfplayState(run: string): Promise<Outcome<book.SelfplayState | null>>;
  putDefinition(row: DefinitionRow): Promise<Outcome<book.DefinitionAnswer>>;
}

export class LeaseBook extends DurableObject<Env> implements LeaseBookApi {
  private tail: Promise<unknown> = Promise.resolve();

  private serial<T>(fn: () => Promise<T>): Promise<Outcome<T>> {
    const run = async (): Promise<Outcome<T>> => {
      try {
        return { ok: await fn() };
      } catch (e) {
        if (e instanceof HttpError) return { err: e.toJSON() };
        throw e;
      }
    };
    const p = this.tail.then(run, run);
    this.tail = p.catch(() => undefined);
    return p;
  }

  grant(req: book.GrantReq) {
    return this.serial(() => book.grant(this.env, req));
  }
  beat(req: book.BeatReq) {
    return this.serial(() => book.beat(this.env, req));
  }
  putResult(req: book.ResultReq) {
    return this.serial(() => book.putResult(this.env, req));
  }
  done(req: book.DoneReq) {
    return this.serial(() => book.done(this.env, req));
  }
  sweep(scope: string, now: number) {
    return this.serial(() => book.sweep(this.env, scope, now));
  }
  cancelLease(leaseId: string, reason: string, now: number) {
    return this.serial(() => book.cancelLease(this.env, leaseId, reason, now));
  }
  putRun(cfg: book.RunCfg, now: number) {
    return this.serial(() => book.putRun(this.env, cfg, now));
  }
  putCampaign(cfg: book.CampaignCfg, now: number) {
    return this.serial(() => book.putCampaign(this.env, cfg, now));
  }
  publishCheckpoint(req: book.CheckpointReq) {
    return this.serial(() => book.publishCheckpoint(this.env, req));
  }
  accept(run: string, gen: number, games: number, now: number) {
    return this.serial(() => book.accept(this.env, run, gen, games, now));
  }
  ack(scope: string, cursor: number, now: number) {
    return this.serial(() => book.ack(this.env, scope, cursor, now));
  }
  // Rev 6: definitions change what a scope's grants may issue, so they go through its LeaseBook.
  putDefinition(row: DefinitionRow) {
    return this.serial(() => book.putDefinition(this.env, row));
  }
  // Read-only, but serialised so it never sees half of a change.
  selfplayState(run: string) {
    return this.serial(async () => {
      const r = await book.getRun(this.env.DB, run);
      const sp = r?.selfplay ? (JSON.parse(r.selfplay) as book.SelfplayCfg | null) : null;
      return r && sp ? book.selfplayState(this.env.DB, r, sp) : null;
    });
  }
}

export function bookFor(env: Env, scope: string): LeaseBookApi {
  const ns = env.LEASEBOOK as unknown as DurableObjectNamespace;
  return ns.get(ns.idFromName(scope)) as unknown as LeaseBookApi;
}
