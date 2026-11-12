// Self-play leases fed by game records (phase 3, DESIGN 7 and PROTOCOL 5 `records`): the trainer's pool remote
// publishes each generation as a NEW definition version that signs it, and only then registers it current; workers
// upload records chunks (game ids as x-ids, the oldest network as x-gen); the trainer reads them by cursor, replays
// them, posts its running total (accepted + held) and then acks. The coordinator never reads a record.

import { describe, expect, it } from "vitest";
import {
  admin,
  advance,
  ask,
  beat,
  checkDefinitionLease,
  definitionFor,
  enroll,
  finish,
  lease,
  makeBuild,
  ok,
  putArtifact,
  putDefinition,
  runBody,
  sha256,
  upload,
} from "./helpers";

const state = async (run: string) => ok(await admin("GET", `/v1/admin/run/${run}`)).selfplay_state;

// The pool's publish of `gen`: files, then a definition version signing the gen and the `keep` gens before it,
// then the registration (PROTOCOL 7: self-play switches only to gens a definition names).
async function publishGen(run: string, gen: number, keep = 1): Promise<any> {
  const model = `onnx ${run} ${gen}`;
  const ref = `reference ${run} ${gen}`;
  await putArtifact(`ckpt/${run}/${gen}/model.onnx`, model);
  await putArtifact(`ckpt/${run}/${gen}/reference.npz`, ref);
  const prev = (await definitionFor("run", run)) as any;
  const doc = prev ?? {};
  doc.checkpoints = Object.fromEntries(
    Object.entries(doc.checkpoints ?? {}).filter(([g]) => Number(g) >= gen - keep),
  );
  doc.checkpoints[String(gen)] = {
    model_sha256: await sha256(model),
    model_size: new TextEncoder().encode(model).length,
    reference_sha256: await sha256(ref),
    reference_size: new TextEncoder().encode(ref).length,
  };
  const def = ok(await putDefinition(doc));
  // between the definition and the registration, the current gen (signed too) is still leased
  const reg = ok(
    await admin("PUT", `/v1/admin/run/${run}/checkpoint/${gen}`, {
      model: `ckpt/${run}/${gen}/model.onnx`,
      sha256: await sha256(model),
      reference: `ckpt/${run}/${gen}/reference.npz`,
      reference_sha256: await sha256(ref),
    }),
  );
  expect(reg.current).toBe(true);
  return def;
}

async function poolRun(run: string, o: { quota?: number; games_per_lease?: number; max_stale?: number } = {}) {
  const build = await makeBuild({ tag: run });
  // the run first without self-play (no lease before a definition signs a gen) ...
  ok(await admin("PUT", `/v1/admin/run/${run}`, runBody(build, { selfplay: false })));
  ok(await admin("PUT", `/v1/admin/run/${run}`, runBody(build, { quota: o.quota ?? 20, games_per_lease: o.games_per_lease ?? 5, max_stale: o.max_stale ?? 1, slack: 0 })));
  return build;
}

