//! fakeworker: a test worker that speaks aw/1 (PROTOCOL 5) with scripted behaviours.
//!
//! `fakeworker serve` sends `hello`, reads the `job` line and "plays" games by sleeping (no CPU
//! load). Behaviour comes from the job's `settings.fake` object (or `fake`; all fields optional):
//!
//! ```json
//! {"game_ms": 100, "shard_every": 2, "status_ms": 500,
//!  "yield_delay_ms": 0,        // >0: answer `yielded` late; <0: never answer
//!  "crash_after": null,        // games finished, then a panic message on stderr + exit 101
//!  "hang_after": null,         // games finished, then silence forever (ignores stdin)
//!  "fatal_after": null,        // games finished, then {"ev":"error","fatal":true} + exit 2
//!  "fail_seeds": [10450007],   // fuzz: seeds that "fail"
//!  "chunk": 1000,              // fuzz: seeds per step
//!  "selftest_fail_gen": null,  // eval: this network fails its self-test (then a fatal error)
//!  "mem_base_mb": 0,           // rolling: memory committed at the start, before any game (a real worker's runtime)
//!  "mem_per_game_mb": 0,       // rolling: memory committed per game in flight (the job's conc), at the start ...
//!  "mem_ramp_ms": 0,           // ... or, when > 0, in ten steps over this long (a real worker's games grow)
//!  "mem_grow_mb": 0,           // rolling: memory committed per finished game on top (a creep)
//!  "mem_grow_until": null,     // rolling: ... until this many games are finished (then it stays)
//!  "oom_abort": false,         // a failed commit: abort on stderr (Rust's message) instead of a fatal error
//!  "crash_gid": null,          // the worker that finishes this game id panics (exit 101): one member of a crew
//!  "build_ms": 0,              // its sessions take this long to build after the job line: meanwhile `status` says
//!                              // `provider: ""` (zero-core's default) and nothing is self-tested or played
//!  "provider": "cpu",          // the provider `status` and `selftest` report once the sessions are built
//!  "vram_base_mb": 0,          // agent 0.3.4: `status` reports `vram_mb` = this + `vram_per_row_mb` x the job's batch
//!  "vram_per_row_mb": 0,       //   rows (conc / groups, rounded up) once its sessions exist (0 while yielded)
//!  "vram_oom_rows": null,      // agent 0.3.4: batches of more rows than this fail at the start like a GPU out of
//!                              //   memory (ONNX Runtime's CUDA allocator's fatal error, exit 2)
//!  "rate": null}               // rolling: leaves per second by layout (below)
//! ```
//!
//! `rate` (rev 19, the agent's layout tuning): `{"leaves_s": 1000, "best": {"threads": 4, "infer_workers": 2,
//! "infer_threads": 2, "batch": 8, "conc": 32}, "penalty": 0.15, "decay_p": null}`. The job's layout (`threads`,
//! `infer_workers` - default 1 -, `infer_threads` - default `threads` -, `conc`, `groups`: batch = conc / groups) plays
//! `leaves_s` x the product over the keys `best` names of max(0.05, 1 - penalty x |log2(value / best)|) leaves per
//! second, scaled by the share of `conc` games in flight and, with `decay_p`, by decay_p / (decay_p + leaves per game in
//! flight) (a fresh worker's games grow); time suspended doesn't count. Workers of one lease share the machine (the
//! agent's crews, PROTOCOL 13.5): each one with a rate model beats a file in `.crew/` of its working directory (the
//! lease's) every 100 ms, and plays `crew[N - 1] / N` of its rate while N of them beat (`"crew": [1.0, 1.3, 1.25]`: two
//! workers 30 % more in all; default 1.0 for any N - the machine's rate shared, nothing gained; a beat counts for 250 ms); a worker told `finish`
//! removes its beat file (a drainer, which the agent stops while a crew is measured). With `rate` a rolling job keeps `conc` games in
//! flight (`games_started` = finished + in flight; still played one at a time), `finish` plays the games in flight to
//! the end first, and `status` carries `leaves`, `games_started`, `concurrency` and `wall_s`.
//!
//! Memory (rolling jobs) is committed in 1 MB pieces (`VirtualAlloc`), so it counts against the job's memory limit
//! as a real worker's does and the job's peak ends within 1 MB of the limit when a commit fails. Each change is logged
//! on stderr (`fakeworker: pid P holds N MB`, and `fakeworker: pid P exits` at the end), so a test can add up what a
//! lease's workers held at once. A failed commit ends
//! the worker like a real one out of memory: ONNX Runtime's fatal error (`... bad allocation`, exit 2), or with
//! `oom_abort` Rust's abort (`memory allocation of N bytes failed` on stderr, exit 0xC0000409).
//!
//! Eval jobs (PROTOCOL 5.2) self-test every network of `models` (the file must exist and match
//! its sha256), then write one JSON row per game to `evals-<seq>.jsonl` files announced with
//! `evals`; with `shard_every` odd, a file can end in the middle of a pair.
//!
//! Fuzz jobs can carry the same object as a JSON string in `env.FAKE`.
//!
//! An optional `startup.json` next to the executable (part of the signed test build) can delay
//! `hello`: `{"hello_delay_ms": 60000}`, and set the features `hello` lists: `{"features": []}` (an old
//! worker; the default is `["rolling"]`).
//!
//! Rolling self-play jobs (PROTOCOL 10.5, `"rolling": true` with the feature): games are played one at a time
//! (the job's `conc` only decides `low`: sent once per `games` value when fewer than `conc` released ids are
//! unstarted); past the released ids the worker waits for `extend` (more ids), `finish` or `quit`, still sending
//! `status`. Finished games are flushed every `shard_every` games or `flush_s` (`settings.fake.flush_ms` overrides
//! it) after the first unflushed one. A `checkpoint` is self-tested (`selftest` with `gen`: the file exists and
//! matches the `sha256` the agent sent) before the switch. Without the feature `rolling` is ignored and `extend` is
//! answered with a non-fatal `error`.

