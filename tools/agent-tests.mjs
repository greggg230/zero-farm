// node tools/agent-tests.mjs [--jobs N] [--test-threads N] [--only TEXT] [-- libtest args]
//
// The agent workspace's tests with several test binaries at once. `cargo test --workspace --all-features --
// --test-threads=2` builds everything and then runs one test binary after another (two tests at a time inside each);
// most of the suite's time is integration tests that start real processes and wait on them, so the machine sits
// mostly idle. This builds the same binaries with the same command (`--no-run`), then runs them itself, --jobs at a
// time (default 4), longest first, each still with --test-threads=2 (default), from its package's directory as cargo
// does.
//
// The result is each binary's own "test result:" line: the run passes only when every binary exited 0 with
// "test result: ok." and at least one test passed overall. A failed binary's whole output is printed. The last line:
//   agent tests: PASSED - 31 binaries, 303 passed, 2 ignored in 95 s (4 at once; one after another: 410 s)
//   agent tests: FAILED - ... naming the failed binaries
// Exit 0 / 1. Each binary's seconds are kept in <target dir>/agent-tests-durations.json for the next run's order.
//
// --jobs 1 is cargo's own order of work (one binary at a time). --only TEXT runs the binaries whose name contains it.
import { spawn } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { availableParallelism } from "node:os";
import { basename, delimiter, dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const AGENT = resolve(dirname(fileURLToPath(import.meta.url)), "..", "agent");

export function parseArgs(argv) {
  const a = { jobs: 4, threads: 2, only: null, extra: [] };
  const k = argv.indexOf("--");
  if (k >= 0) { a.extra = argv.slice(k + 1); argv = argv.slice(0, k); }
  for (let i = 0; i < argv.length; i++) {
    const v = () => { if (i + 1 >= argv.length) throw new Error(`${argv[i]} needs a value`); return argv[++i]; };
    if (argv[i] === "--jobs") a.jobs = Math.max(1, parseInt(v(), 10));
    else if (argv[i] === "--test-threads") a.threads = Math.max(1, parseInt(v(), 10));
    else if (argv[i] === "--only") a.only = v();
    else throw new Error(`unknown option ${argv[i]}`);
  }
  if (!Number.isFinite(a.jobs) || !Number.isFinite(a.threads)) throw new Error("--jobs and --test-threads take a number");
  return a;
}

// cargo's JSON messages (one per line) -> the test binaries: [{name, pkgDir, exe}]
export function testBinaries(jsonLines) {
  const out = [];
  for (const line of jsonLines.split(/\r?\n/)) {
    if (!line.startsWith("{")) continue;
    let m;
    try { m = JSON.parse(line); } catch { continue; }
    if (m.reason !== "compiler-artifact" || !m.executable || !m.profile || !m.profile.test) continue;
    const kind = (m.target.kind || []).join(",");
    const pkg = basename(dirname(m.manifest_path));
    out.push({ name: `${pkg}/${m.target.name}${kind === "test" ? "" : ` (${kind} unit tests)`}`,
               pkgDir: dirname(m.manifest_path), exe: m.executable });
  }
  return out;
}

// a test binary's output -> {ok, passed, failed, ignored}: every "test result:" line (a binary prints one)
export function parseResult(output) {
  const r = { ok: false, passed: 0, failed: 0, ignored: 0, lines: 0 };
  const re = /^test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored;/gm;
  let m, allOk = true;
  while ((m = re.exec(output)) !== null) {
    r.lines++;
    allOk = allOk && m[1] === "ok";
    r.passed += +m[2]; r.failed += +m[3]; r.ignored += +m[4];
  }
  r.ok = r.lines > 0 && allOk && r.failed === 0;
  return r;
}

// longest first by the last run's seconds (an unmeasured binary first of all: it may be the longest)
export function order(bins, seconds) {
  return [...bins].sort((x, y) => (seconds[y.name] ?? Infinity) - (seconds[x.name] ?? Infinity) || x.name.localeCompare(y.name));
}

function run(cmd, args, opts) {
  return new Promise((done) => {
    const p = spawn(cmd, args, { ...opts, stdio: ["ignore", "pipe", opts.inheritErr ? "inherit" : "pipe"] });
    let out = "";
    p.stdout.on("data", (d) => { out += d; });
    if (!opts.inheritErr) p.stderr.on("data", (d) => { out += d; });
    p.on("error", (e) => done({ code: -1, out: out + String(e) }));
    p.on("close", (code) => done({ code, out }));
  });
}

async function main() {
  const a = parseArgs(process.argv.slice(2));
  const t0 = Date.now();
  // the same build as `cargo test --workspace --all-features` (so the same binaries, nothing compiled twice)
  const built = await run("cargo", ["test", "--workspace", "--all-features", "--no-run", "--message-format=json"],
                          { cwd: AGENT, inheritErr: true });
  if (built.code !== 0) { console.log(`agent tests: FAILED - the build failed (exit ${built.code})`); return 1; }
  let bins = testBinaries(built.out);
  if (a.only) bins = bins.filter((b) => b.name.includes(a.only));
  if (!bins.length) { console.log("agent tests: FAILED - no test binary" + (a.only ? ` matches --only ${a.only}` : "")); return 1; }
  const durFile = join(dirname(dirname(dirname(bins[0].exe))), "agent-tests-durations.json");
  let seconds = {};
  try { seconds = JSON.parse(readFileSync(durFile, "utf8")); } catch { /* the first run */ }
  const queue = order(bins, seconds);
  const jobs = Math.min(a.jobs, queue.length, Math.max(1, availableParallelism()));
  console.log(`agent tests: ${bins.length} test binaries, ${jobs} at once, --test-threads=${a.threads} each ` +
              `(built in ${((Date.now() - t0) / 1000).toFixed(0)} s)`);
  const t1 = Date.now();
  const results = [];
  const worker = async () => {
    for (;;) {
      const b = queue.shift();
      if (!b) return;
      const s = Date.now();
      // as cargo runs it: from the package's directory, CARGO_MANIFEST_DIR set, the deps dir on the library path
      const env = { ...process.env, CARGO_MANIFEST_DIR: b.pkgDir };
      const libVar = process.platform === "win32" ? (Object.keys(env).find((k) => k.toUpperCase() === "PATH") || "PATH")
        : process.platform === "darwin" ? "DYLD_FALLBACK_LIBRARY_PATH" : "LD_LIBRARY_PATH";
      env[libVar] = [dirname(b.exe), env[libVar]].filter(Boolean).join(delimiter);
      const r = await run(b.exe, [`--test-threads=${a.threads}`, ...a.extra], { cwd: b.pkgDir, env });
      const res = parseResult(r.out);
      const secs = (Date.now() - s) / 1000;
      const ok = r.code === 0 && res.ok;
      results.push({ b, ok, res, secs, code: r.code });
      console.log(`  ${ok ? "ok  " : "FAIL"}  ${b.name}: ${res.passed} passed, ${res.failed} failed, ${res.ignored} ignored ` +
                  `(${secs.toFixed(1)} s${r.code === 0 ? "" : `, exit ${r.code}`})`);
      if (!ok) console.log(`----- ${b.name}\n${r.out}\n-----`);
    }
  };
  await Promise.all(Array.from({ length: jobs }, worker));
  const wall = (Date.now() - t1) / 1000;
  const sum = results.reduce((x, r) => x + r.secs, 0);
  const tot = results.reduce((x, r) => ({ passed: x.passed + r.res.passed, ignored: x.ignored + r.res.ignored }),
                             { passed: 0, ignored: 0 });
  const bad = results.filter((r) => !r.ok).map((r) => r.b.name).sort();
  for (const r of results) if (r.ok) seconds[r.b.name] = Math.round(r.secs * 10) / 10;
  try { writeFileSync(durFile, JSON.stringify(seconds, null, 1)); } catch { /* best effort */ }
  const counts = `${results.length} binaries, ${tot.passed} passed, ${tot.ignored} ignored in ${wall.toFixed(0)} s ` +
                 `(${jobs} at once; one after another: ${sum.toFixed(0)} s)`;
  if (bad.length || tot.passed === 0) {
    console.log(`agent tests: FAILED - ${counts} - ${bad.length ? "failed: " + bad.join(", ") : "no test passed"}`);
    return 1;
  }
  console.log(`agent tests: PASSED - ${counts}`);
  return 0;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().then((code) => process.exit(code), (e) => { console.log(`agent tests: FAILED - ${e.message}`); process.exit(1); });
}
