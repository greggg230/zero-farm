// Signed definitions (PROTOCOL 7, rev 6): registering one (signature against DATA_PUBKEYS only,
// contents, builds, checkpoints, versions only going up), and which leases they allow: legacy
// leases only for agents older than DEFINITION_AGENT and only while a run or campaign has no
// definition; definition leases only with builds, runtime, checkpoints and job kinds it names,
// carrying only per-lease fields; refusals.

import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { looksLikePath } from "../src/definitions";
import {
  LEGACY,
  admin,
  advance,
  ask,
  ask as askNow,
  call,
  checkDefinitionLease,
  clock,
  define,
  definitionFor,
  enroll,
  err,
  finish,
  lease,
  lease as leaseNow,
  makeBuild,
  makeCampaign,
  makeRun,
  minisign,
  newSignKey,
  ok,
  publishCkpt,
  putArtifact,
  putDefinition,
  runBody,
  sha256,
  upload,
} from "./helpers";

const register = (doc: any, key: string, sha: string, o: { env?: Record<string, string> } = {}) =>
  admin("PUT", "/v1/admin/definition", { kind: doc.kind, id: doc.id, version: doc.version, key, sha256: sha }, { env: o.env });

async function upload2(doc: any, text = JSON.stringify(doc)) {
  const bytes = new TextEncoder().encode(text);
  const key = `defs/${doc.kind}/${doc.id}/${doc.version}/definition.json`;
  await putArtifact(key, bytes);
  return { key, bytes, sha: await sha256(bytes) };
}