use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// macOS: no job objects.
#[cfg(not(windows))]
fn ui_limits() -> String {
    "none".into()
}

/// The UI restrictions of the job this process runs in (`none` outside a job): tests check the
/// agent's worker job.
#[cfg(windows)]
fn ui_limits() -> String {
    use windows_sys::Win32::System::JobObjects::{JobObjectBasicUIRestrictions, QueryInformationJobObject, JOBOBJECT_BASIC_UI_RESTRICTIONS};
    let mut info = JOBOBJECT_BASIC_UI_RESTRICTIONS { UIRestrictionsClass: 0 };
    let ok = unsafe {
        QueryInformationJobObject(
            std::ptr::null_mut(),
            JobObjectBasicUIRestrictions,
            &mut info as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        "none".into()
    } else {
        info.UIRestrictionsClass.to_string()
    }
}

struct Out;
impl Out {
    fn send(&self, v: Value) {
        let mut o = std::io::stdout().lock();
        let _ = writeln!(o, "{v}");
        let _ = o.flush();
    }
}

fn build_id() -> String {
    let exe = std::env::current_exe().unwrap_or_default();
    let m = exe.parent().map(|d| d.join("manifest.json")).unwrap_or_default();
    match std::fs::read(&m) {
        Ok(b) => sha256_hex(&b)[..24].to_string(),
        Err(_) => "unknown".into(),
    }
}

// Minimal sha256 so the fake worker has no crypto dependency (it is a test tool).
fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98,
        0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
        0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8,
        0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
        0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819,
        0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
        0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let mut msg = data.to_vec();
    let bitlen = (data.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bitlen.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([chunk[4 * i], chunk[4 * i + 1], chunk[4 * i + 2], chunk[4 * i + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [t1.wrapping_add(t2), v[0], v[1], v[2], v[3].wrapping_add(t1), v[4], v[5], v[6]];
        }
        for i in 0..8 {
            h[i] = h[i].wrapping_add(v[i]);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

fn spawn_stdin() -> Receiver<Option<Value>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(&line) {
                Ok(v) => {
                    if tx.send(Some(v)).is_err() {
                        return;
                    }
                }
                Err(e) => eprintln!("fakeworker: bad line {line:?}: {e}"),
            }
        }
        let _ = tx.send(None);
    });
    rx
}

/// Half-open id ranges `[[a, b)]` from a sorted list.
fn ranges(ids: &[u64]) -> Vec<[u64; 2]> {
    let mut out: Vec<[u64; 2]> = Vec::new();
    for &g in ids {
        match out.last_mut() {
            Some(r) if r[1] == g => r[1] = g + 1,
            _ => out.push([g, g + 1]),
        }
    }
    out
}

struct Cfg {
    game_ms: u64,
    shard_every: usize,
    status_ms: u64,
    yield_delay_ms: i64,
    crash_after: Option<u64>,
    hang_after: Option<u64>,
    fatal_after: Option<u64>,
    fail_seeds: Vec<u64>,
    chunk: u64,
    selftest_fail_gen: Option<u64>,
    /// Rolling jobs: overrides the job's `flush_s`.
    flush_ms: Option<u64>,
    mem_base_mb: u64,
    mem_per_game_mb: u64,
    mem_ramp_ms: u64,
    mem_grow_mb: u64,
    mem_grow_until: Option<u64>,
    oom_abort: bool,
    crash_gid: Option<u64>,
    rate: Option<Value>,
    build_ms: u64,
    provider: String,
    vram_base_mb: u64,
    vram_per_row_mb: u64,
    vram_oom_rows: Option<u64>,
}

impl Cfg {
    fn from(job: &Value) -> Cfg {
        // The script comes from the job's `settings.fake` object (agents since 0.2.5 pass on only
        // a job's template keys), its `fake` object, or (fuzz, whose jobs are `mode` and `env`
        // only) from `env.FAKE` as a JSON string.
        let f = job
            .get("settings")
            .and_then(|s| s.get("fake"))
            .or_else(|| job.get("fake"))
            .cloned()
            .or_else(|| job.get("env").and_then(|e| e.get("FAKE")).and_then(Value::as_str).and_then(|s| serde_json::from_str(s).ok()))
            .unwrap_or(json!({}));
        let u = |k: &str, d: u64| f.get(k).and_then(Value::as_u64).unwrap_or(d);
        let o = |k: &str| f.get(k).and_then(Value::as_u64);
        Cfg {
            game_ms: u("game_ms", 100),
            shard_every: u("shard_every", 2) as usize,
            status_ms: u("status_ms", 500),
            yield_delay_ms: f.get("yield_delay_ms").and_then(Value::as_i64).unwrap_or(0),
            crash_after: o("crash_after"),
            hang_after: o("hang_after"),
            fatal_after: o("fatal_after"),
            fail_seeds: f
                .get("fail_seeds")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_u64).collect())
                .unwrap_or_default(),
            chunk: u("chunk", 1000).max(1),
            selftest_fail_gen: o("selftest_fail_gen"),
            flush_ms: o("flush_ms"),
            mem_base_mb: u("mem_base_mb", 0),
            mem_per_game_mb: u("mem_per_game_mb", 0),
            mem_ramp_ms: u("mem_ramp_ms", 0),
            mem_grow_mb: u("mem_grow_mb", 0),
            mem_grow_until: o("mem_grow_until"),
            oom_abort: f.get("oom_abort").and_then(Value::as_bool).unwrap_or(false),
            crash_gid: o("crash_gid"),
            rate: f.get("rate").filter(|r| r.is_object()).cloned(),
            build_ms: u("build_ms", 0),
            provider: f.get("provider").and_then(Value::as_str).unwrap_or("cpu").to_string(),
            vram_base_mb: u("vram_base_mb", 0),
            vram_per_row_mb: u("vram_per_row_mb", 0),
            vram_oom_rows: o("vram_oom_rows"),
        }
    }
}

