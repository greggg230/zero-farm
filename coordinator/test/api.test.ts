// Every endpoint: happy path, input validation, and auth failures.

import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import {
  admin,
  advance,
  AGENT,
  ask,
  beat,
  call,
  DIGEST,
  enroll,
  err,
  finish,
  lease,
  makeBuild,
  makeCampaign,
  makeRun,
  LEGACY,
  ok,
  PROFILE,
  publishCkpt,
  putArtifact,
  runBody,
  sha256,
  upload,
} from "./helpers";

const FRIEND_ROUTES: [string, string][] = [
  ["POST", "/v1/lease"],
  ["POST", "/v1/lease/l_00000000000000000000000000/beat"],
  ["PUT", "/v1/lease/l_00000000000000000000000000/result/0"],
  ["POST", "/v1/lease/l_00000000000000000000000000/done"],
  ["GET", "/v1/artifact/builds/x/manifest.json"],
  ["GET", "/v1/agent/latest"],
  ["POST", "/v1/crash"],
  ["POST", "/v1/diag/logs"],
];

const ADMIN_ROUTES: [string, string][] = [
  ["POST", "/v1/admin/invites"],
  ["PUT", "/v1/admin/artifact/builds/x/y"],
  ["POST", "/v1/admin/artifact/builds/x/y?uploads"],
  ["GET", "/v1/admin/artifact/builds/x/y"],
  ["POST", "/v1/admin/build"],
  ["POST", "/v1/admin/agent/release"],
  ["PUT", "/v1/admin/run/r"],
  ["GET", "/v1/admin/run/r"],
  ["PUT", "/v1/admin/run/r/checkpoint/1"],
  ["GET", "/v1/admin/run/r/results"],
  ["POST", "/v1/admin/run/r/accept"],
  ["POST", "/v1/admin/run/r/ack"],
  ["PUT", "/v1/admin/campaign/c"],
  ["GET", "/v1/admin/campaign/c"],
  ["GET", "/v1/admin/campaign/c/failures"],
  ["GET", "/v1/admin/campaign/c/results"],
  ["POST", "/v1/admin/campaign/c/ack"],
  ["GET", "/v1/admin/machines"],
  ["POST", "/v1/admin/machine/m_00000000000000000000000000/revoke"],
  ["POST", "/v1/admin/machine/m_00000000000000000000000000"],
  ["POST", "/v1/admin/pause"],
  ["POST", "/v1/admin/diag"],
  ["GET", "/v1/admin/diag"],
  ["GET", "/v1/admin/stats"],
];

describe("auth", () => {
  it("friend endpoints: no token / unknown token -> 401, revoked -> 403", async () => {
    const m = await enroll("rev");
    ok(await admin("POST", `/v1/admin/machine/${m.machine_id}/revoke`, {}));
    for (const [method, path] of FRIEND_ROUTES) {
      err(await call(method, path, { body: {} }), 401, "unauthorized");
      err(await call(method, path, { token: "not-a-real-token", body: {} }), 401, "unauthorized");
      err(await call(method, path, { token: ADMIN_TOKEN(), body: {} }), 401, "unauthorized"); // admin token is not a machine
      err(await call(method, path, { token: m.token, body: {} }), 403, "revoked");
    }
  });

  it("admin endpoints: no token, a machine token or a wrong token -> 401", async () => {
    const m = await enroll("pc");
    for (const [method, path] of ADMIN_ROUTES) {
      err(await call(method, path, { body: {} }), 401, "unauthorized");
      err(await call(method, path, { token: m.token, body: {} }), 401, "unauthorized");
      err(await call(method, path, { token: ADMIN_TOKEN() + "x", body: {} }), 401, "unauthorized");
    }
  });

  it("machine tokens are stored only as sha256", async () => {
    const m = await enroll("pc");
    const row = await env.DB.prepare(`SELECT * FROM machines WHERE machine_id = ?`).bind(m.machine_id).first<any>();
    expect(row.token_hash).toBe(await sha256(m.token));
    const everything = JSON.stringify(
      await Promise.all(["machines", "invites"].map(async (t) => (await env.DB.prepare(`SELECT * FROM ${t}`).all()).results)),
    );
    expect(everything).not.toContain(m.token);
    const listed = ok(await admin("GET", "/v1/admin/machines"));
    expect(JSON.stringify(listed)).not.toContain("token_hash");
    expect(m.token).toMatch(/^[A-Za-z0-9_-]{43}$/); // 32 bytes base64url
    expect(m.machine_id).toMatch(/^m_[0-9a-z]{26}$/);
  });
});

