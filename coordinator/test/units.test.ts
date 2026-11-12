// Pure pieces: id ranges, semver, lease sizing, BLAKE2b, minisign, telemetry adapter.

import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { leaseSize, SIZING, TARGET_S } from "../src/friend";
import { blake2b512, verifyBuild, verifyMinisign } from "../src/manifest";
import { intersect, normalize, parseRanges, subtract, total, type Range } from "../src/ranges";
import { cleanStats, DOUBLES, telemetry } from "../src/telemetry";
import { cmpSemver, hex, HttpError, maxSemver } from "../src/util";
import { putArtifact, sha256 } from "./helpers";

describe("ranges", () => {
  it("normalizes, subtracts and intersects half-open ranges", () => {
    expect(normalize([[5, 7], [1, 3], [3, 4], [6, 9], [10, 10]])).toEqual([[1, 4], [5, 9]]);
    expect(subtract([[0, 10]], [[2, 3], [5, 7]])).toEqual([[0, 2], [3, 5], [7, 10]]);
    expect(subtract([[0, 10], [20, 30]], [[5, 25]])).toEqual([[0, 5], [25, 30]]);
    expect(subtract([[0, 10]], [[0, 10]])).toEqual([]);
    expect(subtract([[0, 10]], [])).toEqual([[0, 10]]);
    expect(intersect([[0, 10], [20, 30]], [[5, 25]])).toEqual([[5, 10], [20, 25]]);
    expect(total([[0, 10], [20, 25]])).toBe(15);
  });

  it("subtract and intersect partition the set (randomized)", () => {
    let seed = 7;
    const rnd = (n: number) => ((seed = (seed * 1103515245 + 12345) % 2 ** 31), seed % n);
    for (let t = 0; t < 200; t++) {
      const mk = () => normalize(Array.from({ length: rnd(6) }, () => { const a = rnd(100); return [a, a + 1 + rnd(20)] as Range; }));
      const a = mk();
      const b = mk();
      const s = subtract(a, b);
      const i = intersect(a, b);
      expect(total(s) + total(i)).toBe(total(a));
      expect(total(intersect(s, i))).toBe(0);
      expect(normalize([...s, ...i])).toEqual(a);
    }
  });

  it("parses caller ranges strictly", () => {
    expect(parseRanges([[3, 5], [0, 2]], "x")).toEqual([[0, 2], [3, 5]]);
    expect(() => parseRanges([[0, 5], [4, 6]], "x")).toThrow(HttpError); // overlap
    expect(() => parseRanges([[5, 5]], "x")).toThrow(HttpError); // empty
    expect(() => parseRanges([[1.5, 3]], "x")).toThrow(HttpError);
    expect(() => parseRanges([[0, 11]], "x", [0, 10])).toThrow(HttpError); // outside the lease
    expect(() => parseRanges("nope", "x")).toThrow(HttpError);
  });
});

describe("semver", () => {
  it("orders versions, pre-releases before releases", () => {
    expect(cmpSemver("0.3.1", "0.3.0")).toBe(1);
    expect(cmpSemver("0.3.1", "0.10.0")).toBe(-1);
    expect(cmpSemver("1.0.0", "1.0.0")).toBe(0);
    expect(cmpSemver("0.4.0-rc1", "0.4.0")).toBe(-1);
    expect(maxSemver([null, "0.2.0", "0.10.1", undefined, "0.9.9"])).toBe("0.10.1");
  });
});

describe("lease sizing", () => {
  it("uses the configured size, else ~TARGET_S of measured rate, else the default", () => {
    expect(leaseSize("selfplay", 40, 1)).toBe(40);
    expect(leaseSize("selfplay", "auto", null)).toBe(SIZING.selfplay.def);
    expect(leaseSize("selfplay", "auto", 0.1)).toBe(Math.round(0.1 * TARGET_S));
    expect(leaseSize("fuzz", "auto", 50)).toBe(50 * TARGET_S);
    expect(leaseSize("fuzz", "auto", 1e9)).toBe(SIZING.fuzz.max);
    expect(leaseSize("eval", "auto", 1e-6)).toBe(SIZING.eval.min);
  });
});