describe("registering definitions", () => {
  it("verifies, registers and makes a definition current; GET run and stats show it", async () => {
    await makeRun("r", { define: false });
    const doc = await definitionFor("run", "r");
    const ans = ok(await putDefinition(doc));
    expect(ans).toMatchObject({ ok: true, kind: "run", id: "r", version: 1, key: "defs/run/r/1/definition.json", unchanged: false, current: true });
    const row = await env.DB.prepare(`SELECT * FROM definitions`).first<any>();
    expect(row).toMatchObject({ kind: "run", id: "r", version: 1, key_id: "233415" + "4E05C886F7", game: "mtg" });
    const run = ok(await admin("GET", "/v1/admin/run/r"));
    expect(run.definition).toMatchObject({ version: 1, gens: [1], jobs: ["selfplay"], problems: [] });
    const s = ok(await admin("GET", "/v1/admin/stats"));
    expect(s.runs[0].definition).toMatchObject({ version: 1, problems: [] });
    expect(s.limits).toMatchObject({ definition_agent: "0.2.5", data_keys: 1 });
    // The same version again with the same bytes is fine; other bytes, or a lower version, are not.
    expect(ok(await putDefinition(doc)).unchanged).toBe(true);
    err(await register(doc, ans.key, "0".repeat(64)), 400, "bad_request"); // sha256 doesn't match the stored bytes
    ok(await putDefinition({ ...doc, version: 3 }));
    expect(err(await putDefinition({ ...doc, version: 2 }), 409, "conflict").message).toMatch(/versions only go up/);
    // A registered version's files can't be replaced (agents check their sha256); the same bytes
    // again are a no-op (a retried publish).
    err(await admin("PUT", "/v1/admin/artifact/defs/run/r/3/definition.json", undefined, { raw: "{}" }), 409, "conflict");
    err(await admin("PUT", "/v1/admin/artifact/defs/run/r/3/definition.json.minisig", undefined, { raw: "x" }), 409, "conflict");
    err(await admin("POST", "/v1/admin/artifact/defs/run/r/3/definition.json?uploads"), 409, "conflict");
    const same = JSON.stringify({ ...doc, version: 3 }, null, 2);
    expect(ok(await admin("PUT", "/v1/admin/artifact/defs/run/r/3/definition.json", undefined, { raw: same })).unchanged).toBe(true);
    // Only defs/<kind>/<id>/<version>/definition.json(.minisig) under defs/.
    err(await admin("PUT", "/v1/admin/artifact/defs/run/r/4/other.json", undefined, { raw: "{}" }), 400, "bad_request");
  });

  it("only a data key's signature counts: unsigned, other keys, release keys and tampered bytes are refused", async () => {
    await makeRun("r", { define: false });
    const doc = await definitionFor("run", "r");
    const { key, bytes, sha } = await upload2(doc);
    expect(err(await register(doc, key, sha), 400, "bad_request").message).toMatch(/minisig is not uploaded/);
    // Another key, even one the coordinator trusts for builds (RELEASE_PUBKEYS).
    const other = await newSignKey();
    await putArtifact(`${key}.minisig`, await minisign(bytes, other.key));
    expect(err(await register(doc, key, sha), 400, "bad_request").message).toMatch(/not in DATA_PUBKEYS/);
    expect(err(await register(doc, key, sha, { env: { RELEASE_PUBKEYS: other.pub } }), 400, "bad_request").message).toMatch(/not in DATA_PUBKEYS/);
    // The data key's signature of other bytes.
    await putArtifact(`${key}.minisig`, await minisign(new TextEncoder().encode(JSON.stringify({ ...doc, builds: [] }))));
    expect(err(await register(doc, key, sha), 400, "bad_request").message).toMatch(/does not verify/);
    // No data keys configured: nothing can be registered.
    await putArtifact(`${key}.minisig`, await minisign(bytes));
    expect(err(await register(doc, key, sha, { env: { DATA_PUBKEYS: "" } }), 400, "bad_request").message).toMatch(/DATA_PUBKEYS is not set/);
    ok(await register(doc, key, sha));
    // A key that isn't where kind/id/version say.
    expect(err(await admin("PUT", "/v1/admin/definition", { kind: "run", id: "r", version: 2, key, sha256: sha }), 400, "bad_request").message).toMatch(
      /key must be defs\/run\/r\/2/,
    );
  });

  it("checks the contents: fields, builds, runtimes, checkpoints, templates", async () => {
    const build = await makeRun("r", { define: false });
    const camp = await makeBuild({ tag: "fz" });
    const rt = await makeBuild({ kind: "runtime", tag: "rt" });
    const base = await definitionFor("run", "r");
    const problems = async (mutate: (d: any) => void) => {
      const d = structuredClone(base);
      mutate(d);
      return err(await putDefinition(d), 400, "bad_request").problems as string[];
    };
    expect(await problems((d) => (d.definition = 2))).toContain("definition must be 1");
    expect(await problems((d) => (d.builds = ["f".repeat(24)]))).toEqual([`build ${"f".repeat(24)} is not registered (POST /v1/admin/build first)`]);
    expect(await problems((d) => (d.builds = [rt]))).toEqual([`build ${rt} is a runtime build, not a worker`]);
    expect(await problems((d) => (d.runtime = build))).toEqual([`runtime ${build} is a worker build`]);
    expect(await problems((d) => (d.game = "other"))).toEqual([`build ${build} is for game mtg, not other`]);
    expect(await problems((d) => (d.jobs.selfplay.games = 10))).toEqual([
      "jobs.selfplay.games is not allowed (a selfplay template has settings; per-lease fields are the coordinator's)",
    ]);
    expect(await problems((d) => (d.jobs.selfplay.seed = 7))).toHaveLength(1); // the seed is per lease too
    // The agent's other rules: no unknown fields, no path-like strings, sizes, times.
    expect(await problems((d) => (d.note = "x"))).toEqual(["note: unknown field"]);
    const winPath = ["C:", "Users", "x", "a.txt"].join(String.fromCharCode(92));
    expect(await problems((d) => (d.jobs.selfplay.settings = { book: winPath }))).toEqual([`jobs.selfplay.settings.book: ${JSON.stringify(winPath)} looks like a file path`]);
    expect(await problems((d) => (d.jobs.selfplay.settings = { share: "x" + String.fromCharCode(92) + "y" }))).toHaveLength(1);
    expect(await problems((d) => (d.jobs.selfplay.settings = { "../x": 1 }))).toEqual(['jobs.selfplay.settings: key "../x" looks like a file path']);
    // Card names with the Oracle separator " // " in a game spec (the Delver 75's "Rough // Tumble") are not paths;
    // any other "/" still is.
    expect(looksLikePath(JSON.stringify({ decks: { p0: ["Rough // Tumble", "Tamiyo, Inquisitive Student // Tamiyo, Seasoned Scholar"] } }))).toBe(false);
    expect(await problems((d) => (d.jobs.selfplay.settings = { spec: "Rough //Tumble" }))).toHaveLength(1);
    expect(await problems((d) => (d.jobs.selfplay.settings = { spec: "x // ../y" }))).toHaveLength(1);
    expect(await problems((d) => (d.checkpoints["1"].extra = 1))).toEqual(["checkpoints.1.extra: unknown field"]);
    expect(await problems((d) => (d.checkpoints["1"].model_size = 0))).toEqual(["checkpoints.1.model_size must be 1 to 1073741824 bytes"]);
    expect(await problems((d) => (d.expires = d.created))).toContain("expires must be Unix seconds after created, or null");
    expect(await problems((d) => (d.jobs.selfplay = {}))).toEqual(["jobs.selfplay.settings must be an object"]);
    expect(await problems((d) => (d.jobs = { selfplay: null, eval: null, fuzz: null }))).toEqual(["jobs names no job (selfplay, eval or fuzz)"]);
    expect(await problems((d) => (d.jobs.fuzz = { mode: "delver-games" }))).toEqual(["a run's definition has no fuzz job (campaigns fuzz)"]);
    expect(await problems((d) => (d.jobs.eval = { settings: {}, seats: [{ net: 1 }, { bot: "random" }] }))).toEqual(["jobs.eval.paired must be true or false"]);
    expect(await problems((d) => (d.jobs.eval = { settings: {}, seats: [{ net: 9 }, { bot: "random" }], paired: true }))).toEqual([
      "jobs.eval.seats name gen 9, which checkpoints doesn't list",
    ]);
    expect(await problems((d) => (d.jobs.eval = { settings: {}, seats: [{ net: 1 }, { bot: "ext:x" }], paired: false }))).toEqual([
      "jobs.eval.seats[1]: ext: bots can't run on the farm (ext:x)",
    ]);
    expect(await problems((d) => (d.checkpoints["1"].model_sha256 = "0".repeat(64)))).toEqual(["checkpoint 1's model differs from definition v1"]);
    expect(await problems((d) => (d.checkpoints["1"].model_size += 1))).toEqual(["checkpoint 1's model differs from definition v1"]);
    expect(await problems((d) => (d.checkpoints["1"].reference_size += 1))).toEqual(["checkpoint 1's reference size differs from definition v1"]);
    expect(await problems((d) => ((d.created = clock.now - 10), (d.expires = clock.now)))).toEqual([`it expired at ${clock.now}`]);
    // The signed body names another run and version than the key and the request.
    const bytes = new TextEncoder().encode(JSON.stringify({ ...base, id: "other" }));
    const key = "defs/run/r/9/definition.json";
    await putArtifact(key, bytes);
    await putArtifact(`${key}.minisig`, await minisign(bytes));
    const wrong = err(await admin("PUT", "/v1/admin/definition", { kind: "run", id: "r", version: 9, key, sha256: await sha256(bytes) }), 400, "bad_request");
    expect(wrong.problems).toEqual(["the signed definition is run other version 1, not run r version 9"]);
    // Campaigns: a fuzz job only, no checkpoints or runtime.
    await makeCampaign("fz", { build: camp, define: false });
    const c = await definitionFor("campaign", "fz");
    expect(err(await putDefinition({ ...c, jobs: { ...c.jobs, selfplay: { settings: {} } } }), 400, "bad_request").problems).toEqual([
      "a campaign's definition has only a fuzz job",
    ]);
    ok(await putDefinition(c));
    // A gen the current definition signs can only be registered with exactly those files.
    await define("run", "r", (d) => (d.checkpoints["2"] = { model_sha256: "1".repeat(64), model_size: 5, reference_sha256: null, reference_size: null }));
    await putArtifact("ckpt/r/2/model.onnx", "a different model");
    expect(
      err(await admin("PUT", "/v1/admin/run/r/checkpoint/2", { model: "ckpt/r/2/model.onnx", sha256: await sha256("a different model") }), 400, "bad_request").message,
    ).toMatch(/checkpoint 2's model differs/);
  });
});

describe("leases under definitions", () => {
  it("legacy leases only for older agents and only until the run or campaign has a definition", async () => {
    await makeRun("r", { define: false, games_per_lease: 4 });
    const m = await enroll("pc");
    // No definition yet: an older agent gets a legacy lease (the coordinator's job object)...
    const old = await lease(m, { agent: LEGACY });
    expect(old.definition).toBeUndefined();
    expect(old.job).toMatchObject({ kind: "selfplay", settings: { sims: 64 }, games: 4 });
    // ...a definition-aware agent nothing (it would refuse a lease without one).
    expect(ok(await ask(m, { agent: "0.2.5" })).reason).toBe("no_work");
    await define("run", "r");
    const l = await lease(m, { agent: "0.2.5" });
    await checkDefinitionLease(m, l);
    expect(l.job.seed).toBe(old.job.seed); // the same base seed either way
    // Now older agents get nothing here, or are told to update when their channel can.
    expect(ok(await ask(m, { agent: LEGACY })).reason).toBe("no_work");
    const a = await makeBuild({ kind: "agent", version: "0.2.5" });
    ok(await admin("POST", "/v1/admin/agent/release", { version: "0.2.5", build_id: a, channel: "stable", min_agent: "0.2.0" }));
    expect(ok(await ask(m, { agent: LEGACY }))).toEqual({ update: { min_agent: "0.2.5" } });
    // The legacy lease already out runs to its end.
    ok(await upload(m, old.lease_id, 0, "games", { ids: [[0, 4]] }));
    ok(await finish(m, old.lease_id, [0]));
    // DEFINITION_AGENT is a var.
    expect(ok(await ask(m, { agent: "0.2.5", env: { DEFINITION_AGENT: "0.3.0" } })).reason).toBe("no_work");
  });

  it("issues only builds, runtimes, checkpoints and job kinds the current definition names", async () => {
    const w1 = await makeRun("r", { games_per_lease: 2 });
    const m = await enroll("pc");
    // One lease request per 5 s (the per-machine rate limit).
    const ask = (x: typeof m, o: Parameters<typeof askNow>[1] = {}) => (advance(5), askNow(x, o));
    const lease = (x: typeof m, o: Parameters<typeof leaseNow>[1] = {}) => (advance(5), leaseNow(x, o));
    await checkDefinitionLease(m, await lease(m));
    // A new build: no self-play until a definition signs it.
    const w2 = await makeBuild({ tag: "w2" });
    ok(await admin("PUT", "/v1/admin/run/r", runBody(w2, { games_per_lease: 2 })));
    expect(ok(await ask(m)).reason).toBe("no_work");
    expect(ok(await admin("GET", "/v1/admin/run/r")).definition.problems).toEqual([`build ${w2} is not in definition v1`]);
    await define("run", "r");
    expect((await lease(m)).build.build_id).toBe(w2);
    // A runtime the definition doesn't name.
    const rt = await makeBuild({ kind: "runtime", tag: "rt" });
    ok(await admin("PUT", "/v1/admin/run/r", { ...runBody(w2, { games_per_lease: 2 }), runtime: rt }));
    expect(ok(await ask(m)).reason).toBe("no_work");
    await define("run", "r");
    const lr = await lease(m);
    expect(lr.runtime.build_id).toBe(rt);
    await checkDefinitionLease(m, lr);
    // A new current gen not signed yet: no self-play until it is (publishCkpt signs it at once, so
    // register it by hand here).
    await putArtifact("ckpt/r/2/model.onnx", "m2");
    ok(await admin("PUT", "/v1/admin/run/r/checkpoint/2", { model: "ckpt/r/2/model.onnx", sha256: await sha256("m2") }));
    expect(ok(await ask(m)).reason).toBe("no_work");
    await define("run", "r");
    expect((await lease(m)).checkpoint.gen).toBe(2);
    // A definition without a self-play job.
    await define("run", "r", (d) => {
      d.jobs.selfplay = null;
      d.jobs.eval = { settings: {}, seats: [{ net: 2 }, { bot: "random" }], paired: false };
    });
    expect(ok(await ask(m)).reason).toBe("no_work");
    expect(ok(await admin("GET", "/v1/admin/run/r")).definition.problems).toContain("definition v5 has no selfplay job");
    expect(w1).not.toBe(w2);
  });

  it("eval batches lease only with the networks and pairing the definition seats", async () => {
    const w = await makeRun("r");
    const ev = { batch: "b1", games: 4, checkpoints: [1], games_per_lease: 2, job: { seats: [{ net: 1 }, { bot: "random" }], paired: true } };
    ok(await admin("PUT", "/v1/admin/run/r", runBody(w, { eval: ev })));
    const m = await enroll("pc");
    // v1 has no eval job yet.
    expect(ok(await ask(m, { kinds: ["eval"] })).reason).toBe("no_work");
    await define("run", "r", (d) => (d.jobs.eval = { settings: {}, seats: [{ net: 1 }, { bot: "random" }], paired: false }));
    expect(ok(await ask(m, { kinds: ["eval"] })).reason).toBe("no_work"); // pairing differs
    expect(ok(await admin("GET", "/v1/admin/run/r")).definition.problems[0]).toMatch(/is paired, definition v2's eval job is not/);
    await define("run", "r");
    const l = await lease(m, { kinds: ["eval"] });
    expect(l.checkpoints.map((c: any) => c.gen)).toEqual([1]);
    expect((await checkDefinitionLease(m, l)).jobs.eval.paired).toBe(true);
  });

  it("a run without self-play reports no self-play problem; gens are compared as numbers", async () => {
    const w = await makeBuild({ tag: "h2h" });
    ok(await admin("PUT", "/v1/admin/run/h", { ...runBody(w, { selfplay: false }), eval: null }));
    await publishCkpt("h", 1);
    const ev = { batch: "b1", games: 2, checkpoints: [1], job: { settings: {}, seats: [{ net: 1 }, { bot: "random" }], paired: true } };
    ok(await admin("PUT", "/v1/admin/run/h", { ...runBody(w, { selfplay: false }), eval: ev }));
    await define("run", "h", (d) => (d.checkpoints["2"] = { model_sha256: "1".repeat(64), model_size: 5, reference_sha256: null, reference_size: null }));
    expect(ok(await admin("GET", "/v1/admin/run/h")).definition.problems).toEqual([]);
    await putArtifact("ckpt/h/2/model.onnx", "other");
    const body = { model: "ckpt/h/2/model.onnx", sha256: await sha256("other") };
    expect(err(await admin("PUT", "/v1/admin/run/h/checkpoint/02", body), 400, "bad_request").message).toMatch(/checkpoint 2's model differs/);
  });

  it("an expired definition gives no leases", async () => {
    await makeCampaign("fz", { define: false });
    await define("campaign", "fz", (d) => (d.expires = clock.now + 3600));
    const m = await enroll("pc");
    await checkDefinitionLease(m, await lease(m));
    advance(3600);
    expect(ok(await ask(m)).reason).toBe("no_work");
    expect(ok(await admin("GET", "/v1/admin/campaign/fz")).definition.problems).toContain("definition v1 expired");
  });

  it("agents download definitions; a refused lease is given back and counted", async () => {
    await makeCampaign("fz", { seed_base: 0, chunk: 100 });
    const m = await enroll("pc");
    const l = await lease(m);
    const got = await call("GET", `/v1/artifact/${l.definition.key}`, { token: m.token });
    expect(await sha256(got.body as string)).toBe(l.definition.sha256);
    expect((await call("GET", `/v1/artifact/${l.definition.key}.minisig`, { token: m.token })).status).toBe(200);
    expect((await call("GET", "/v1/artifact/defs/run/x/1/other.json", { token: m.token })).status).toBe(404);
    // PROTOCOL 7: the agent refuses it (done with refused), and reports it once as a crash.
    ok(await call("POST", "/v1/crash", { token: m.token, body: { kind: "definition", message: "definition: signature does not verify", lease_id: l.lease_id } }));
    expect(ok(await finish(m, l.lease_id, [], { refused: "definition" })).returned).toBe(100);
    const row = await env.DB.prepare(`SELECT state, close_reason FROM leases WHERE lease_id = ?`).bind(l.lease_id).first<any>();
    expect(row).toEqual({ state: "given_back", close_reason: "refused:definition" });
    err(await finish(m, l.lease_id, [], { refused: "Not A Reason!" }), 400, "bad_request");
    const s = ok(await admin("GET", "/v1/admin/stats"));
    expect(s.machines[0].refused_24h).toBe(1);
    expect(s.failures.crashes[0]).toMatchObject({ kind: "definition" });
    expect((await admin("GET", "/admin")).body).toContain("Signed definitions");
    // The ids went back: the next lease has them.
    expect((await lease(m)).job.first).toBe(0);
  });

  it("the trainer's flow: each new gen comes with a new definition version", async () => {
    await makeRun("r", { games_per_lease: 2 });
    const m = await enroll("pc");
    for (let g = 2; g <= 4; g++) {
      await publishCkpt("r", g); // registers the gen, then signs definition v<g>
      const l = await lease(m);
      expect([l.checkpoint.gen, l.definition.key]).toEqual([g, `defs/run/r/${g}/definition.json`]);
      const d = await checkDefinitionLease(m, l);
      expect(Object.keys(d.checkpoints)).toEqual(Array.from({ length: g }, (_, i) => String(i + 1)));
    }
  });
});