function ADMIN_TOKEN() {
  return env.ADMIN_TOKEN;
}

describe("misc routes", () => {
  it("root, admin page, unknown route, wrong method", async () => {
    expect((await call("GET", "/")).status).toBe(200);
    const page = await call("GET", "/admin");
    expect(page.status).toBe(200);
    expect(page.headers.get("content-type")).toContain("text/html");
    expect(page.body).toContain("/v1/admin/stats");
    err(await call("GET", "/nope"), 404, "not_found");
    err(await call("GET", "/v1/lease"), 405, "bad_request");
  });
});

describe("invites and enrollment", () => {
  it("issues single-use codes in the protocol's format", async () => {
    const { codes } = ok(await admin("POST", "/v1/admin/invites", { count: 3, note: "Sam" }));
    expect(codes).toHaveLength(3);
    for (const c of codes) expect(c).toMatch(/^[A-HJ-NP-Z2-9]{4}-[A-HJ-NP-Z2-9]{4}-[A-HJ-NP-Z2-9]{4}$/);
    const body = { invite: codes[0].toLowerCase(), name: "Sam's desktop", profile: PROFILE };
    const first = ok(await call("POST", "/v1/enroll", { body }));
    expect(first.channel).toBe("stable");
    err(await call("POST", "/v1/enroll", { body }), 403, "unauthorized"); // used
    err(await call("POST", "/v1/enroll", { body: { ...body, invite: "AAAA-BBBB-CCCC" } }), 403, "unauthorized"); // unknown
    err(await call("POST", "/v1/enroll", { body: { ...body, invite: "not a code" } }), 403, "unauthorized");
  });

  it("validates input", async () => {
    err(await admin("POST", "/v1/admin/invites", { count: 0 }), 400, "bad_request");
    err(await admin("POST", "/v1/admin/invites", { count: 101 }), 400, "bad_request");
    err(await admin("POST", "/v1/admin/invites", { channel: "beta" }), 400, "bad_request");
    const { codes } = ok(await admin("POST", "/v1/admin/invites", {}));
    err(await call("POST", "/v1/enroll", { body: { invite: codes[0], name: "x" } }), 400, "bad_request"); // no profile
    err(await call("POST", "/v1/enroll", { body: { invite: codes[0], name: "", profile: PROFILE } }), 400, "bad_request");
    err(await call("POST", "/v1/enroll", { body: { invite: codes[0], name: "x", profile: { ...PROFILE, threads: "16" } } }), 400, "bad_request");
    err(await call("POST", "/v1/enroll", { raw: "{nope" }), 400, "bad_request");
    err(await call("POST", "/v1/enroll", { raw: "[1]" }), 400, "bad_request");
    // The code is still usable after the failed attempts.
    ok(await call("POST", "/v1/enroll", { body: { invite: codes[0], name: "x", profile: PROFILE } }));
  });

  it("canary invites enroll canary machines", async () => {
    const m = await enroll("mine", { channel: "canary" });
    const row = await env.DB.prepare(`SELECT channel FROM machines WHERE machine_id = ?`).bind(m.machine_id).first<any>();
    expect(row.channel).toBe("canary");
  });
});