describe("blake2b-512", () => {
  const vec = (n: number) => Uint8Array.from({ length: n }, (_, i) => i % 256);
  it("matches reference vectors (RFC 7693 'abc', empty, one exact block, multi-block)", () => {
    expect(hex(blake2b512(new TextEncoder().encode("abc")))).toBe(
      "ba80a53f981c4d0d6a2797b69f12f6e94c212f14685ac4b74b12bb6fdbffa2d17d87c5392aab792dc252d5de4533cc9518d38aa8dbf1925ab92386edd4009923",
    );
    expect(hex(blake2b512(new Uint8Array()))).toBe(
      "786a02f742015903c6c6fd852552d272912f4740e15847618a86e217f71f5419d25e1031afee585313896444934eb04b903a685b1448b755d56f701afe9be2ce",
    );
    expect(hex(blake2b512(vec(128)))).toBe(
      "2319e3789c47e2daa5fe807f61bec2a1a6537fa03f19ff32e87eecbfd64b7e0e8ccff439ac333b040f19b0c4ddd11a61e24ac1fe0f10a039806c5dcc0da3d115",
    );
    expect(hex(blake2b512(vec(300)))).toBe(
      "d9cf5983dc6b34c0fa1f0226926855ad3eccd2bcdcd8f8053b9a80664d33b5afcc32fd21c70ea14f4ef50ca97c3203c4d1803159f0e01bb6cb1d1c83db52b63c",
    );
  });
});

// Builds a minisign key pair and signer the way `minisign -S` (prehashed, "ED") or `-l` ("Ed") does.
async function minisignKey() {
  const kp = (await crypto.subtle.generateKey({ name: "Ed25519" }, true, ["sign", "verify"])) as CryptoKeyPair;
  const pk = new Uint8Array((await crypto.subtle.exportKey("raw", kp.publicKey)) as ArrayBuffer);
  const keyId = crypto.getRandomValues(new Uint8Array(8));
  const b64 = (b: Uint8Array) => btoa(String.fromCharCode(...b));
  const pub = b64(new Uint8Array([0x45, 0x64, ...keyId, ...pk]));
  const sign = async (msg: Uint8Array, trusted: string, alg: "ED" | "Ed" = "ED") => {
    const data = alg === "ED" ? blake2b512(msg) : msg;
    const sig = new Uint8Array(await crypto.subtle.sign({ name: "Ed25519" }, kp.privateKey, data));
    const tc = new TextEncoder().encode(trusted);
    const global = new Uint8Array(await crypto.subtle.sign({ name: "Ed25519" }, kp.privateKey, new Uint8Array([...sig, ...tc])));
    return `untrusted comment: signature from minisign secret key\n${b64(new Uint8Array([alg.charCodeAt(0), alg.charCodeAt(1), ...keyId, ...sig]))}\ntrusted comment: ${trusted}\n${b64(global)}\n`;
  };
  return { pub, sign };
}