describe("self-play with records (the trainer's pool remote)", () => {
  it("each gen: a definition that signs it, then its registration; leases carry it under that definition", async () => {
    const build = await poolRun("p");
    const m = await enroll("pc");
    expect(ok(await ask(m)).reason).toBe("no_work"); // no gen yet
    // the first definition needs the run's self-play template: definitionFor reads the run (gens: none yet)
    await publishGen("p", 80);
    const l = await lease(m);
    expect([l.checkpoint.gen, l.definition.key, l.build.build_id]).toEqual([80, "defs/run/p/1/definition.json", build]);
    const d = await checkDefinitionLease(m, l);
    expect(Object.keys(d.checkpoints)).toEqual(["80"]);
    await publishGen("p", 81);
    const l2 = await lease(m);
    expect([l2.checkpoint.gen, l2.definition.key]).toEqual([81, "defs/run/p/2/definition.json"]);
    expect(Object.keys((await checkDefinitionLease(m, l2)).checkpoints).sort()).toEqual(["80", "81"]);
    // the older lease's definition (v1) doesn't sign 81: its beats never offer it
    expect(ok(await beat(m, l.lease_id)).checkpoint).toBeNull();
  });

  it("between the definition and the registration, leases keep the current gen, signed by the new version", async () => {
    await poolRun("p");
    await publishGen("p", 5);
    const m = await enroll("pc");
    // the trainer signed v2 (gens 5 and 6) but hasn't registered 6 yet
    const doc = (await definitionFor("run", "p")) as any;
    const model = "onnx p 6";
    const ref = "reference p 6";
    await putArtifact("ckpt/p/6/model.onnx", model);
    await putArtifact("ckpt/p/6/reference.npz", ref);
    doc.checkpoints["6"] = { model_sha256: await sha256(model), model_size: 8, reference_sha256: await sha256(ref), reference_size: 13 };
    ok(await putDefinition(doc));
    const l = await lease(m);
    expect([l.checkpoint.gen, l.definition.key]).toEqual([5, "defs/run/p/2/definition.json"]);
    ok(
      await admin("PUT", "/v1/admin/run/p/checkpoint/6", {
        model: "ckpt/p/6/model.onnx",
        sha256: await sha256(model),
        reference: "ckpt/p/6/reference.npz",
        reference_sha256: await sha256(ref),
      }),
    );
    // this lease's own definition (v2) signs 6: its beat offers it (a switch at the next game boundary)
    expect(ok(await beat(m, l.lease_id)).checkpoint.gen).toBe(6);
    expect((await lease(m)).checkpoint.gen).toBe(6);
  });

  it("records chunks count as games; the trainer's total (accepted + held) goes before the ack", async () => {
    await poolRun("p", { quota: 10, games_per_lease: 5 });
    await publishGen("p", 1);
    const m = await enroll("pc");
    const l1 = await lease(m);
    const l2 = await lease(m);
    expect(ok(await ask(m)).reason).toBe("no_work"); // 10 leased = the quota
    // gzip'd records files: the coordinator stores the bytes, never reads them
    const rec = "\x1f\x8b records of games 0..3";
    const up = ok(await upload(m, l1.lease_id, 0, rec, { ids: [[0, 3]], gen: 1 }));
    expect(up).toMatchObject({ credited: 3, already_credited: 0 });
    ok(await upload(m, l1.lease_id, 1, rec + " 3..5", { ids: [[3, 5]], gen: 1 }));
    ok(await finish(m, l1.lease_id, [0, 1]));
    expect(await state("p")).toMatchObject({ accepted: 0, leased: 5, pending: 5, headroom: 0 });
    const res = ok(await admin("GET", "/v1/admin/run/p/results"));
    expect(res.results.map((r: any) => [r.kind, r.gen, r.games, r.ids])).toEqual([
      ["selfplay", 1, 3, [[0, 3]]],
      ["selfplay", 1, 2, [[3, 5]]],
    ]);
    // the trainer replays them: 4 accepted, 1 refused (a record that doesn't replay): its total is 4
    ok(await admin("POST", "/v1/admin/run/p/accept", { gen: 1, games: 4 }));
    expect(await state("p")).toMatchObject({ accepted: 4, pending: 5, headroom: -4 });
    ok(await admin("POST", "/v1/admin/run/p/ack", { cursor: res.cursor }));
    // the refused game frees one id of the quota: a new lease of 1 game
    expect(await state("p")).toMatchObject({ accepted: 4, leased: 5, pending: 0, headroom: 1 });
    expect((await lease(m)).job.games).toBe(1);
    // a duplicate upload of a credited id credits nothing
    expect(ok(await upload(m, l2.lease_id, 0, "dup", { ids: [[5, 6]], gen: 1 }))).toMatchObject({ credited: 1 });
  });

  it("staleness: a chunk older than max_stale gens is refused; within it, a late chunk still counts", async () => {
    await poolRun("p", { quota: 50, games_per_lease: 4, max_stale: 1 });
    await publishGen("p", 10);
    const m = await enroll("pc");
    const l = await lease(m);
    await publishGen("p", 11);
    // one gen behind: taken
    ok(await upload(m, l.lease_id, 0, "a", { ids: [[0, 2]], gen: 10 }));
    await publishGen("p", 12);
    advance(30);
    // two behind (max_stale 1): 410 gone, the agent drops it; its ids go back to the pool at the lease's end
    const r = await upload(m, l.lease_id, 1, "b", { ids: [[2, 4]], gen: 10 });
    expect(r.status).toBe(410);
    expect(r.body.message).toMatch(/stale/);
    ok(await finish(m, l.lease_id, [0]));
    const res = ok(await admin("GET", "/v1/admin/run/p/results"));
    expect(res.results.map((x: any) => [x.gen, x.ids])).toEqual([[10, [[0, 2]]]]);
    // a records chunk can't claim a newer gen than the run's current one, nor an older one than its lease's
    const l2 = await lease(m);
    expect(l2.checkpoint.gen).toBe(12);
    expect((await upload(m, l2.lease_id, 0, "c", { ids: [[l2.job.gid_base, l2.job.gid_base + 1]], gen: 13 })).status).toBe(400);
    expect((await upload(m, l2.lease_id, 0, "c", { ids: [[l2.job.gid_base, l2.job.gid_base + 1]], gen: 11 })).status).toBe(400);
  });
});