describe("lease request validation", () => {
  it("needs the agent header, caps and a digest; asks for the profile when the digest is new", async () => {
    const m = await enroll("pc", { digest: null });
    err(await ask(m), 409, "conflict").need_profile === true || expect.fail();
    expect((await ask(m)).body.need_profile).toBe(true);
    const b = ok(await ask(m, { profile: PROFILE }));
    expect(b).toEqual({ wait_s: 300, reason: "no_work" });
    // Known digest now: no profile needed.
    ok(await ask(m));
    // (Spaced out: a machine may ask for a lease only every 5 s after a burst of 6.)
    advance(60);
    err(await call("POST", "/v1/lease", { token: m.token, agent: null, body: { profile_digest: DIGEST, caps: {}, kinds: [] } }), 400, "bad_request");
    err(await call("POST", "/v1/lease", { token: m.token, body: { profile_digest: DIGEST, kinds: ["fuzz"] } }), 400, "bad_request");
    err(await call("POST", "/v1/lease", { token: m.token, body: { profile_digest: "x", caps: {}, kinds: [] } }), 400, "bad_request");
    err(
      await call("POST", "/v1/lease", { token: m.token, body: { profile_digest: DIGEST, caps: { threads: 0, ram_mb: 1, gpu: false, disk_mb: 1 }, kinds: [] } }),
      400,
      "bad_request",
    );
    err(await call("POST", "/v1/lease", { token: m.token, agent: "latest", body: {} }), 400, "bad_request");
  });

  it("answers paused_by_admin when the farm or the machine is paused", async () => {
    await makeCampaign("fz");
    const m = await enroll("pc");
    ok(await admin("POST", "/v1/admin/pause", { paused: true }));
    expect(ok(await ask(m))).toEqual({ wait_s: 300, reason: "paused_by_admin" });
    ok(await admin("POST", "/v1/admin/pause", { paused: false }));
    ok(await admin("POST", `/v1/admin/machine/${m.machine_id}`, { paused: true }));
    expect(ok(await ask(m)).reason).toBe("paused_by_admin");
    ok(await admin("POST", `/v1/admin/machine/${m.machine_id}`, { paused: false }));
    expect(ok(await ask(m)).lease.kind).toBe("fuzz");
    err(await admin("POST", `/v1/admin/machine/m_00000000000000000000000000`, { paused: true }), 404, "not_found");
    err(await admin("POST", "/v1/admin/pause", {}), 400, "bad_request");
  });

  it("ignores kinds it doesn't know", async () => {
    await makeCampaign("fz");
    const m = await enroll("pc");
    expect(ok(await ask(m, { kinds: ["records", "fuzz"] })).lease.kind).toBe("fuzz");
    expect(ok(await ask(m, { kinds: ["records"] })).reason).toBe("no_work");
  });
});

describe("artifacts", () => {
  it("serves builds/ and ckpt/ to machines with range requests, nothing else", async () => {
    const m = await enroll("pc");
    await putArtifact("builds/abc/file.bin", "0123456789");
    await putArtifact("ckpt/r/1/model.onnx", "model");
    const whole = await call("GET", "/v1/artifact/builds/abc/file.bin", { token: m.token });
    expect(whole.status).toBe(200);
    expect(whole.body).toBe("0123456789");
    const part = await call("GET", "/v1/artifact/builds/abc/file.bin", { token: m.token, headers: { range: "bytes=2-5" } });
    expect(part.status).toBe(206);
    expect(part.body).toBe("2345");
    expect(part.headers.get("content-range")).toBe("bytes 2-5/10");
    const tail = await call("GET", "/v1/artifact/builds/abc/file.bin", { token: m.token, headers: { range: "bytes=-3" } });
    expect(tail.body).toBe("789");
    expect(tail.headers.get("content-range")).toBe("bytes 7-9/10");
    expect((await call("HEAD", "/v1/artifact/ckpt/r/1/model.onnx", { token: m.token })).headers.get("content-length")).toBe("5");
    err(await call("GET", "/v1/artifact/builds/abc/missing", { token: m.token }), 404, "not_found");
    // Results and diagnostics are for the desktop only.
    await env.BUCKET.put("results/r/l_x/0", "secret");
    err(await call("GET", "/v1/artifact/results/r/l_x/0", { token: m.token }), 404, "not_found");
    expect((await admin("GET", "/v1/admin/artifact/results/r/l_x/0")).body).toBe("secret");
    err(await call("GET", "/v1/artifact/builds/../results/r/l_x/0", { token: m.token }), 404, "not_found");
    err(await call("GET", "/v1/artifact/builds/a%2F..%2F..%2Fresults/x", { token: m.token }), 400, "bad_request");
  });

  it("admin uploads: key rules, sha256 check, multipart", async () => {
    err(await admin("PUT", "/v1/admin/artifact/results/x", undefined, { raw: "x" }), 400, "bad_request");
    // (A literal "/../" is resolved by URL parsing before the Worker sees it; an encoded one isn't.)
    err(await admin("PUT", "/v1/admin/artifact/builds/a%2F..%2Fb", undefined, { raw: "x" }), 400, "bad_request");
    err(await admin("PUT", "/v1/admin/artifact/builds/a%2F%2Fb", undefined, { raw: "x" }), 400, "bad_request");
    err(await admin("PUT", "/v1/admin/artifact/builds/a/b", undefined, { raw: "x", headers: { "x-sha256": "0".repeat(64) } }), 400, "bad_request");
    err(await admin("PUT", "/v1/admin/artifact/builds/a/b", undefined, { raw: "x", headers: { "x-sha256": "nope" } }), 400, "bad_request");
    await putArtifact("builds/a/b", "x");

    const part1 = new Uint8Array(5 * 1024 * 1024).fill(1);
    const part2 = new TextEncoder().encode("tail");
    const start = ok(await admin("POST", "/v1/admin/artifact/builds/big/runtime.zip?uploads"));
    const p1 = ok(await admin("PUT", `/v1/admin/artifact/builds/big/runtime.zip?upload_id=${encodeURIComponent(start.upload_id)}&part=1`, undefined, { raw: part1 }));
    const p2 = ok(await admin("PUT", `/v1/admin/artifact/builds/big/runtime.zip?upload_id=${encodeURIComponent(start.upload_id)}&part=2`, undefined, { raw: part2 }));
    const done = ok(
      await admin("POST", `/v1/admin/artifact/builds/big/runtime.zip?upload_id=${encodeURIComponent(start.upload_id)}`, {
        parts: [p1, p2],
      }),
    );
    expect(done.size).toBe(part1.length + part2.length);
    err(await admin("POST", "/v1/admin/artifact/builds/big/x", {}), 400, "bad_request");
  });
});

