// node --test tools/agent-tests.test.mjs: the parts of tools/agent-tests.mjs that decide what runs and what a pass is.
import assert from "node:assert/strict";
import { test } from "node:test";
import { order, parseArgs, parseResult, testBinaries } from "./agent-tests.mjs";

test("the options", () => {
  assert.deepEqual(parseArgs([]), { jobs: 4, threads: 2, only: null, extra: [] });
  assert.deepEqual(parseArgs(["--jobs", "6", "--test-threads", "1", "--only", "crew", "--", "--nocapture", "x"]),
                   { jobs: 6, threads: 1, only: "crew", extra: ["--nocapture", "x"] });
  assert.equal(parseArgs(["--jobs", "0"]).jobs, 1);
  assert.throws(() => parseArgs(["--jobs"]));
  assert.throws(() => parseArgs(["--jobs", "many"]));
  assert.throws(() => parseArgs(["--job", "2"]));
});

test("the test binaries are cargo's test artifacts with an executable", () => {
  const art = (name, kind, test, executable, pkg = "agent") => JSON.stringify({
    reason: "compiler-artifact", manifest_path: `/w/${pkg}/Cargo.toml`, target: { name, kind: [kind] },
    profile: { test }, executable });
  const lines = [
    art("crew", "test", true, "/t/debug/deps/crew-1.exe"),
    art("farm_agent", "lib", true, "/t/debug/deps/farm_agent-2.exe"),
    art("farm-agent", "bin", false, "/t/debug/farm-agent.exe"),          // a built binary, not a test
    art("serde", "lib", false, null, "serde"),                            // a dependency
    art("smoke", "test", true, "/t/debug/deps/smoke-3.exe", "fakeworker"),
    '{"reason":"build-finished","success":true}', "warning: not json", "",
  ].join("\n");
  const bins = testBinaries(lines);
  assert.deepEqual(bins.map((b) => b.name), ["agent/crew", "agent/farm_agent (lib unit tests)", "fakeworker/smoke"]);
  assert.equal(bins[2].pkgDir.replaceAll("\\", "/"), "/w/fakeworker");
  assert.equal(bins[0].exe, "/t/debug/deps/crew-1.exe");
});

test("a pass is a 'test result: ok.' line with no failure", () => {
  const ok = "running 2 tests\ntest a ... ok\ntest b ... ok\n\ntest result: ok. 2 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.42s\n";
  assert.deepEqual(parseResult(ok), { ok: true, passed: 2, failed: 0, ignored: 1, lines: 1 });
  const bad = "test result: FAILED. 4 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s\n";
  assert.equal(parseResult(bad).ok, false);
  assert.equal(parseResult(bad).failed, 1);
  assert.equal(parseResult("thread 'main' panicked\n").ok, false);       // no result line: the binary died
  assert.equal(parseResult("").ok, false);
  assert.equal(parseResult(ok + bad).ok, false);                           // every result line must be ok
  assert.equal(parseResult("test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s").ok, true);
});

test("longest first, an unmeasured binary first of all", () => {
  const bins = ["b", "a", "new", "c"].map((name) => ({ name }));
  assert.deepEqual(order(bins, { a: 5, b: 70.5, c: 5 }).map((b) => b.name), ["new", "b", "a", "c"]);
});