describe("minisign", () => {
  it("verifies prehashed and legacy signatures and rejects tampering", async () => {
    const k = await minisignKey();
    const other = await minisignKey();
    const msg = new TextEncoder().encode('{"manifest": 1}');
    for (const alg of ["ED", "Ed"] as const) {
      const sig = await k.sign(msg, "build_id=abc kind=worker", alg);
      expect(await verifyMinisign(msg, sig, [k.pub])).toBeNull();
      expect(await verifyMinisign(msg, sig, [other.pub, k.pub])).toBeNull();
      expect(await verifyMinisign(new TextEncoder().encode('{"manifest": 2}'), sig, [k.pub])).toMatch(/does not verify/);
      expect(await verifyMinisign(msg, sig.replace("kind=worker", "kind=agent"), [k.pub])).toMatch(/trusted comment/);
      expect(await verifyMinisign(msg, sig, [other.pub])).toMatch(/not in RELEASE_PUBKEYS/);
    }
    expect(await verifyMinisign(msg, "garbage", [k.pub])).toMatch(/not a minisign/);
  });

  it("verifyBuild checks the signature and its trusted comment when keys are configured", async () => {
    const k = await minisignKey();
    const file = "worker bytes";
    const text = JSON.stringify({
      manifest: 1, kind: "worker", version: "w1", game: "mtg", aw: 1, entry: "w.exe",
      files: [{ path: "w.exe", sha256: await sha256(file), size: file.length }],
    });
    const id = (await sha256(text)).slice(0, 24);
    await putArtifact(`builds/${id}/manifest.json`, text);
    await putArtifact(`builds/${id}/w.exe`, file);
    const bytes = new TextEncoder().encode(text);

    await putArtifact(`builds/${id}/manifest.json.minisig`, await k.sign(bytes, `build_id=${id} kind=worker`));
    const v = await verifyBuild(env.BUCKET, id, k.pub);
    expect(v.sig_verified).toBe(true);
    // Without keys: presence only.
    expect((await verifyBuild(env.BUCKET, id, "")).sig_verified).toBe(false);

    // Signed, but for another build id: refused.
    await putArtifact(`builds/${id}/manifest.json.minisig`, await k.sign(bytes, `build_id=000000000000000000000000 kind=worker`));
    await expect(verifyBuild(env.BUCKET, id, k.pub)).rejects.toThrow(/failed its checks/);
    // Signed by an unknown key: refused.
    await putArtifact(`builds/${id}/manifest.json.minisig`, await (await minisignKey()).sign(bytes, `build_id=${id} kind=worker`));
    await expect(verifyBuild(env.BUCKET, id, k.pub)).rejects.toThrow(/failed its checks/);
  });
});

describe("telemetry", () => {
  it("writes one AE data point per sample, and is a no-op without the binding", () => {
    const points: AnalyticsEngineDataPoint[] = [];
    const t = telemetry({ writeDataPoint: (p?: AnalyticsEngineDataPoint) => points.push(p!) } as AnalyticsEngineDataset);
    t.write({ machine_id: "m_x", class: "cpu / no gpu", lease_id: "l_y", kind: "selfplay", scope: "run:r", event: "beat", stats: { games: 3, provider: "cpu" } });
    expect(points).toHaveLength(1);
    expect(points[0].indexes).toEqual(["m_x"]);
    expect(points[0].doubles).toHaveLength(DOUBLES.length);
    expect(points[0].doubles![0]).toBe(3);
    expect(() => telemetry(undefined).write({ machine_id: "m", class: null, lease_id: "l", kind: "fuzz", scope: "s", event: "done", stats: {} })).not.toThrow();
    // A throwing binding never fails the caller.
    expect(() =>
      telemetry({ writeDataPoint: () => { throw new Error("x"); } } as unknown as AnalyticsEngineDataset).write({
        machine_id: "m", class: null, lease_id: "l", kind: "fuzz", scope: "s", event: "done", stats: {},
      }),
    ).not.toThrow();
    // The real (simulated) binding accepts writes.
    expect(() => telemetry(env.TELEMETRY).write({ machine_id: "m", class: null, lease_id: "l", kind: "fuzz", scope: "s", event: "beat", stats: {} })).not.toThrow();
  });

  it("keeps only the Stats fields the protocol defines", () => {
    expect(cleanStats({ games: 2, cpu_pct: 540, evil: "x".repeat(10000), provider: "cuda", throttle: ["thermal", 3] })).toEqual({
      games: 2,
      cpu_pct: 540,
      provider: "cuda",
      throttle: ["thermal"],
    });
    expect(cleanStats(null)).toBeUndefined();
    // starved_ms goes to AE too (the 20th and last double AE allows); paused_ms and
    // resume_ms_max are kept in D1 only.
    expect(DOUBLES.length).toBeLessThanOrEqual(20);
    expect(DOUBLES[DOUBLES.length - 1]).toBe("starved_ms");
    expect(cleanStats({ starved_ms: 1500, paused_ms: 20000, resume_ms_max: 80, starved: "lots" })).toEqual({
      starved_ms: 1500,
      paused_ms: 20000,
      resume_ms_max: 80,
    });
  });
});