describe("builds and agent releases", () => {
  it("registers a build only when manifest, files and signature are all there and match", async () => {
    const file = "worker!";
    let n = 0; // every case gets its own manifest, so its own build id
    const manifest = (files: unknown) =>
      JSON.stringify({ manifest: 1, kind: "worker", version: `w${n++}`, game: "mtg", aw: 1, entry: "w.exe", files });
    const good = [{ path: "w.exe", sha256: await sha256(file), size: file.length }];
    const setup = async (text: string, upload: Record<string, string>) => {
      const id = (await sha256(text)).slice(0, 24);
      await putArtifact(`builds/${id}/manifest.json`, text);
      for (const [k, v] of Object.entries(upload)) await putArtifact(`builds/${id}/${k}`, v);
      return id;
    };
    const sig = "untrusted comment: x\nAAAA\ntrusted comment: x\nAAAA\n";

    err(await admin("POST", "/v1/admin/build", { build_id: "f".repeat(24) }), 400, "bad_request"); // nothing uploaded
    err(await admin("POST", "/v1/admin/build", { build_id: "xyz" }), 400, "bad_request");
    let id = await setup(manifest(good), { "w.exe": file }); // no signature
    expect(err(await admin("POST", "/v1/admin/build", { build_id: id }), 400, "bad_request").problems.join()).toMatch(/minisig/);
    id = await setup(manifest(good), { "manifest.json.minisig": sig }); // file missing
    expect(err(await admin("POST", "/v1/admin/build", { build_id: id }), 400, "bad_request").problems.join()).toMatch(/not uploaded/);
    id = await setup(manifest([{ ...good[0], size: 3 }]), { "w.exe": file, "manifest.json.minisig": sig });
    expect(err(await admin("POST", "/v1/admin/build", { build_id: id }), 400, "bad_request").problems.join()).toMatch(/bytes/);
    id = await setup(manifest([{ ...good[0], sha256: "0".repeat(64) }]), { "w.exe": file, "manifest.json.minisig": sig });
    expect(err(await admin("POST", "/v1/admin/build", { build_id: id }), 400, "bad_request").problems.join()).toMatch(/sha256/);
    id = await setup(manifest([{ path: "../evil", sha256: good[0].sha256, size: 1 }]), {});
    err(await admin("POST", "/v1/admin/build", { build_id: id }), 400, "bad_request");
    // A manifest stored under the wrong id.
    await putArtifact(`builds/${"e".repeat(24)}/manifest.json`, manifest(good));
    err(await admin("POST", "/v1/admin/build", { build_id: "e".repeat(24) }), 400, "bad_request");

    id = await setup(manifest(good), { "w.exe": file, "manifest.json.minisig": sig });
    const b = ok(await admin("POST", "/v1/admin/build", { build_id: id }));
    expect(b).toMatchObject({ ok: true, build_id: id, kind: "worker", sig_verified: false });
  });

  it("releases agents per channel; /v1/agent/latest follows the machine's channel", async () => {
    const m = await enroll("pc");
    err(await call("GET", "/v1/agent/latest", { token: m.token }), 404, "not_found");
    const worker = await makeBuild();
    const a1 = await makeBuild({ kind: "agent", version: "0.3.1" });
    const a2 = await makeBuild({ kind: "agent", version: "0.4.0" });
    err(await admin("POST", "/v1/admin/agent/release", { version: "0.3.1", build_id: worker, channel: "stable", min_agent: "0.3.0" }), 400, "bad_request");
    err(await admin("POST", "/v1/admin/agent/release", { version: "0.4.0", build_id: a1, channel: "stable", min_agent: "0.3.0" }), 400, "bad_request");
    err(await admin("POST", "/v1/admin/agent/release", { version: "0.3.1", build_id: a1, channel: "beta", min_agent: "0.3.0" }), 400, "bad_request");
    err(await admin("POST", "/v1/admin/agent/release", { version: "0.3.1", build_id: a1, channel: "stable", min_agent: "0.9.0" }), 400, "bad_request");
    ok(await admin("POST", "/v1/admin/agent/release", { version: "0.3.1", build_id: a1, channel: "stable", min_agent: "0.3.0" }));
    ok(await admin("POST", "/v1/admin/agent/release", { version: "0.4.0", build_id: a2, channel: "canary", min_agent: "0.3.0" }));
    expect(ok(await call("GET", "/v1/agent/latest", { token: m.token }))).toEqual({
      version: "0.3.1",
      build_id: a1,
      manifest: `builds/${a1}/manifest.json`,
      min_agent: "0.3.0",
    });
    expect(ok(await call("GET", "/v1/agent/latest?channel=canary", { token: m.token })).version).toBe("0.4.0");
    err(await call("GET", "/v1/agent/latest?channel=nightly", { token: m.token }), 400, "bad_request");
  });
});

