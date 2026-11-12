// A simulated fleet: N fake machines lease self-play, paired eval (two networks per lease, PROTOCOL
// 5.2) and fuzz work from the real Worker (local workerd), and randomly vanish (some for good),
// yield past the slack, come back late with results, double-upload and upload conflicting bodies.
// Eval games finish in random order and are flushed at random, so pairs are often split across
// chunks or left half-played by a dropout. A fake trainer ingests results by cursor, fills each
// generation's quota, publishes the next checkpoint, reads eval batches the way PROTOCOL 5.2 says
// (rows of `ids` plus rows named by `also`), starts a second batch when the first is complete,
// and acks.
//
// Rev 6 (signed definitions, PROTOCOL 7): four machines run an agent older than DEFINITION_AGENT.
// The fuzz campaign is signed from the start, the run only once gen 2 is published (the trainer
// then signs a new version for every gen and eval batch). Legacy agents get legacy self-play
// leases until then and nothing after; every other lease carries a definition, which the fake
// agent checks the way a real one does (signature, run/campaign, build, runtime, checkpoints,
// per-lease fields only). Machines run with the real lease cap (2 active leases).
//
// Checked: no game id or seed is lost or counted twice (claims + pool tile every id handed out),
// every generation is filled exactly by accepted games, the coordinator never has more than
// quota x (1 + slack) games accepted + in flight, every eval batch ends with every pair credited
// exactly once, both games from one lease, and the run finishes although machines die.

import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { runCron } from "../src/cron";
import { normalize, total, type Range } from "../src/ranges";
import {
  admin,
  advance,
  ask,
  beat,
  checkDefinitionLease,
  clock,
  define,
  enroll,
  LEGACY,
  expectTiled,
  finish,
  makeBuild,
  makeCampaign,
  ok,
  publishCkpt,
  runBody,
  upload,
  type Machine,
} from "./helpers";