enum Stop {
    No,
    Finish,
    Quit,
}

/// Committed memory (`VirtualAlloc` MEM_COMMIT, 1 MB pieces, never freed): what a job's memory limit counts.
#[derive(Default)]
struct Mem {
    pieces: Vec<usize>,
}

impl Mem {
    /// macOS: allocates `mb` more MB and writes every page (what the physical footprint counts, and so the agent's
    /// memory cap). The agent kills a worker past its cap: nothing fails here.
    #[cfg(not(windows))]
    fn commit(&mut self, mb: u64, _out: &Out, _abort: bool) {
        for _ in 0..mb {
            let mut v = vec![0u8; 1 << 20];
            for i in (0..v.len()).step_by(4096) {
                v[i] = 1;
            }
            self.pieces.push(Box::leak(v.into_boxed_slice()).as_ptr() as usize);
        }
    }

    /// Commits `mb` more MB, or ends the worker the way a real one out of memory ends (`Cfg::oom_abort`).
    #[cfg(windows)]
    fn commit(&mut self, mb: u64, out: &Out, abort: bool) {
        use windows_sys::Win32::System::Memory::{VirtualAlloc, MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE};
        for _ in 0..mb {
            let p = unsafe { VirtualAlloc(std::ptr::null(), 1 << 20, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) };
            if p.is_null() {
                eprintln!("fakeworker: a commit failed with {} MB committed", self.pieces.len());
                if abort {
                    eprintln!("memory allocation of 1048576 bytes failed");
                    std::process::exit(0xC0000409u32 as i32);
                }
                out.send(json!({"ev": "error", "message": "ONNX Runtime: Non-zero status code returned while running Concat node. Status Message: bad allocation", "fatal": true}));
                std::process::exit(2);
            }
            self.pieces.push(p as usize);
        }
    }
}

struct W {
    out: Out,
    rx: Receiver<Option<Value>>,
    cfg: Cfg,
    threads: u64,
    out_dir: PathBuf,
    gen: Option<u64>,
    done: u64,
    seq: u64,
    yields: u64,
    last_status: Instant,
    stop: Stop,
    /// A rolling job runs (the job asked and the worker has the feature).
    rolling: bool,
    /// A rolling job's released ids (`games`, raised by `extend`).
    games: u64,
    mem: Mem,
    /// `rate`: the layout's leaves per second at full games in flight (None: no rate model).
    leaves_s: Option<f64>,
    decay_p: Option<f64>,
    leaves: f64,
    /// Ids started (finished + in flight) of a rolling job with a rate model, from its first id.
    started: u64,
    conc: u64,
    t0: Instant,
    t_acc: Instant,
    /// The crew's total rate by its size (`rate.crew`), this worker's beat file and the crew's size as last counted.
    crew: Vec<f64>,
    beat: Option<(PathBuf, Instant)>,
    crew_n: usize,
    /// Its sessions are being built (`build_ms`): no provider yet.
    building: bool,
    /// `mem_ramp_ms`: (the job's start, the games' MB to commit, committed so far).
    ramp: Option<(Instant, u64, u64)>,
    /// The job's batch rows (conc / groups, rounded up) and whether it yielded (its sessions released).
    batch: u64,
    yielded: bool,
}

impl W {
    fn stats(&self) -> Value {
        let provider = if self.building { "" } else { self.cfg.provider.as_str() };
        let mut s = json!({"games": self.done, "decisions": self.done * 100, "threads": self.threads, "provider": provider,
               "cpu_pct": 1, "rss_mb": 5, "yields": self.yields});
        if self.cfg.vram_base_mb + self.cfg.vram_per_row_mb > 0 {
            let held = !self.building && !self.yielded;
            s["vram_mb"] = json!(if held { self.cfg.vram_base_mb + self.cfg.vram_per_row_mb * self.batch } else { 0 });
        }
        if self.leaves_s.is_some() {
            s["leaves"] = json!(self.leaves as u64);
            s["games_started"] = json!(self.started);
            s["concurrency"] = json!(self.conc);
            s["wall_s"] = json!((self.t0.elapsed().as_secs_f64() * 10.0).round() / 10.0);
        }
        s
    }

    /// Leaves played since the last call (at most 100 ms counted at a time: a suspended worker's time doesn't count).
    fn accrue(&mut self) {
        let dt = self.t_acc.elapsed().as_secs_f64().min(0.1);
        self.t_acc = Instant::now();
        let Some(r) = self.leaves_s else { return };
        self.crew_beat();
        let conc = self.conc.max(1) as f64;
        let in_flight = self.started.saturating_sub(self.done) as f64;
        let decay = self.decay_p.map_or(1.0, |p| p / (p + self.leaves / conc));
        let n = self.crew_n.max(1);
        let share = self.crew.get(n - 1).or(self.crew.last()).copied().unwrap_or(1.0) / n as f64;
        self.leaves += r * dt * (in_flight / conc).min(1.0) * decay * share;
    }