describe("runs, checkpoints, campaigns", () => {
  it("validates run configs", async () => {
    const w = await makeBuild();
    const rt = await makeBuild({ kind: "runtime" });
    err(await admin("PUT", "/v1/admin/run/Bad_Name", runBody(w)), 400, "bad_request");
    err(await admin("PUT", "/v1/admin/run/r", runBody("f".repeat(24))), 400, "bad_request"); // unknown build
    err(await admin("PUT", "/v1/admin/run/r", runBody(rt)), 400, "bad_request"); // not a worker
    err(await admin("PUT", "/v1/admin/run/r", { ...runBody(w), runtime: w }), 400, "bad_request"); // not a runtime
    err(await admin("PUT", "/v1/admin/run/r", { ...runBody(w), selfplay: { quota: -1 } }), 400, "bad_request");
    err(await admin("PUT", "/v1/admin/run/r", { ...runBody(w), selfplay: { quota: 5, gpu: "maybe" } }), 400, "bad_request");
    err(await admin("PUT", "/v1/admin/run/r", { ...runBody(w), eval: { batch: "x", games: 0 } }), 400, "bad_request");
    err(await admin("PUT", "/v1/admin/run/r", { ...runBody(w), state: "gone" }), 400, "bad_request");
    err(await admin("GET", "/v1/admin/run/r"), 404, "not_found");
    expect(ok(await admin("PUT", "/v1/admin/run/r", { ...runBody(w), runtime: rt })).created).toBe(true);
    expect(ok(await admin("PUT", "/v1/admin/run/r", { ...runBody(w), runtime: rt })).created).toBe(false);
    const got = ok(await admin("GET", "/v1/admin/run/r"));
    expect(got).toMatchObject({ run: "r", runtime: rt, current_gen: null, next_gid: 0, selfplay: { quota: 100, slack: 0.1, max_stale: 1 } });
  });

  it("publishes checkpoints: files must exist and match; an older generation is registered, never made current", async () => {
    await makeRun("r", { gen: null });
    await putArtifact("ckpt/r/1/model.onnx", "m1");
    const sha = await sha256("m1");
    err(await admin("PUT", "/v1/admin/run/r/checkpoint/1", { model: "ckpt/r/1/missing.onnx", sha256: sha }), 400, "bad_request");
    err(await admin("PUT", "/v1/admin/run/r/checkpoint/1", { model: "ckpt/other/1/model.onnx", sha256: sha }), 400, "bad_request");
    err(await admin("PUT", "/v1/admin/run/r/checkpoint/1", { model: "ckpt/r/1/model.onnx", sha256: "0".repeat(64) }), 400, "bad_request");
    err(await admin("PUT", "/v1/admin/run/r/checkpoint/1", { model: "ckpt/r/1/model.onnx", sha256: sha, reference: "ckpt/r/1/ref.npz" }), 400, "bad_request");
    err(await admin("PUT", "/v1/admin/run/nope/checkpoint/1", { model: "ckpt/nope/1/model.onnx", sha256: sha }), 400, "bad_request");
    expect(ok(await admin("PUT", "/v1/admin/run/r/checkpoint/1", { model: "ckpt/r/1/model.onnx", sha256: sha })).unchanged).toBe(false);
    expect(ok(await admin("PUT", "/v1/admin/run/r/checkpoint/1", { model: "ckpt/r/1/model.onnx", sha256: sha })).unchanged).toBe(true);
    await putArtifact("ckpt/r/1/model2.onnx", "m2");
    err(await admin("PUT", "/v1/admin/run/r/checkpoint/1", { model: "ckpt/r/1/model2.onnx", sha256: await sha256("m2") }), 409, "conflict");
    await publishCkpt("r", 3);
    // Rev 5: an older gen (an eval opponent, a head-to-head label) is registered but not current.
    expect(ok(await admin("PUT", "/v1/admin/run/r/checkpoint/2", { model: "ckpt/r/1/model2.onnx", sha256: await sha256("m2") }))).toEqual({
      ok: true,
      unchanged: false,
      current: false,
    });
    expect(ok(await admin("PUT", "/v1/admin/run/r/checkpoint/2", { model: "ckpt/r/1/model2.onnx", sha256: await sha256("m2") })).unchanged).toBe(true);
    err(await admin("PUT", "/v1/admin/run/r/checkpoint/2", { model: "ckpt/r/1/model.onnx", sha256: sha }), 409, "conflict");
    const r = ok(await admin("GET", "/v1/admin/run/r"));
    expect(r.current_gen).toBe(3);
    expect(r.checkpoints).toEqual([1, 2, 3]);
    // The older gen started no quota count of its own; gen 3's is still the current one.
    const q = (await env.DB.prepare(`SELECT gen FROM quotas WHERE run = 'r' ORDER BY gen`).all<any>()).results.map((x: any) => x.gen);
    expect(q).toEqual([1, 3]);
  });

  it("results, accept and ack need a known run; inputs are checked", async () => {
    err(await admin("GET", "/v1/admin/run/nope/results"), 404, "not_found");
    err(await admin("POST", "/v1/admin/run/nope/accept", { gen: 1, games: 1 }), 404, "not_found");
    err(await admin("POST", "/v1/admin/run/nope/ack", { cursor: 1 }), 404, "not_found");
    await makeRun("r");
    err(await admin("POST", "/v1/admin/run/r/accept", { gen: 1 }), 400, "bad_request");
    err(await admin("POST", "/v1/admin/run/r/ack", { cursor: -1 }), 400, "bad_request");
    err(await admin("GET", "/v1/admin/run/r/results?after=x"), 400, "bad_request");
    expect(ok(await admin("GET", "/v1/admin/run/r/results"))).toEqual({ results: [], cursor: 0 });
    expect(ok(await admin("POST", "/v1/admin/run/r/accept", { gen: 1, games: 5 })).selfplay_state.accepted).toBe(5);
    expect(ok(await admin("POST", "/v1/admin/run/r/ack", { cursor: 10 })).acked).toBe(0);
  });

  it("validates campaigns", async () => {
    const w = await makeBuild();
    const base = { build_id: w, mode: "delver", env: { FUZZ_POOL: "delver" }, seed_base: 10_000_000 };
    err(await admin("PUT", "/v1/admin/campaign/c", { ...base, seed_base: -1 }), 400, "bad_request");
    err(await admin("PUT", "/v1/admin/campaign/c", { ...base, env: { "bad name": "x" } }), 400, "bad_request");
    err(await admin("PUT", "/v1/admin/campaign/c", { ...base, env: { X: 1 } }), 400, "bad_request");
    err(await admin("PUT", "/v1/admin/campaign/c", { ...base, chunk: 0 }), 400, "bad_request");
    err(await admin("PUT", "/v1/admin/campaign/c", { ...base, mode: "" }), 400, "bad_request");
    err(await admin("GET", "/v1/admin/campaign/c"), 404, "not_found");
    err(await admin("GET", "/v1/admin/campaign/c/failures"), 404, "not_found");
    err(await admin("GET", "/v1/admin/campaign/c/results"), 404, "not_found");
    expect(ok(await admin("PUT", "/v1/admin/campaign/c", base))).toMatchObject({ created: true, cursor: 10_000_000 });
    expect(ok(await admin("GET", "/v1/admin/campaign/c"))).toMatchObject({ campaign: "c", chunk: "auto", priority: 0.2, seeds_done: 0 });
  });
});