function mulberry32(seed: number) {
  return () => {
    seed = (seed + 0x6d2b79f5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const toRanges = (ids: number[]): Range[] => normalize(ids.map((i) => [i, i + 1] as Range));
const idsIn = (rs: Range[]) => rs.flatMap(([a, b]) => Array.from({ length: b - a }, (_, i) => a + i));

const STEP_S = 30;
const SLACK_S = 600;
const RUN = "sim";
const CAMPAIGN = "simfz";
const QUOTA = 80;
const SLACK = 0.1;
const MAX_STALE = 2;
const GENS = 6;
const N = 10;
const SEED_BASE = 1_000_000;
// Eval batches: e1 (gen 2 v gen 1) once gen 2 exists, e2 (current gen v gen 1) once e1 is complete.
const E1_GAMES = 120;
const E2_GAMES = 60;
const N_LEGACY = 4; // machines 1..4 run an agent older than DEFINITION_AGENT
const CAP = { MAX_ACTIVE_LEASES: "2" }; // the real default (tests otherwise allow more)

interface Chunk {
  seq: number;
  content: string;
  ids: Range[] | null;
  gen?: number;
}

interface Work {
  l: any;
  def: any; // the lease's definition (null: a legacy lease)
  kind: "selfplay" | "fuzz" | "eval";
  todo: number[]; // ids not played yet (self-play: in random order; fuzz: ascending)
  played: { id: number; gen: number }[]; // played, not in a chunk yet
  fuzzDone: number; // fuzz: seeds done, from first
  chunks: Chunk[]; // made, not acknowledged yet
  uploaded: number[]; // seqs the coordinator took
  seq: number;
  gen: number;
  pausedFor: number;
  pausedSteps: number;
  lastBeat: number;
}

interface Sm {
  m: Machine;
  i: number;
  agent: string;
  gpu: boolean;
  rate: number; // self-play games per step
  fuzzRate: number; // fuzz seeds per step
  state: "idle" | "working" | "offline" | "dead";
  back: number;
  work: Work | null;
}

const stats = {
  leases: 0,
  vanished: 0,
  crashedLosingGames: 0,
  died: 0,
  lateTaken: 0,
  lateDup: 0,
  late410: 0,
  stale410: 0,
  cancelled: 0,
  agentGiveBacks: 0,
  coordGiveBacks: 0,
  expiredOnReturn: 0,
  idempotent: 0,
  conflicts: 0,
  checkpointSwitches: 0,
  fuzzFailures: 0,
  evalLeases: 0,
  evalHeldUploads: 0, // uploads that left a game waiting for its pair-mate
  evalAlso: 0, // chunks that completed a pair held by an earlier chunk
  evalVanished: 0, // machines that vanished holding an eval lease
  evalPairsReplayed: 0, // pairs leased again after a lease gave them back
  alignedSkip: 0, // eval batches that had to skip an odd id
  defLeases: 0, // leases with a definition (checked like an agent does)
  legacyLeases: 0, // leases without one (legacy agents, before the run had a definition)
  legacyIdleAfterCutover: 0, // legacy agents' lease requests answered without work after it
  definitions: 0, // definition versions the trainer signed
};

describe("simulated fleet", () => {
  it("survives vanishing, slow, late and double-uploading machines without losing or double-counting work", async () => {
    const rnd = mulberry32(20261111);
    const pick = (n: number) => Math.floor(rnd() * n);

    // --- setup: one self-play run, one fuzz campaign (the backfill), N machines ---
    const build = await makeBuild({ tag: "sim" });
    ok(await admin("PUT", `/v1/admin/run/${RUN}`, runBody(build, { quota: QUOTA, slack: SLACK, max_stale: MAX_STALE, games_per_lease: "auto" })));
    await publishCkpt(RUN, 1);
    await makeCampaign(CAMPAIGN, { build, seed_base: SEED_BASE, chunk: "auto", priority: 0.2 });
    const fleet: Sm[] = [];
    for (let i = 0; i < N; i++) {
      fleet.push({
        m: await enroll(`sim-${i}`),
        i,
        agent: i >= 1 && i <= N_LEGACY ? LEGACY : "0.3.1",
        gpu: i % 3 === 0,
        rate: 0.4 + rnd() * 1.6,
        fuzzRate: 1500 + pick(5000),
        state: "idle",
        back: 0,
        work: null,
      });
    }
    const doomed = fleet[0]; // dies for good while holding a lease
    let doomedLease: any = null;

    // --- the fake trainer ---
    let gen = 1;
    let cursor = 0;
    let fuzzCursor = 0;
    const used = new Map<number, number[]>([[1, []]]); // gen -> game ids it trained on
    let extras: number[] = [];
    const credited = new Map<number, string>(); // game id -> machine (first copy)
    const seeds = new Set<number>();
    // Eval: gid -> the lease its credited row came from; batches as the trainer started them.
    const evalCredited = new Map<number, string>();
    const batches = new Map<string, { lo: number; hi: number; games: number; done: number }>();
    const bodies = new Map<string, Map<number, any>>(); // result key -> rows by gid
    let evalDone = false;
    const evalCfg = (batch: string, games: number, a: number, b: number) => ({
      batch,
      games,
      checkpoints: [a, b],
      games_per_lease: "auto",
      job: { settings: { sims: 32 }, seats: [{ net: a }, { net: b }], paired: true },
    });
    let evalNow: any = null;
    const runPut = (state: "active" | "paused" = "active") =>
      admin("PUT", `/v1/admin/run/${RUN}`, runBody(build, { quota: QUOTA, slack: SLACK, max_stale: MAX_STALE, games_per_lease: "auto", eval: evalNow, state }));
    // The run's definition: none until gen 2, then a new version for every gen and batch.
    let runDefined = false;
    const signRun = async () => {
      await define("run", RUN);
      runDefined = true;
      stats.definitions++;
    };
    async function startBatch(batch: string, games: number, a: number, b: number) {
      const before = ok(await admin("GET", `/v1/admin/run/${RUN}`)).next_gid;
      evalNow = evalCfg(batch, games, a, b);
      ok(await runPut());
      if (runDefined) await signRun();
      const st = ok(await admin("GET", `/v1/admin/run/${RUN}`)).eval_state;
      expect(st.lo % 2, "a paired batch starts on an even id").toBe(0);
      expect(st.lo).toBe(before + (before % 2));
      if (before % 2) stats.alignedSkip++;
      batches.set(batch, { lo: st.lo, hi: st.hi, games, done: 0 });
    }
    async function rowsOf(key: string): Promise<Map<number, any>> {
      if (!bodies.has(key)) {
        const text = String((await admin("GET", `/v1/admin/artifact/${key}`)).body);
        bodies.set(key, new Map(text.split("\n").filter(Boolean).map((l) => JSON.parse(l)).map((r) => [r.gid, r])));
      }
      return bodies.get(key)!;
    }
    // PROTOCOL 5.2: a credited game's row is in `key` (gids in `ids`) or in an `also` entry's key.
    async function ingestEval(r: any) {
      const here = await rowsOf(r.key);
      expect(normalize([...r.ids, ...r.dup_ids, ...r.held])).toEqual(toRanges([...here.keys()]));
      expect(r.gen).toBeNull(); // eval chunks have no generation
      const b = batches.get(r.batch)!;
      expect(b, `unknown batch ${r.batch}`).toBeTruthy();
      const take = async (key: string, ids: Range[]) => {
        const rows = await rowsOf(key);
        for (const g of idsIn(ids)) {
          expect(evalCredited.has(g), `eval game ${g} credited twice`).toBe(false);
          expect(rows.get(g)?.gid, `eval game ${g} has no row in ${key}`).toBe(g);
          expect(g >= b.lo && g < b.hi).toBe(true);
          evalCredited.set(g, r.lease_id);
          b.done++;
        }
      };
      await take(r.key, r.ids);
      for (const a of r.also) await take(a.key, a.ids);
      if (r.also.length) stats.evalAlso++;
      expect(r.games).toBe(total(r.ids) + r.also.reduce((n: number, a: any) => n + total(a.ids), 0));
    }
    let rejectedStale = 0;
    let done = false;

    async function trainerStep() {
      for (;;) {
        const page = ok(await admin("GET", `/v1/admin/run/${RUN}/results?after=${cursor}&limit=1000`));
        for (const r of page.results) {
          if (r.kind === "eval") {
            await ingestEval(r);
            continue;
          }
          // The stored bytes are what the machine sent, and the listing's ids match them.
          const body = JSON.parse((await admin("GET", `/v1/admin/artifact/${r.key}`)).body);
          expect(normalize([...r.ids, ...r.dup_ids])).toEqual(toRanges(body.gids));
          expect(r.games).toBe(total(r.ids));
          const fresh = idsIn(r.ids);
          for (const id of fresh) {
            expect(credited.has(id), `game id ${id} credited twice`).toBe(false);
            credited.set(id, r.machine_id);
          }
          if (r.gen < gen - MAX_STALE) {
            rejectedStale += fresh.length;
            continue;
          }
          if (done) continue;
          for (const id of fresh) {
            const u = used.get(gen)!;
            if (u.length < QUOTA) u.push(id);
            else extras.push(id);
          }
          // A full generation: train, publish the next checkpoint, carry the extras over.
          while (!done && used.get(gen)!.length === QUOTA) {
            ok(await admin("POST", `/v1/admin/run/${RUN}/accept`, { gen, games: QUOTA + extras.length }));
            if (gen === GENS) {
              done = true;
              break;
            }
            gen++;
            await publishCkpt(RUN, gen); // signs the next version itself once the run has one
            if (runDefined) stats.definitions++;
            if (gen === 2 && !runDefined) await signRun(); // the cutover: legacy agents are done here
            if (gen === 2 && !batches.has("e1")) await startBatch("e1", E1_GAMES, 2, 1);
            used.set(gen, extras.slice(0, QUOTA));
            extras = extras.slice(QUOTA);
          }
        }
        cursor = page.cursor;
        if (page.results.length < 1000) break;
      }
      // Eval: e1 complete -> e2 (the current gen against gen 1, which self-play left long ago).
      const e1 = batches.get("e1");
      if (e1 && e1.done === e1.games && !batches.has("e2")) await startBatch("e2", E2_GAMES, gen, 1);
      const e2 = batches.get("e2");
      if (e2 && e2.done === e2.games) evalDone = true;
      if (!done) ok(await admin("POST", `/v1/admin/run/${RUN}/accept`, { gen, games: used.get(gen)!.length + extras.length }));
      ok(await admin("POST", `/v1/admin/run/${RUN}/ack`, { cursor }));
      // Fuzz results: every seed counted once.
      const fz = ok(await admin("GET", `/v1/admin/campaign/${CAMPAIGN}/results?after=${fuzzCursor}&limit=1000`));
      for (const r of fz.results) {
        for (const s of idsIn(r.ids)) {
          expect(seeds.has(s), `seed ${s} credited twice`).toBe(false);
          seeds.add(s);
        }
      }
      fuzzCursor = fz.cursor;
      ok(await admin("POST", `/v1/admin/campaign/${CAMPAIGN}/ack`, { cursor: fuzzCursor }));
      // The coordinator never has more than quota x (1 + slack) accepted + in flight.
      if (!done) {
        const st = ok(await admin("GET", `/v1/admin/run/${RUN}`)).selfplay_state;
        expect(st.accepted + st.in_flight, JSON.stringify(st)).toBeLessThanOrEqual(st.cap);
      }
    }

    // --- the fake agent ---
    function chunkOf(w: Work): Chunk | null {
      if (w.kind === "eval") {
        if (!w.played.length) return null;
        const gids = w.played.map((p) => p.id);
        w.played = [];
        const rows = gids.map((g) => JSON.stringify({ gid: g, pair: Math.floor(g / 2), result: g % 3 ? [1, 0] : [0.5, 0.5], lease: w.l.lease_id }));
        return { seq: w.seq++, ids: toRanges(gids), content: rows.join("\n") };
      }
      if (w.kind === "selfplay") {
        if (!w.played.length) return null;
        const gids = w.played.map((p) => p.id);
        const g = Math.min(...w.played.map((p) => p.gen));
        w.played = [];
        return { seq: w.seq++, ids: toRanges(gids), gen: g, content: JSON.stringify({ lease: w.l.lease_id, gids, gen: g }) };
      }
      const first = w.l.job.first;
      const ids: Range[] = w.fuzzDone ? [[first, first + w.fuzzDone]] : [];
      return {
        seq: w.seq++,
        ids: ids.length ? ids : null,
        content: JSON.stringify({ campaign: CAMPAIGN, mode: "delver", first, count: w.l.job.count, done: w.fuzzDone, failures: [], gids: idsIn(ids) }),
      };
    }

    // Uploads every pending chunk. Returns false if the lease is gone (cancelled).
    async function flush(s: Sm, late: boolean): Promise<boolean> {
      const w = s.work!;
      while (w.chunks.length) {
        const c = w.chunks[0];
        const res = await upload(s.m, w.l.lease_id, c.seq, c.content, { ids: c.ids, gen: c.gen });
        if (res.status === 200) {
          w.uploaded.push(c.seq);
          if (w.kind === "eval") {
            expect(res.body.held, JSON.stringify(res.body)).toBeGreaterThanOrEqual(0);
            if (res.body.held) stats.evalHeldUploads++;
          }
          if (late) {
            stats.lateTaken++;
            if (res.body.already_credited) stats.lateDup++;
          }
          // Double upload: an exact repeat is a no-op...
          if (rnd() < 0.15) {
            const again = ok(await upload(s.m, w.l.lease_id, c.seq, c.content, { ids: c.ids, gen: c.gen }));
            expect(again.duplicate).toBe(true);
            stats.idempotent++;
          }
          // ...and a different body under the same seq is refused.
          if (rnd() < 0.05) {
            const bad = await upload(s.m, w.l.lease_id, c.seq, c.content + " ", { ids: c.ids, gen: c.gen });
            expect(bad.status).toBe(409);
            stats.conflicts++;
          }
        } else {
          expect(res.status, JSON.stringify(res.body)).toBe(410);
          if (w.kind === "eval") expect(res.body.message, "eval chunks never go stale").not.toMatch(/stale/);
          if (/stale/.test(res.body.message)) stats.stale410++;
          else {
            if (late) stats.late410++;
            return false; // cancelled
          }
        }
        w.chunks.shift();
      }
      return true;
    }

    async function closeWork(s: Sm, givenBack: boolean) {
      const w = s.work!;
      const c = chunkOf(w);
      if (c) w.chunks.push(c);
      if (await flush(s, false)) {
        const unfinished = w.kind === "fuzz" ? { seeds: toRanges(w.todo) } : { gids: toRanges(w.todo) };
        const r = await finish(s.m, w.l.lease_id, w.uploaded, { given_back: givenBack, unfinished });
        expect([200, 410]).toContain(r.status);
      }
      s.work = null;
      s.state = "idle";
    }

    async function doBeat(s: Sm, state: "running" | "paused"): Promise<void> {
      const w = s.work!;
      w.lastBeat = step;
      const r = await beat(s.m, w.l.lease_id, state, { stats: { games: w.uploaded.length, yields: w.pausedSteps ? 1 : 0 } });
      if (r.status === 410) {
        stats.expiredOnReturn++;
        s.work = null;
        s.state = "idle";
        return;
      }
      const b = ok(r);
      if (b.action === "cancel") {
        stats.cancelled++;
        s.work = null;
        s.state = "idle";
      } else if (b.action === "give_back") {
        if (state === "paused") stats.coordGiveBacks++;
        await closeWork(s, true);
      } else if (b.checkpoint && b.checkpoint.gen > w.gen) {
        // Rev 6: a definition lease switches only to a gen its own definition signs.
        if (w.def) expect(w.def.checkpoints[String(b.checkpoint.gen)], "a switch the lease's definition signs").toBeTruthy();
        w.gen = b.checkpoint.gen; // at the next game boundary
        stats.checkpointSwitches++;
      }
    }

    async function machineStep(s: Sm, draining: boolean) {
      if (s.state === "dead") return;
      if (s.state === "offline") {
        if (step < s.back && !draining) return;
        s.state = s.work ? "working" : "idle";
        if (s.work) {
          // Back: first the results it still holds (late, if the lease expired meanwhile), then a beat.
          if (!(await flush(s, true))) {
            s.work = null;
            s.state = "idle";
            return;
          }
          await doBeat(s, "running");
        }
        return;
      }
      if (s.state === "idle") {
        if (draining || rnd() < 0.2) return;
        const r = ok(await ask(s.m, { caps: { threads: 8, ram_mb: 16000, gpu: s.gpu, disk_mb: 4000 }, agent: s.agent, env: CAP }));
        expect(r.reason, "one lease at a time never meets the cap").not.toBe("too_many_leases");
        if (!r.lease) {
          if (s.agent === LEGACY && runDefined) stats.legacyIdleAfterCutover++;
          return;
        }
        const l = r.lease;
        stats.leases++;
        const kind = l.kind as "selfplay" | "fuzz" | "eval";
        // Rev 6: a definition-aware agent gets only definition leases, checked as it would; an
        // older one only legacy self-play leases, and only before the run had a definition.
        let def: any = null;
        if (s.agent === LEGACY) {
          stats.legacyLeases++;
          expect(l.definition, "a legacy agent gets no definition").toBeUndefined();
          expect(kind, "the campaign is signed: no legacy fuzz").toBe("selfplay");
          expect(runDefined, "no legacy lease once the run has a definition").toBe(false);
          expect(l.job.settings).toEqual({ sims: 64 });
        } else {
          stats.defLeases++;
          def = await checkDefinitionLease(s.m, l);
        }
        let todo: number[];
        if (kind === "eval") {
          // Rev 5: both networks, whole pairs, no single checkpoint; rev 6: the seats are signed.
          stats.evalLeases++;
          expect(l.checkpoint).toBeNull();
          expect(l.checkpoints.map((c: any) => c.gen)).toEqual(def.jobs.eval.seats.map((x: any) => x.net));
          expect(l.job.gid_base % 2).toBe(0);
          expect(l.job.games % 2).toBe(0);
          for (let g = l.job.gid_base; g < l.job.gid_base + l.job.games; g += 2) {
            if (leasedPairs.has(g)) stats.evalPairsReplayed++;
            leasedPairs.add(g);
          }
        }
        if (kind === "selfplay" || kind === "eval") {
          todo = Array.from({ length: l.job.games }, (_, i) => l.job.gid_base + i);
          for (let i = todo.length - 1; i > 0; i--) {
            const j = pick(i + 1);
            [todo[i], todo[j]] = [todo[j], todo[i]];
          }
        } else todo = [];
        s.work = { l, def, kind, todo, played: [], fuzzDone: 0, chunks: [], uploaded: [], seq: 0, gen: l.checkpoint?.gen ?? 0, pausedFor: 0, pausedSteps: 0, lastBeat: step };
        if (kind === "fuzz") s.work.todo = Array.from({ length: l.job.count }, (_, i) => l.job.first + i);
        s.state = "working";
        return;
      }

      // working
      const w = s.work!;
      if (!draining) {
        if (s === doomed && step >= 25 && w.kind === "selfplay" && !doomedLease) {
          doomedLease = w.l;
          stats.died++;
          s.state = "dead";
          return;
        }
        if (rnd() < 0.03) {
          stats.vanished++;
          if (w.kind === "eval") stats.evalVanished++;
          s.state = "offline";
          s.back = step + 2 + pick(40); // up to 20 minutes: often past the 3-beat expiry
          if (rnd() < 0.4) {
            stats.crashedLosingGames++;
            w.played = []; // games in progress are lost; finished chunks are on disk
          } else {
            const c = chunkOf(w);
            if (c) w.chunks.push(c);
          }
          return;
        }
        if (w.pausedFor === 0 && rnd() < 0.03) w.pausedFor = 1 + pick(30); // up to 15 min: sometimes past the slack
      }
      if (w.pausedFor > 0 && !draining) {
        w.pausedFor--;
        w.pausedSteps++;
        if (w.pausedSteps * STEP_S > SLACK_S && rnd() < 0.5) {
          stats.agentGiveBacks++;
          await closeWork(s, true);
          return;
        }
        if (step - w.lastBeat >= 2) await doBeat(s, "paused");
        return;
      }
      w.pausedSteps = 0;

      // Play (eval games finish in any order, so pairs get split across chunks).
      if (w.kind === "selfplay" || w.kind === "eval") {
        let n = Math.floor(s.rate + rnd());
        while (n-- > 0 && w.todo.length) w.played.push({ id: w.todo.pop()!, gen: w.gen });
        if (w.played.length && (rnd() < 0.6 || !w.todo.length)) w.chunks.push(chunkOf(w)!);
      } else {
        const n = Math.min(s.fuzzRate, w.todo.length);
        w.todo.splice(0, n);
        w.fuzzDone += n;
        if (rnd() < 0.05) {
          stats.fuzzFailures++;
          const seed = w.l.job.first + Math.max(0, w.fuzzDone - 1);
          w.chunks.push({
            seq: w.seq++,
            ids: null,
            content: JSON.stringify({ campaign: CAMPAIGN, failures: [{ seed, site: `engine/src/x.rs:${seed % 7}`, panic: "boom", repro: "r" }], gids: [] }),
          });
        }
      }
      if (!(await flush(s, false))) {
        s.work = null;
        s.state = "idle";
        return;
      }
      if (!w.todo.length && !w.played.length) {
        await closeWork(s, false);
        return;
      }
      if (step - w.lastBeat >= 2) await doBeat(s, "running");
    }

    // --- run until the trainer has all generations and both eval batches ---
    const leasedPairs = new Set<number>();
    let step = 0;
    for (; step < 1500 && !(done && evalDone); step++) {
      advance(STEP_S);
      const order = [...fleet].sort(() => rnd() - 0.5);
      for (const s of order) await machineStep(s, false);
      await trainerStep();
    }
    expect(done, `trainer finished ${gen - 1} of ${GENS} generations in ${step} steps`).toBe(true);
    expect(evalDone, `eval batches: ${JSON.stringify([...batches])}`).toBe(true);

    // --- drain: stop handing out work, let everyone come back and wind down ---
    ok(await runPut("paused"));
    await makeCampaign(CAMPAIGN, { build, seed_base: SEED_BASE, chunk: "auto", state: "paused" });
    for (let i = 0; i < 20; i++, step++) {
      advance(STEP_S);
      for (const s of fleet) await machineStep(s, true);
    }
    advance(400);
    await runCron(env, clock.now);
    await trainerStep();

    // --- every game id and seed handed out is claimed exactly once or back in the pool ---
    const run = ok(await admin("GET", `/v1/admin/run/${RUN}`));
    const runAcc = await expectTiled(`run:${RUN}`, 0, run.next_gid);
    const camp = ok(await admin("GET", `/v1/admin/campaign/${CAMPAIGN}`));
    const fzAcc = await expectTiled(`campaign:${CAMPAIGN}`, SEED_BASE, camp.cursor);
    // ... and the trainer saw exactly the claimed ones.
    expect(credited.size + evalCredited.size).toBe(total(runAcc.claims.map((c: any) => [c.lo, c.hi])));

    // --- every eval batch: every pair credited exactly once, both games from one lease ---
    for (const [name, b] of batches) {
      expect(b.done, `batch ${name}`).toBe(b.games);
      for (let g = b.lo; g < b.hi; g += 2) {
        expect(evalCredited.has(g) && evalCredited.has(g + 1), `pair ${g} of ${name}`).toBe(true);
        expect(evalCredited.get(g), `pair ${g} of ${name} from one lease`).toBe(evalCredited.get(g + 1));
      }
      const st = ok(await admin("GET", `/v1/admin/run/${RUN}`)).eval_batches.find((x: any) => x.batch === name);
      expect(st).toMatchObject({ done: b.games, leased: 0, unissued: 0 });
    }
    expect(seeds.size).toBe(total(fzAcc.claims.map((c: any) => [c.lo, c.hi])));

    // --- every generation was filled exactly by accepted games, none used twice ---
    const all = new Set<number>();
    for (let g = 1; g <= GENS; g++) {
      const u = used.get(g)!;
      expect(u.length, `gen ${g}`).toBe(QUOTA);
      for (const id of u) {
        expect(all.has(id)).toBe(false);
        all.add(id);
        expect(credited.has(id)).toBe(true);
      }
      // No generation depends on one machine.
      expect(new Set(u.map((id) => credited.get(id))).size, `machines in gen ${g}`).toBeGreaterThanOrEqual(3);
    }
    const quotas = (await env.DB.prepare(`SELECT gen, accepted FROM quotas WHERE run = ? ORDER BY gen`).bind(RUN).all<any>()).results;
    for (const q of quotas) expect(q.accepted).toBeGreaterThanOrEqual(QUOTA);

    // --- the machine that died for good held a lease; its games were played by others ---
    expect(doomedLease).not.toBeNull();
    const [lo, hi] = [doomedLease.job.gid_base, doomedLease.job.gid_base + doomedLease.job.games];
    const doomedRow = await env.DB.prepare(`SELECT state FROM leases WHERE lease_id = ?`).bind(doomedLease.lease_id).first<any>();
    expect(doomedRow.state).toBe("expired");
    const takenByOthers = [...credited].filter(([id, mid]) => id >= lo && id < hi && mid !== doomed.m.machine_id).length;
    expect(takenByOthers).toBeGreaterThan(0);

    // --- the scenarios really happened ---
    console.log("fleet simulation:", JSON.stringify({ steps: step, gens: GENS, games: credited.size, seeds: seeds.size, rejectedStale, ...stats }));
    expect(stats.vanished).toBeGreaterThan(5);
    expect(stats.lateTaken).toBeGreaterThan(0);
    expect(stats.lateDup).toBeGreaterThan(0);
    expect(stats.expiredOnReturn).toBeGreaterThan(0);
    expect(stats.agentGiveBacks + stats.coordGiveBacks).toBeGreaterThan(0);
    expect(stats.idempotent).toBeGreaterThan(0);
    expect(stats.conflicts).toBeGreaterThan(0);
    // Rev 6: both kinds of lease happened, legacy agents stopped at the cutover.
    expect(stats.legacyLeases).toBeGreaterThan(5);
    expect(stats.defLeases).toBeGreaterThan(50);
    expect(stats.legacyIdleAfterCutover).toBeGreaterThan(10);
    expect(stats.definitions).toBeGreaterThanOrEqual(GENS);
    expect(stats.leases).toBeGreaterThan(50);
    expect(stats.evalLeases).toBeGreaterThan(10);
    expect(stats.evalHeldUploads).toBeGreaterThan(0);
    expect(stats.evalAlso).toBeGreaterThan(0);
    expect(stats.evalVanished).toBeGreaterThan(0);
    expect(stats.evalPairsReplayed).toBeGreaterThan(0);
  }, 600_000);
});