    /// Every 100 ms: this worker's beat file touched, and the crew counted (beat files touched in the last 250 ms: a
    /// stopped or dead worker drops out - before a trial's window starts, `tune_warm` after its refill in the tests).
    fn crew_beat(&mut self) {
        let Some((path, at)) = self.beat.as_mut() else { return };
        if at.elapsed() < Duration::from_millis(100) {
            return;
        }
        *at = Instant::now();
        // (changing content: on Windows rewriting an empty file leaves its modification time - every worker then counted
        // a crew of one)
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let _ = std::fs::write(&*path, stamp.to_string());
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let fresh = |e: &std::fs::DirEntry| e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|d| d < Duration::from_millis(250));
        self.crew_n = std::fs::read_dir(dir).map(|rd| rd.flatten().filter(fresh).count()).unwrap_or(1).max(1);
    }

    fn status(&mut self) {
        self.out.send(json!({"ev": "status", "stats": self.stats()}));
        self.last_status = Instant::now();
    }

    /// Handles one op. Returns false when the worker must stop now.
    fn handle(&mut self, op: Option<Value>) {
        let Some(op) = op else {
            // stdin closed: the agent is gone.
            std::process::exit(0);
        };
        match op.get("op").and_then(Value::as_str).unwrap_or("") {
            "ping" => self.status(),
            "checkpoint" => self.checkpoint(&op),
            "extend" if self.rolling => {
                let g = op.get("games").and_then(Value::as_u64).unwrap_or(0);
                eprintln!("fakeworker: extend to {g} games (from {})", self.games);
                self.games = self.games.max(g);
            }
            "extend" => self.out.send(json!({"ev": "error", "message": "unknown op extend", "fatal": false})),
            "finish" => {
                self.stop = Stop::Finish;
                // (a worker finishing its games no longer counts in the crew: the agent stops such a drainer while
                // the crew is measured, and its beat file would stay fresh for up to 250 ms after - agent 0.3.3)
                if let Some((path, _)) = self.beat.take() {
                    let _ = std::fs::remove_file(path);
                }
            }
            "quit" => self.stop = Stop::Quit,
            "yield" => self.do_yield(),
            "resume" => {} // not yielded: nothing to do
            other => eprintln!("fakeworker: unknown op {other:?}"),
        }
    }

    /// A new network: in a rolling job self-tested first (`selftest` with `gen`), then played.
    fn checkpoint(&mut self, op: &Value) {
        let gen = op.get("gen").and_then(Value::as_u64);
        if self.rolling {
            let sha = op.get("sha256").and_then(Value::as_str);
            let file_ok = op
                .get("model")
                .and_then(Value::as_str)
                .and_then(|p| std::fs::read(p).ok())
                .is_some_and(|b| sha.is_none_or(|h| sha256_hex(&b) == h));
            self.out.send(json!({"ev": "selftest", "gen": gen, "provider": self.cfg.provider, "max_abs": if file_ok { 0.0 } else { 0.5 }, "ok": file_ok}));
            if !file_ok {
                self.out.send(json!({"ev": "error", "message": format!("network {gen:?} failed the self-test"), "fatal": true}));
                std::process::exit(2);
            }
            eprintln!("fakeworker: checkpoint {gen:?} self-tested (sha256 given: {})", sha.is_some());
        }
        self.gen = gen;
        eprintln!("fakeworker: switching to checkpoint {:?} at the next game", self.gen);
    }

    fn do_yield(&mut self) {
        self.yields += 1;
        if self.cfg.yield_delay_ms < 0 {
            eprintln!("fakeworker: ignoring yield");
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
        if self.cfg.yield_delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(self.cfg.yield_delay_ms as u64));
        }
        self.yielded = true;
        eprintln!("fakeworker: pid {} yielded", std::process::id());
        self.out.send(json!({"ev": "yielded"}));
        // Wait for resume or quit. (The agent suspends us; time stands still meanwhile.)
        loop {
            match self.rx.recv() {
                Ok(Some(op)) => match op.get("op").and_then(Value::as_str).unwrap_or("") {
                    "resume" => {
                        self.yielded = false;
                        eprintln!("fakeworker: pid {} resumed", std::process::id());
                        self.out.send(json!({"ev": "resumed"}));
                        return;
                    }
                    "quit" => {
                        self.stop = Stop::Quit;
                        self.yielded = false;
                        return;
                    }
                    "finish" => self.stop = Stop::Finish,
                    "ping" => self.status(),
                    "checkpoint" => self.checkpoint(&op),
                    "extend" if self.rolling => self.games = self.games.max(op.get("games").and_then(Value::as_u64).unwrap_or(0)),
                    _ => {}
                },
                Ok(None) | Err(_) => std::process::exit(0),
            }
        }
    }

    /// Sleeps `ms` of "work", serving ops and status meanwhile. Returns early on quit.
    fn work(&mut self, ms: u64) {
        let end = Instant::now() + Duration::from_millis(ms);
        loop {
            if matches!(self.stop, Stop::Quit) {
                return;
            }
            self.accrue();
            self.ramp_mem();
            if self.last_status.elapsed() >= Duration::from_millis(self.cfg.status_ms) {
                self.status();
            }
            let now = Instant::now();
            if now >= end {
                return;
            }
            match self.rx.recv_timeout((end - now).min(Duration::from_millis(20))) {
                Ok(op) => self.handle(op),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => std::process::exit(0),
            }
        }
    }

    fn after_game_hooks(&mut self) {
        self.after_game_hooks_gid(None);
    }

    /// `after_game_hooks`, `gid` the game just finished (`crash_gid`).
    fn after_game_hooks_gid(&mut self, gid: Option<u64>) {
        if gid.is_some() && self.cfg.crash_gid == gid {
            eprintln!("thread 'main' panicked at src/fake.rs:21:5:
fake crash at game {gid:?}");
            std::process::exit(101);
        }
        if self.cfg.crash_after == Some(self.done) {
            eprintln!("thread 'main' panicked at src/fake.rs:13:5:\nfake crash after {} games", self.done);
            std::process::exit(101);
        }
        if self.cfg.fatal_after == Some(self.done) {
            self.out.send(json!({"ev": "error", "message": "fake fatal error", "fatal": true}));
            std::process::exit(2);
        }
        if self.cfg.hang_after == Some(self.done) {
            eprintln!("fakeworker: hanging");
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
    }

    fn flush(&mut self, gids: &mut Vec<u64>) {
        if gids.is_empty() {
            return;
        }
        let name = format!("shard-{}.bin", self.seq);
        let body = format!("fake shard seq={} gids={:?} gen={:?} ui_limits={}\n", self.seq, gids, self.gen, ui_limits());
        let _ = std::fs::create_dir_all(&self.out_dir);
        std::fs::write(self.out_dir.join(&name), body.as_bytes()).expect("write shard");
        let mut ev = json!({"ev": "shard", "seq": self.seq, "path": name, "games": gids.len(),
                            "rows": gids.len() * 10, "bytes": body.len(), "gids": ranges(gids)});
        if let Some(g) = self.gen {
            ev["gen"] = json!(g);
        }
        self.out.send(ev);
        self.seq += 1;
        gids.clear();
    }

    fn run_games(&mut self, job: &Value) {
        let games = job.get("games").and_then(Value::as_u64).unwrap_or(4);
        let base = job.get("gid_base").and_then(Value::as_u64).unwrap_or(0);
        let mut unflushed = Vec::new();
        let mut next = 0u64;
        while next < games {
            if !matches!(self.stop, Stop::No) {
                break;
            }
            self.work(self.cfg.game_ms);
            if matches!(self.stop, Stop::Quit) {
                break;
            }
            unflushed.push(base + next);
            next += 1;
            self.done += 1;
            self.out.send(json!({"ev": "fin", "n": 1}));
            self.after_game_hooks();
            if unflushed.len() >= self.cfg.shard_every {
                self.flush(&mut unflushed);
            }
        }
        self.flush(&mut unflushed);
        let reason = match self.stop {
            Stop::No => "done",
            Stop::Finish => "finish",
            Stop::Quit => "quit",
        };
        let rest: Vec<u64> = (base + next..base + games).collect();
        let unfinished = if rest.is_empty() { Value::Null } else { json!({"gids": ranges(&rest)}) };
        self.out.send(json!({"ev": "end", "reason": reason, "stats": self.stats(), "unfinished": unfinished}));
    }

    /// `mem_ramp_ms`: the games in flight's memory committed in ten steps over that long from the job's start.
    fn ramp_mem(&mut self) {
        let Some((t0, total, done)) = self.ramp else { return };
        let steps = ((t0.elapsed().as_millis() as u64 * 10) / self.cfg.mem_ramp_ms.max(1)).min(10);
        let want = total * steps / 10;
        if want > done {
            self.mem.commit(want - done, &self.out, self.cfg.oom_abort);
            self.ramp = (want < total).then_some((t0, total, want));
            self.note_mem();
        }
    }

    /// What this worker holds now, on stderr (a test adds up a lease's workers).
    fn note_mem(&self) {
        if !self.mem.pieces.is_empty() {
            eprintln!("fakeworker: pid {} holds {} MB", std::process::id(), self.mem.pieces.len());
        }
    }

    /// A rolling self-play job (PROTOCOL 10.5).
    fn run_rolling(&mut self, job: &Value) {
        let base = job.get("gid_base").and_then(Value::as_u64).unwrap_or(0);
        let conc = job.get("conc").and_then(Value::as_u64).unwrap_or(1).max(1);
        let flush = Duration::from_millis(self.cfg.flush_ms.unwrap_or(job.get("flush_s").and_then(Value::as_u64).unwrap_or(120) * 1000));
        self.games = job.get("games").and_then(Value::as_u64).unwrap_or(4);
        eprintln!("fakeworker: rolling job, {} ids released, {conc} in flight, flush {flush:?}", self.games);
        // the runtime's and the games in flight's memory (or the latter in steps, `ramp_mem`)
        let games_mb = conc * self.cfg.mem_per_game_mb;
        if self.cfg.mem_ramp_ms > 0 && games_mb > 0 {
            self.mem.commit(self.cfg.mem_base_mb, &self.out, self.cfg.oom_abort);
            self.ramp = Some((Instant::now(), games_mb, 0));
        } else {
            self.mem.commit(self.cfg.mem_base_mb + games_mb, &self.out, self.cfg.oom_abort);
        }
        self.note_mem();
        let mut unflushed = Vec::new();
        let mut first_fin: Option<Instant> = None;
        let mut next = 0u64;
        let mut low_for = None;
        let model = self.leaves_s.is_some();
        self.conc = conc;
        loop {
            // (with a rate model: `conc` games in flight; `finish` plays them to the end, starting none)
            match self.stop {
                Stop::No if model => self.started = self.games.min(next + conc).max(self.started),
                Stop::No => {}
                Stop::Finish if model && next < self.started => {}
                _ => break,
            }
            if next < self.games && (!model || next < self.started) {
                let started = if model { self.started } else { next + 1 };
                if matches!(self.stop, Stop::No) && self.games - started < conc && low_for != Some(self.games) {
                    low_for = Some(self.games);
                    self.out.send(json!({"ev": "low", "started": started, "games": self.games}));
                }
                self.work(self.cfg.game_ms);
                if matches!(self.stop, Stop::Quit) {
                    break;
                }
                unflushed.push(base + next);
                first_fin.get_or_insert_with(Instant::now);
                next += 1;
                self.done += 1;
                self.out.send(json!({"ev": "fin", "n": 1}));
                self.after_game_hooks_gid(Some(base + next - 1));
                if self.cfg.mem_grow_until.is_none_or(|u| self.done <= u) && self.cfg.mem_grow_mb > 0 {
                    self.mem.commit(self.cfg.mem_grow_mb, &self.out, self.cfg.oom_abort);
                    self.note_mem();
                }
            } else {
                // every released id is played: wait for more (still answering ops, sending status)
                if low_for != Some(self.games) {
                    low_for = Some(self.games);
                    self.out.send(json!({"ev": "low", "started": next, "games": self.games}));
                }
                self.work(20);
            }
            if unflushed.len() >= self.cfg.shard_every || first_fin.is_some_and(|t| t.elapsed() >= flush) {
                self.flush(&mut unflushed);
                first_fin = None;
            }
        }
        self.flush(&mut unflushed);
        let reason = match self.stop {
            Stop::Quit => "quit",
            _ => "finish",
        };
        let rest: Vec<u64> = if model && !matches!(self.stop, Stop::Quit) { (base + self.started.max(next)..base + self.games).collect() } else { (base + next..base + self.games).collect() };
        let unfinished = if rest.is_empty() { Value::Null } else { json!({"gids": ranges(&rest)}) };
        eprintln!("fakeworker: rolling job ended ({reason}): {next} of {} released ids played", self.games);
        self.out.send(json!({"ev": "end", "reason": reason, "stats": self.stats(), "unfinished": unfinished}));
    }

    /// Eval: one `selftest` per network in `models` (by gen); a network whose file is missing or
    /// doesn't match its sha256 (or `selftest_fail_gen`) fails it, and the worker refuses to play.
    fn selftest_models(&mut self, job: &Value) -> bool {
        let Some(models) = job.get("models").and_then(Value::as_object) else { return true };
        let mut gens: Vec<(u64, &Value)> = models.iter().filter_map(|(g, m)| g.parse().ok().map(|g| (g, m))).collect();
        gens.sort_by_key(|(g, _)| *g);
        for (gen, m) in gens {
            let file_ok = |path: Option<&str>, sha: Option<&str>| match path {
                Some(p) => std::fs::read(p).ok().is_some_and(|b| sha.is_none_or(|h| sha256_hex(&b) == h)),
                None => true,
            };
            let ok = m.get("model").and_then(Value::as_str).is_some()
                && file_ok(m.get("model").and_then(Value::as_str), m.get("sha256").and_then(Value::as_str))
                && file_ok(m.get("reference").and_then(Value::as_str), None)
                && self.cfg.selftest_fail_gen != Some(gen);
            self.out.send(json!({"ev": "selftest", "gen": gen, "provider": self.cfg.provider, "max_abs": if ok { 2.5e-6 } else { 0.5 }, "ok": ok}));
            if !ok {
                self.out.send(json!({"ev": "error", "message": format!("network {gen} failed the self-test"), "fatal": true}));
                return false;
            }
        }
        true
    }

    /// Eval rows (PROTOCOL 5.2), one JSON line per game.
    fn flush_evals(&mut self, job: &Value, gids: &mut Vec<u64>) {
        if gids.is_empty() {
            return;
        }
        let paired = job.get("paired").and_then(Value::as_bool).unwrap_or(false);
        let seats: Vec<Value> = job.get("seats").and_then(Value::as_array).cloned().unwrap_or_default();
        let mut body = String::new();
        for &g in gids.iter() {
            let mut s = seats.clone();
            if paired && g % 2 == 1 {
                s.reverse();
            }
            let result = if g % 3 == 0 { json!([0.5, 0.5]) } else if g % 2 == 0 { json!([1, 0]) } else { json!([0, 1]) };
            let row = json!({"gid": g, "pair": if paired { json!(g / 2) } else { Value::Null }, "seats": s, "result": result,
                             "margin": (g % 7) as i64 - 3, "rounds": 5 + g % 4, "over": "life", "net_ms": 1.5});
            body.push_str(&row.to_string());
            body.push('\n');
        }
        let name = format!("evals-{}.jsonl", self.seq);
        let _ = std::fs::create_dir_all(&self.out_dir);
        std::fs::write(self.out_dir.join(&name), body.as_bytes()).expect("write evals");
        self.out.send(json!({"ev": "evals", "seq": self.seq, "path": name, "games": gids.len(), "rows": gids.len(),
                             "bytes": body.len(), "gids": ranges(gids)}));
        self.seq += 1;
        gids.clear();
    }

    fn run_eval(&mut self, job: &Value) {
        if !self.selftest_models(job) {
            std::process::exit(2);
        }
        let games = job.get("games").and_then(Value::as_u64).unwrap_or(4);
        let base = job.get("gid_base").and_then(Value::as_u64).unwrap_or(0);
        let mut unflushed = Vec::new();
        let mut next = 0u64;
        while next < games && matches!(self.stop, Stop::No) {
            self.work(self.cfg.game_ms);
            if matches!(self.stop, Stop::Quit) {
                break;
            }
            unflushed.push(base + next);
            next += 1;
            self.done += 1;
            self.out.send(json!({"ev": "fin", "n": 1}));
            self.after_game_hooks();
            if unflushed.len() >= self.cfg.shard_every {
                self.flush_evals(job, &mut unflushed);
            }
        }
        self.flush_evals(job, &mut unflushed);
        let reason = match self.stop {
            Stop::No => "done",
            Stop::Finish => "finish",
            Stop::Quit => "quit",
        };
        let rest: Vec<u64> = (base + next..base + games).collect();
        let unfinished = if rest.is_empty() { Value::Null } else { json!({"gids": ranges(&rest)}) };
        self.out.send(json!({"ev": "end", "reason": reason, "stats": self.stats(), "unfinished": unfinished}));
    }

    fn run_fuzz(&mut self, job: &Value) {
        let first = job.get("first").and_then(Value::as_u64).unwrap_or(0);
        let count = job.get("count").and_then(Value::as_u64).unwrap_or(1000);
        let mode = job.get("mode").and_then(Value::as_str).unwrap_or("fake").to_string();
        let mut at = first;
        let end = first + count;
        while at < end && matches!(self.stop, Stop::No) {
            let step_end = (at + self.cfg.chunk).min(end);
            self.work(self.cfg.game_ms);
            if matches!(self.stop, Stop::Quit) {
                break;
            }
            for &s in self.cfg.fail_seeds.clone().iter().filter(|s| **s >= at && **s < step_end) {
                self.out.send(json!({"ev": "failure", "seed": s, "mode": mode,
                    "panic": format!("fake panic at seed {s}"), "site": "engine/src/fake.rs:42:9",
                    "repro": format!("FUZZ_FIRST={s} FUZZ_GAMES=1 FUZZ_POOL={mode}"), "output_tail": "...last lines..."}));
            }
            self.done += step_end - at;
            self.out.send(json!({"ev": "fin", "n": step_end - at}));
            at = step_end;
            self.after_game_hooks();
        }
        let reason = match self.stop {
            Stop::No => "done",
            Stop::Finish => "finish",
            Stop::Quit => "quit",
        };
        let unfinished = if at < end { json!({"seeds": [[at, end]]}) } else { Value::Null };
        self.out.send(json!({"ev": "end", "reason": reason, "stats": self.stats(), "unfinished": unfinished}));
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // `probe-providers ... --json FILE` (PROTOCOL 8, 9.6): writes the build's `probe.json` (next to the exe) as the
    // report, else one where only the CPU passes; the arguments it got go to the log.
    if args.get(1).map(String::as_str) == Some("probe-providers") {
        eprintln!("fakeworker: probe-providers {}", args[2..].join(" "));
        let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_default();
        // with --model (a job's network: the bench), the build's `probe_model.json` when it has one
        let on_model = args.iter().any(|a| a == "--model") && exe_dir.join("probe_model.json").exists();
        let report = std::fs::read_to_string(exe_dir.join(if on_model { "probe_model.json" } else { "probe.json" })).unwrap_or_else(|_| {
            json!({"probe": 1, "providers": ["cpu"], "error": null, "results": [
                {"provider": "cpu", "gpu": null, "ok": true, "rows_per_s": 1000.0,
                 "rows_per_s_by_batch": {"1": 1000.0, "8": 1000.0, "32": 1000.0, "128": 1000.0, "512": 1000.0, "1024": 1000.0}}]})
            .to_string()
        });
        if let Some(i) = args.iter().position(|a| a == "--json") {
            std::fs::write(&args[i + 1], report).unwrap();
        } else {
            println!("{report}");
        }
        return;
    }
    if args.get(1).map(String::as_str) != Some("serve") {
        eprintln!("usage: fakeworker serve");
        std::process::exit(64);
    }
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_default();
    if let Ok(s) = std::fs::read_to_string(exe_dir.join("startup.json")) {
        let v: Value = serde_json::from_str(&s).unwrap_or(json!({}));
        if let Some(ms) = v.get("hello_delay_ms").and_then(Value::as_u64) {
            std::thread::sleep(Duration::from_millis(ms));
        }
    }
    let features: Value = std::fs::read_to_string(exe_dir.join("startup.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("features").cloned())
        .unwrap_or(json!(["rolling"]));
    let rolling_feature = features.as_array().is_some_and(|f| f.iter().any(|x| x == "rolling"));
    let out = Out;
    out.send(json!({"ev": "hello", "aw": 1, "build_id": build_id(), "game": "fake",
                    "kinds": ["fuzz", "eval", "selfplay"], "zero_onnx": [1], "pid": std::process::id(), "features": features}));
    let rx = spawn_stdin();
    let job = loop {
        match rx.recv() {
            Ok(Some(v)) if v.get("op").and_then(Value::as_str) == Some("job") => break v,
            Ok(Some(v)) if v.get("op").and_then(Value::as_str) == Some("quit") => {
                out.send(json!({"ev": "end", "reason": "quit", "stats": {}, "unfinished": null}));
                return;
            }
            Ok(Some(v)) => eprintln!("fakeworker: ignoring {v} before job"),
            _ => return,
        }
    };
    eprintln!("fakeworker: job {job}");
    let cfg = Cfg::from(&job);
    let mut w = W {
        out,
        rx,
        cfg,
        threads: job.get("threads").and_then(Value::as_u64).unwrap_or(1),
        out_dir: PathBuf::from(job.get("out_dir").and_then(Value::as_str).unwrap_or("out")),
        gen: None,
        done: 0,
        seq: 0,
        yields: 0,
        last_status: Instant::now(),
        stop: Stop::No,
        rolling: rolling_feature && job.get("kind").and_then(Value::as_str) == Some("selfplay") && job.get("rolling").and_then(Value::as_bool) == Some(true),
        games: 0,
        mem: Mem::default(),
        leaves_s: None,
        decay_p: None,
        leaves: 0.0,
        started: 0,
        conc: 1,
        t0: Instant::now(),
        t_acc: Instant::now(),
        crew: vec![],
        beat: None,
        crew_n: 1,
        building: false,
        ramp: None,
        batch: {
            let conc = job.get("conc").and_then(Value::as_u64).unwrap_or(1).max(1);
            conc.div_ceil(job.get("groups").and_then(Value::as_u64).unwrap_or(1).max(1))
        },
        yielded: false,
    };
    if let Some(r) = w.cfg.rate.clone() {
        w.leaves_s = Some(layout_rate(&r, &job));
        w.decay_p = r.get("decay_p").and_then(Value::as_f64);
        w.crew = r.get("crew").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default();
        // (the crew's beat files: in the lease's working directory, shared by its workers)
        let dir = Path::new(".crew");
        if std::fs::create_dir_all(dir).is_ok() {
            let f = dir.join(std::process::id().to_string());
            let _ = std::fs::write(&f, b"");
            w.beat = Some((f, Instant::now()));
        }
        eprintln!("fakeworker: {} leaves/s at this layout", w.leaves_s.unwrap_or(0.0));
    }
    if w.cfg.build_ms > 0 {
        // (a real worker builds its sessions first: its `status` says `provider: ""` until they exist)
        w.building = true;
        w.status();
        let ms = w.cfg.build_ms;
        w.work(ms);
        w.building = false;
        eprintln!("fakeworker: sessions built ({})", w.cfg.provider);
    }
    if job.get("model").and_then(Value::as_str).is_some() {
        w.out.send(json!({"ev": "selftest", "provider": w.cfg.provider, "max_abs": 0.0, "ok": true}));
    }
    // (a batch the GPU's memory doesn't hold: the CUDA allocator's error at the first batch)
    if w.cfg.vram_oom_rows.is_some_and(|n| w.batch > n) {
        eprintln!("fakeworker: batches of {} rows don't fit the GPU", w.batch);
        w.out.send(json!({"ev": "error", "fatal": true, "message": "ONNX Runtime: Non-zero status code returned while running MatMul node. Status Message: onnxruntime::CudaCall CUDA failure 2: out of memory ; GPU=0 ; expr=cudaMalloc((void**)&p, size);"}));
        std::process::exit(2);
    }
    w.status();
    match job.get("kind").and_then(Value::as_str) {
        Some("fuzz") => w.run_fuzz(&job),
        Some("eval") => w.run_eval(&job),
        _ if w.rolling => {
            w.run_rolling(&job);
            eprintln!("fakeworker: pid {} exits", std::process::id());
        }
        _ => w.run_games(&job),
    }
}

/// `rate`'s leaves per second for the job's layout (see the module's notes).
fn layout_rate(r: &Value, job: &Value) -> f64 {
    let u = |k: &str| job.get(k).and_then(Value::as_u64);
    let threads = u("threads").unwrap_or(1).max(1) as f64;
    let workers = u("infer_workers").unwrap_or(1).max(1) as f64;
    let per = u("infer_threads").map_or(threads, |t| t.max(1) as f64);
    let conc = u("conc").unwrap_or(1).max(1) as f64;
    let groups = u("groups").unwrap_or(1).max(1) as f64;
    let have = |k: &str| match k {
        "threads" => Some(threads),
        "infer_workers" => Some(workers),
        "infer_threads" => Some(per),
        "batch" => Some((conc / groups).ceil()),
        "conc" => Some(conc),
        _ => None,
    };
    let pen = r.get("penalty").and_then(Value::as_f64).unwrap_or(0.15);
    let mut x = r.get("leaves_s").and_then(Value::as_f64).unwrap_or(1000.0);
    for (k, b) in r.get("best").and_then(Value::as_object).into_iter().flatten() {
        if let (Some(v), Some(b)) = (have(k), b.as_f64().filter(|b| *b > 0.0)) {
            x *= (1.0 - pen * (v / b).log2().abs()).max(0.05);
        }
    }
    x
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn layout_rates() {
        let r = json!({"leaves_s": 1000, "best": {"threads": 4, "batch": 8}, "penalty": 0.25});
        assert_eq!(layout_rate(&r, &json!({"threads": 4, "conc": 32, "groups": 4})), 1000.0);
        assert_eq!(layout_rate(&r, &json!({"threads": 8, "conc": 32, "groups": 4})), 750.0, "one doubling away");
        assert_eq!(layout_rate(&r, &json!({"threads": 8, "conc": 32, "groups": 2})), 562.5);
    }

    #[test]
    fn sha_and_ranges() {
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(sha256_hex(&[b'a'; 200])[..8], *"c2a908d9");
        assert_eq!(ranges(&[1, 2, 3, 7, 8, 10]), vec![[1, 4], [7, 9], [10, 11]]);
    }
}