describe("machines, crashes, diagnostics, stats", () => {
  it("lists, renames and revokes machines; revoking ends their leases", async () => {
    await makeCampaign("fz");
    const m = await enroll("pc");
    const l = await lease(m);
    ok(await admin("POST", `/v1/admin/machine/${m.machine_id}`, { name: "Sam's PC", channel: "canary" }));
    const list = ok(await admin("GET", "/v1/admin/machines")).machines;
    expect(list[0]).toMatchObject({ machine_id: m.machine_id, name: "Sam's PC", channel: "canary", active_leases: 1, invite_note: "pc" });
    expect(list[0].profile.cpu).toBe(PROFILE.cpu);
    err(await admin("POST", `/v1/admin/machine/m_00000000000000000000000000/revoke`, {}), 404, "not_found");
    err(await admin("POST", `/v1/admin/machine/bogus/revoke`, {}), 404, "not_found");
    expect(ok(await admin("POST", `/v1/admin/machine/${m.machine_id}/revoke`, {})).cancelled).toBe(1);
    expect(ok(await admin("POST", `/v1/admin/machine/${m.machine_id}/revoke`, {})).cancelled).toBe(0);
    const row = await env.DB.prepare(`SELECT state FROM leases WHERE lease_id = ?`).bind(l.lease_id).first<any>();
    expect(row.state).toBe("cancelled");
    // Its seeds went back to the pool and the next machine gets them.
    const m2 = await enroll("pc2");
    expect((await lease(m2)).job.first).toBe(l.job.first);
  });

  it("records crash reports", async () => {
    const m = await enroll("pc");
    ok(await call("POST", "/v1/crash", { token: m.token, body: { build_id: "a".repeat(24), kind: "panic", message: "x".repeat(10000), backtrace: "bt" } }));
    err(await call("POST", "/v1/crash", { token: m.token, body: { kind: "oops" } }), 400, "bad_request");
    err(await call("POST", "/v1/crash", { token: m.token, body: { kind: "crash", build_id: "short" } }), 400, "bad_request");
    const row = await env.DB.prepare(`SELECT * FROM crashes`).first<any>();
    expect(row).toMatchObject({ machine_id: m.machine_id, kind: "panic", agent_version: AGENT });
    expect(row.message.length).toBe(4096);
  });

  it("asks for logs through beats and lease answers; refuses unasked uploads", async () => {
    await makeCampaign("fz");
    const m = await enroll("pc");
    err(await call("POST", "/v1/diag/logs", { token: m.token, raw: "log" }), 409, "conflict");
    const l = await lease(m);
    err(await admin("POST", "/v1/admin/diag", { kind: "logs" }), 400, "bad_request");
    err(await admin("POST", "/v1/admin/diag", { machine_id: m.machine_id, kind: "dump" }), 400, "bad_request");
    err(await admin("POST", "/v1/admin/diag", { class: "nobody", kind: "logs" }), 404, "not_found");
    const { requests } = ok(await admin("POST", "/v1/admin/diag", { machine_id: m.machine_id, kind: "logs" }));
    expect(requests).toHaveLength(1);
    const b = ok(await beat(m, l.lease_id));
    expect(b.upload_diag).toEqual(["logs"]);
    expect(ok(await beat(m, l.lease_id)).upload_diag).toBeUndefined(); // asked once (re-asked after 30 min)
    err(await call("POST", "/v1/diag/BAD NAME", { token: m.token, raw: "x" }), 400, "bad_request");
    const up = ok(await call("POST", "/v1/diag/logs", { token: m.token, raw: "the log" }));
    expect((await admin("GET", `/v1/admin/artifact/${up.key}`)).body).toBe("the log");
    err(await call("POST", "/v1/diag/logs", { token: m.token, raw: "again" }), 409, "conflict");
    const list = ok(await admin("GET", `/v1/admin/diag?machine_id=${m.machine_id}`));
    expect(list.requests[0].done_at).not.toBeNull();
    expect(list.uploads[0].key).toBe(up.key);

    // A machine with no lease hears about it in its next /v1/lease answer.
    const m2 = await enroll("pc2");
    ok(await admin("POST", "/v1/admin/diag", { class: "AMD Ryzen 7 7800X3D / RTX 4070", kind: "logs" }));
    expect(ok(await ask(m2, { kinds: [] })).upload_diag).toEqual(["logs"]);
  });

  it("runs diag jobs as leases, first, and closes the request when done (agents before definitions)", async () => {
    const build = await makeRun("r", { define: false });
    const m = await enroll("pc");
    const { requests } = ok(await admin("POST", "/v1/admin/diag", { machine_id: m.machine_id, kind: "sweep", params: { threads: [2, 4] } }));
    // Rev 6: a definition-aware agent never gets a diag lease (it carries no definition).
    expect(ok(await ask(m)).reason).toBe("no_work");
    const l = await lease(m, { agent: LEGACY });
    expect(l).toMatchObject({ kind: "diag", build: { build_id: build }, job: { kind: "diag", what: "sweep", params: { threads: [2, 4] } } });
    expect(l.checkpoint.gen).toBe(1);
    // Without "diag" in kinds it gets ordinary work.
    const m2 = await enroll("pc2");
    ok(await admin("POST", "/v1/admin/diag", { machine_id: m2.machine_id, kind: "profile" }));
    expect((await lease(m2, { kinds: ["selfplay"], agent: LEGACY })).kind).toBe("selfplay");
    err(await upload(m, l.lease_id, 0, "x", { ids: [[0, 1]] }), 400, "bad_request");
    const res = ok(await upload(m, l.lease_id, 0, "sweep results"));
    expect(res.credited).toBe(0);
    ok(await finish(m, l.lease_id, [0]));
    const list = ok(await admin("GET", `/v1/admin/diag?machine_id=${m.machine_id}`));
    expect(list.requests.find((r: any) => r.id === requests[0].id).done_at).not.toBeNull();
    expect(list.results[0].key).toMatch(new RegExp(`^diag/${m.machine_id}/`));
    // Done: the next lease is ordinary work.
    expect((await lease(m, { agent: LEGACY })).kind).toBe("selfplay");
  });

  it("stats aggregates machines, runs, campaigns and failures", async () => {
    await makeRun("r");
    await makeCampaign("fz");
    const m = await enroll("pc");
    const l = await lease(m);
    ok(await beat(m, l.lease_id, "running", { stats: { games: 3, yields: 2, yield_ms_max: 640, cpu_pct: 500 } }));
    ok(await call("POST", "/v1/crash", { token: m.token, body: { kind: "crash", message: "boom" } }));
    const s = ok(await admin("GET", "/v1/admin/stats"));
    expect(s.paused).toBe(false);
    expect(s.machines[0]).toMatchObject({ machine_id: m.machine_id, active_leases: 1, yields_24h: 2, crashes_24h: 1, working_on: "selfplay:r" });
    expect(s.runs[0]).toMatchObject({ run: "r", current_gen: 1, active_leases: 1, selfplay: { quota: 100, cap: 110, leased: 10 } });
    expect(s.campaigns[0]).toMatchObject({ campaign: "fz", cursor: 10_000_000 });
    expect(s.failures.last_day.crashes).toBe(1);
    expect(s.failures.crashes[0].message).toBe("boom");
    expect(s.machines[0]).toMatchObject({ starved_s_24h: 0, idle_h_day: null, on_h_day: null });
    expect(s.releases).toEqual([]);
    expect(s.diag_uploads).toEqual([]);
  });

  it("stats show availability (profile idle_h_day / on_h_day), starvation, releases and diagnostics uploads", async () => {
    await makeCampaign("fz");
    const m = await enroll("pc");
    const digest = "ef".repeat(32);
    err(await ask(m, { digest, profile: { ...PROFILE, idle_h_day: 25 } }), 400, "bad_request");
    const l = await lease(m, { digest, profile: { ...PROFILE, idle_h_day: 9.5, on_h_day: 14.2 } });
    ok(await beat(m, l.lease_id, "running", { stats: { starved_ms: 5400_000, paused_ms: 1000 } }));
    ok(await call("POST", "/v1/diag/logs", { token: m.token, raw: "logs", headers: { "x-farm-diag": "user" } }));
    const id = await makeBuild({ kind: "agent", version: "0.2.0" });
    ok(await admin("POST", "/v1/admin/agent/release", { version: "0.2.0", build_id: id, channel: "canary", min_agent: "0.1.0" }));
    const s = ok(await admin("GET", "/v1/admin/stats"));
    expect(s.machines[0]).toMatchObject({ idle_h_day: 9.5, on_h_day: 14.2, starved_s_24h: 5400 });
    expect(s.releases).toEqual([expect.objectContaining({ channel: "canary", version: "0.2.0", min_agent: "0.1.0", github_url: null })]);
    expect(s.diag_uploads).toEqual([expect.objectContaining({ machine_id: m.machine_id, name: "pc", what: "logs", user: true, size: 4 })]);
    const page = (await call("GET", "/admin")).body as string;
    for (const col of ["idle h/day", "starved h", "Agent releases", "Diagnostics uploads"]) expect(page).toContain(col);
  });
});
