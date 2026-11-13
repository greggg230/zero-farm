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
//!  "selftest_fail_gen": null}  // eval: this network fails its self-test (then a fatal error)
//! ```
//!
//! Eval jobs (PROTOCOL 5.2) self-test every network of `models` (the file must exist and match
//! its sha256), then write one JSON row per game to `evals-<seq>.jsonl` files announced with
//! `evals`; with `shard_every` odd, a file can end in the middle of a pair.
//!
//! Fuzz jobs can carry the same object as a JSON string in `env.FAKE`.
//!
//! An optional `startup.json` next to the executable (part of the signed test build) can delay
//! `hello`: `{"hello_delay_ms": 60000}`.

use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// The UI restrictions of the job this process runs in (`none` outside a job): tests check the
/// agent's worker job.
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
        }
    }
}

enum Stop {
    No,
    Finish,
    Quit,
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
}

impl W {
    fn stats(&self) -> Value {
        json!({"games": self.done, "decisions": self.done * 100, "threads": self.threads, "provider": "cpu",
               "cpu_pct": 1, "rss_mb": 5, "yields": self.yields})
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
            "checkpoint" => {
                self.gen = op.get("gen").and_then(Value::as_u64);
                eprintln!("fakeworker: switching to checkpoint {:?} at the next game", self.gen);
            }
            "finish" => self.stop = Stop::Finish,
            "quit" => self.stop = Stop::Quit,
            "yield" => self.do_yield(),
            "resume" => {} // not yielded: nothing to do
            other => eprintln!("fakeworker: unknown op {other:?}"),
        }
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
        self.out.send(json!({"ev": "yielded"}));
        // Wait for resume or quit. (The agent suspends us; time stands still meanwhile.)
        loop {
            match self.rx.recv() {
                Ok(Some(op)) => match op.get("op").and_then(Value::as_str).unwrap_or("") {
                    "resume" => {
                        self.out.send(json!({"ev": "resumed"}));
                        return;
                    }
                    "quit" => {
                        self.stop = Stop::Quit;
                        return;
                    }
                    "finish" => self.stop = Stop::Finish,
                    "ping" => self.status(),
                    "checkpoint" => self.gen = op.get("gen").and_then(Value::as_u64),
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
            self.out.send(json!({"ev": "selftest", "gen": gen, "provider": "cpu", "max_abs": if ok { 2.5e-6 } else { 0.5 }, "ok": ok}));
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
        let report = std::fs::read_to_string(exe_dir.join("probe.json")).unwrap_or_else(|_| {
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
    let out = Out;
    out.send(json!({"ev": "hello", "aw": 1, "build_id": build_id(), "game": "fake",
                    "kinds": ["fuzz", "eval", "selfplay"], "zero_onnx": [1], "pid": std::process::id()}));
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
    };
    if job.get("model").and_then(Value::as_str).is_some() {
        w.out.send(json!({"ev": "selftest", "provider": "cpu", "max_abs": 0.0, "ok": true}));
    }
    w.status();
    match job.get("kind").and_then(Value::as_str) {
        Some("fuzz") => w.run_fuzz(&job),
        Some("eval") => w.run_eval(&job),
        _ => w.run_games(&job),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sha_and_ranges() {
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(sha256_hex(&[b'a'; 200])[..8], *"c2a908d9");
        assert_eq!(ranges(&[1, 2, 3, 7, 8, 10]), vec![[1, 4], [7, 9], [10, 11]]);
    }
}
