//! Several workers serving one rolling lease (rev 19, PROTOCOL 13.5) with the real engine, the real fakeworker (its
//! `rate` model: counters, and the machine shared by the crew) and the mock coordinator: each worker on its own range of
//! the lease's released ids (only the tail extended), members that started all of their ids replaced, every released id
//! played once or returned, results from every worker uploaded under the lease, the crew paused and resumed together, a
//! member's crash leaving the others going, a new generation switched to by all of them, the memory plan for the whole
//! crew, the tuner choosing the worker count, and (0.3.2) workers that report no provider before their sessions exist
//! never taken for GPU jobs.

mod harness;

use harness::{wait, H};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

const RUN: &str = "crew-run";

/// Serves network `gen` for the run: (lease checkpoint, definition entry).
fn network(h: &H, gen: u64) -> (Value, Value) {
    let (m, r) = (format!("onnx graph {RUN} {gen}").into_bytes(), format!("reference rows {RUN} {gen}").into_bytes());
    let (ml, rl) = (m.len(), r.len());
    let model = format!("ckpt/{RUN}/{gen}/model.onnx");
    let reference = format!("ckpt/{RUN}/{gen}/reference.npz");
    let sha = h.mock.add_artifact(&model, m);
    let rsha = h.mock.add_artifact(&reference, r);
    (
        json!({"gen": gen, "model": model, "sha256": sha, "reference": reference, "reference_sha256": rsha}),
        json!({"model_sha256": sha, "model_size": ml, "reference_sha256": rsha, "reference_size": rl}),
    )
}

fn definition(h: &H, version: u64, checkpoints: Value, fake: &Value) -> Value {
    json!({"definition": 1, "kind": "run", "id": RUN, "version": version, "created": zerofarm_common::now_unix() - 60,
           "expires": null, "game": "fake", "builds": [h.build_id], "runtime": null, "checkpoints": checkpoints,
           "jobs": {"selfplay": {"settings": {"sims": 8, "fake": fake}}}})
}

/// A rolling lease of ids `[0, block)` (8 released), `inflight` games in flight on the CPU, its definition's settings
/// carrying `fake`.
fn crew_lease(h: &H, id: &str, fake: &Value, inflight: Value, block: u64) -> Value {
    let (c1, d1) = network(h, 1);
    let dref = h.define(&definition(h, 1, json!({"1": d1}), fake));
    let mut l = h.lease(id, json!({"kind": "selfplay", "gid_base": 0, "games": 8, "seed": 7}));
    l["run"] = json!(RUN);
    l["definition"] = dref;
    l["checkpoint"] = c1;
    l["rolling"] = json!({"block": [0, block], "inflight": {"cpu": inflight, "gpu": "auto"},
                          "until": zerofarm_common::now_unix() + 7200, "max_s": 7200, "flush_s": 10});
    l
}

/// The fake: games of `game_ms`, small chunks, statuses every 100 ms, a rate model (counters; `crew`: the crew's total
/// by its size).
fn fake(game_ms: u64, crew: Value) -> Value {
    json!({"game_ms": game_ms, "shard_every": 3, "flush_ms": 300, "status_ms": 100, "rate": {"leaves_s": 1000, "best": {}, "crew": crew}})
}

/// The harness with `workers` per job (autotune off: a fixed crew, no trials, unless `tune`); the fake worker never
/// counted as starved by the test machine's load (it sleeps).
fn harness(workers: Option<u32>, tune: bool) -> H {
    let mut h = H::new();
    h.signals.real_starvation = false;
    let mut m = h.machine.lock().unwrap();
    m.settings.workers = workers;
    m.settings.autotune = tune;
    drop(m);
    h
}

fn job_lines(h: &H) -> Vec<Value> {
    (0..).map_while(|n| h.job_line_n(n)).collect()
}

/// A job line's ids: `[gid_base, gid_base + games)` as first given.
fn range_of(j: &Value) -> (u64, u64) {
    let b = j["gid_base"].as_u64().unwrap();
    (b, b + j["games"].as_u64().unwrap())
}

/// Every id credited to `id`'s chunks, and how many were credited a second time.
fn credits(h: &H, id: &str) -> (u64, u64) {
    h.mock.with(|s| s.credits.iter().filter(|(l, ..)| l == id).fold((0, 0), |(c, a), (_, _, x, y)| (c + x, a + y)))
}

fn unfinished_n(done: &Value) -> u64 {
    done["unfinished"]["gids"].as_array().map_or(0, |u| u.iter().map(|r| r[1].as_u64().unwrap() - r[0].as_u64().unwrap()).sum())
}

/// No id played twice, and every released id credited or reported unfinished.
fn ids_accounted(h: &H, id: &str, done: &Value) {
    let (credited, again) = credits(h, id);
    assert_eq!(again, 0, "an id played twice: {done}");
    let released = done["stats"]["released"].as_u64().unwrap();
    assert_eq!(credited + unfinished_n(done), released, "{done}");
}

/// The x-ids of every result chunk of `id`.
fn chunk_ids(h: &H, id: &str) -> Vec<(u64, u64)> {
    h.mock
        .results_for(id)
        .iter()
        .filter_map(|(_, r)| r.headers.get("x-ids").and_then(|v| serde_json::from_str::<Value>(v).ok()))
        .flat_map(|v| v.as_array().cloned().unwrap_or_default())
        .map(|r| (r[0].as_u64().unwrap(), r[1].as_u64().unwrap()))
        .collect()
}

/// The workers (their first job lines' ranges) whose ids appear in `id`'s chunks.
fn workers_uploading(h: &H, id: &str) -> usize {
    let ranges: Vec<(u64, u64)> = job_lines(h).iter().map(range_of).collect();
    let ids = chunk_ids(h, id);
    ranges.iter().filter(|(a, _)| ids.iter().any(|(x, _)| x >= a && ranges.iter().filter(|(b, _)| b > a).all(|(b, _)| x < b))).count()
}

fn crew_log(h: &H) -> String {
    h.agent_log().lines().filter(|l| l.contains("crew [") || l.contains("tuning [")).collect::<Vec<_>>().join("\n")
}

#[test]
fn two_workers_serve_one_rolling_lease_each_on_its_own_ids() {
    let mut h = harness(Some(2), false);
    h.push(crew_lease(&h, "l_C1", &fake(25, json!([1.0, 1.3])), json!(8), 400));
    h.start();
    let done = h.wait_done("l_C1");
    assert_eq!(done["given_back"], false, "{done}\n{}", crew_log(&h));
    assert!(done.get("refused").is_none());
    ids_accounted(&h, "l_C1", &done);
    assert_eq!(done["stats"]["released"], 400, "the block used up");
    // (beats: the crew's workers and games in flight in all while it runs)
    let beats: Vec<Value> = h.mock.with(|s| s.beats.iter().filter(|(l, _)| l == "l_C1").map(|(_, b)| b["stats"].clone()).collect());
    assert!(beats.iter().any(|b| b["workers"] == 2 && b["inflight"] == 8), "{beats:?}");
    let lines = job_lines(&h);
    // two at once, then a new one each time a member started all of its ids
    assert!(lines.len() >= 3, "{}", crew_log(&h));
    assert!(lines.iter().all(|j| j["conc"] == 4 && j["rolling"] == true), "the run's 8 games in flight, 4 a worker: {lines:?}");
    let dirs: BTreeSet<&str> = lines.iter().map(|j| j["out_dir"].as_str().unwrap()).collect();
    assert_eq!(dirs.len(), lines.len(), "a results directory per worker: {lines:?}");
    // the ranges as first given never overlap: a member's range is closed, the tail's starts after every released id
    let mut starts: Vec<u64> = lines.iter().map(|j| j["gid_base"].as_u64().unwrap()).collect();
    starts.sort();
    starts.dedup();
    assert_eq!(starts.len(), lines.len(), "{lines:?}");
    assert!(workers_uploading(&h, "l_C1") >= 2, "results of several workers under the lease: {:?}", chunk_ids(&h, "l_C1"));
    assert!(crew_log(&h).contains("started every id of its range"), "{}", crew_log(&h));
    // every extension asks for about twice the crew's games in flight
    let extends: Vec<Value> = h.mock.with(|s| s.extends.iter().filter(|(l, _)| l == "l_C1").map(|(_, b)| b.clone()).collect());
    assert!(!extends.is_empty() && extends.iter().all(|e| e["games"] == 16), "{extends:?}");
    assert!(h.crashes().is_empty(), "{:?}", h.crashes());
    h.stop();
}

#[test]
fn the_crew_pauses_and_resumes_together() {
    let mut h = harness(Some(2), false);
    h.push(crew_lease(&h, "l_CP", &fake(40, json!([1.0, 1.3])), json!(8), 100_000));
    h.start();
    assert!(wait(|| job_lines(&h).len() >= 2), "{}", crew_log(&h));
    assert!(wait(|| workers_uploading(&h, "l_CP") >= 2), "{:?}", chunk_ids(&h, "l_CP"));
    // the computer is used (work only while it is idle): every worker stops
    h.user_active(true);
    assert!(wait(|| h.phase() == farm_agent::status::Phase::Paused));
    std::thread::sleep(Duration::from_millis(700));
    let n = h.mock.results_for("l_CP").len();
    let fins = h.agent_log().matches("rolling job").count();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(h.mock.results_for("l_CP").len(), n, "nothing is played while paused: {}", crew_log(&h));
    assert_eq!(h.agent_log().matches("rolling job").count(), fins);
    // idle again: all of them go on
    h.user_active(false);
    let before = chunk_ids(&h, "l_CP").len();
    assert!(wait(|| chunk_ids(&h, "l_CP").len() > before + 4), "work goes on");
    let lines = job_lines(&h);
    let after: Vec<(u64, u64)> = chunk_ids(&h, "l_CP")[before..].to_vec();
    let owners: BTreeSet<u64> = after.iter().filter_map(|(x, _)| lines.iter().map(range_of).filter(|(a, _)| a <= x).map(|(a, _)| a).max()).collect();
    assert!(owners.len() >= 2, "both workers play again: {after:?} {lines:?}");
    h.stop();
    let done = h.wait_done("l_CP");
    ids_accounted(&h, "l_CP", &done);
}

/// A crew of two whose worker finishing game `gid` panics: the lease goes on and ends as any, the crashed worker's games
/// in flight go back (never credited, never played by another), the crash is reported once, a worker took its place.
fn crash_in_crew(id: &str, game_ms: u64, gid: u64, lost: u64) -> H {
    let mut h = harness(Some(2), false);
    let mut fk = fake(game_ms, json!([1.0, 1.3]));
    fk["crash_gid"] = json!(gid);
    h.push(crew_lease(&h, id, &fk, json!(8), 200));
    h.start();
    let done = h.wait_done(id);
    assert_eq!(done["given_back"], false, "the lease went on and ended as any: {done}
{}", crew_log(&h));
    ids_accounted(&h, id, &done);
    assert_eq!(done["stats"]["released"], 200, "{done}");
    let (credited, _) = credits(&h, id);
    assert!(credited >= 150, "the other workers and the replacement played on: {credited}");
    let u = done["unfinished"]["gids"].as_array().cloned().unwrap_or_default();
    assert!(u.iter().any(|r| r[0].as_u64().unwrap() <= lost && r[1].as_u64().unwrap() > lost), "{done}");
    assert!(!chunk_ids(&h, id).iter().any(|&(a, b)| a <= lost && lost < b), "id {lost} was never credited");
    let crashes = h.crashes();
    assert_eq!(crashes.len(), 1, "reported once: {crashes:?}");
    assert_eq!(crashes[0]["kind"], "panic", "{crashes:?}");
    assert!(job_lines(&h).len() >= 3, "a worker took its place: {}", crew_log(&h));
    h
}

/// Agent 0.3.2: a worker that hasn't built its sessions yet answers `status` with `provider: ""` (zero-core's default).
/// On a machine where GPU work isn't allowed (every Mac) 0.3.1 took that for a GPU job: it yielded the main worker
/// right after its start ("GPU is busy") and the worker stayed suspended, its sessions never built, until the
/// coordinator took the lease back - a crew of two (fixed, or the tuner's) pings its main worker at once, and one worker
/// is pinged too when quiet. A job is what the agent chose for it (here the CPU) until the worker names a provider.
#[test]
fn workers_without_sessions_yet_are_never_taken_for_gpu_jobs() {
    for (name, workers, tune) in [("a crew of two", Some(2), false), ("one worker", None, false), ("tuned", None, true)] {
        let mut h = harness(workers, tune);
        h.signals.set(|s| s.gpu = None); // no GPU signal: GPU work isn't allowed here
        let mut fk = fake(25, json!([1.0, 1.3]));
        fk["build_ms"] = json!(1500);
        h.push(crew_lease(&h, "l_CB", &fk, json!(8), 300));
        h.start();
        let done = h.wait_done("l_CB");
        assert_eq!(done["given_back"], false, "{name}: {done}
{}", crew_log(&h));
        assert_eq!(done["stats"]["yields"], 0, "{name}: never yielded: {done}");
        assert_eq!(done["stats"]["paused_ms"], 0, "{name}: {done}");
        assert_eq!(done["stats"]["released"], 300, "{name}: the block used up: {done}");
        ids_accounted(&h, "l_CB", &done);
        assert!(h.agent_log().contains("sessions built (cpu)"), "{name}: {}", h.agent_log());
        if workers == Some(2) {
            assert!(job_lines(&h).len() >= 2 && workers_uploading(&h, "l_CB") >= 2, "{name}: {}", crew_log(&h));
        }
        assert!(h.crashes().is_empty(), "{name}: {:?}", h.crashes());
        h.stop();
    }
}

#[test]
fn a_crew_member_crashing_in_its_range_is_replaced_and_the_others_go_on() {
    // the first worker keeps its range [0, 24) beside the tail from its first extension on; game 12 is in its middle
    let mut h = crash_in_crew("l_CX", 100, 12, 13);
    assert!(h.crashes()[0]["message"].as_str().unwrap().contains("crew worker failed"), "{:?}", h.crashes());
    h.stop();
}

#[test]
fn a_crew_member_crashing_near_the_end_of_its_range_is_reported() {
    // game 22: the first worker started all of its ids [0, 24) two games before - it is finishing its games then (or
    // about to: either way the crash is reported once and its games in flight go back)
    let mut h = crash_in_crew("l_CF", 100, 22, 23);
    h.stop();
}

#[test]
fn the_crews_tail_crashing_is_replaced_and_the_others_go_on() {
    // the second worker is the tail from id 24 (the first extension made the released ids [0, 24)); game 24 is its first,
    // long before the first worker started all of its ids (when a third would take the tail's place)
    let mut h = crash_in_crew("l_CTX", 100, 24, 25);
    assert!(h.crashes()[0]["message"].as_str().unwrap().contains("crew main worker failed"), "{:?}", h.crashes());
    assert!(h.agent_log().contains("fake crash at game Some(24)"), "{}", h.agent_log());
    h.stop();
}

#[test]
fn a_new_generation_switches_the_whole_crew() {
    let mut h = harness(Some(2), false);
    let fk = fake(40, json!([1.0, 1.3]));
    h.push(crew_lease(&h, "l_CG", &fk, json!(8), 100_000));
    h.start();
    assert!(wait(|| job_lines(&h).len() >= 2 && workers_uploading(&h, "l_CG") >= 2), "{}", crew_log(&h));
    let (_, d1) = network(&h, 1);
    let (c2, d2) = network(&h, 2);
    let dref2 = h.define(&definition(&h, 2, json!({"1": d1, "2": d2}), &fk));
    h.mock.set_run_current(RUN, c2, dref2);
    assert!(wait(|| h.agent_log().matches("checkpoint Some(2) self-tested (sha256 given: true)").count() >= 2), "both workers switch: {}", h.agent_log());
    // chunks of generation 2 from several workers
    let gen2_owners = || {
        let lines = job_lines(&h);
        let ids: Vec<u64> = h
            .mock
            .results_for("l_CG")
            .iter()
            .filter(|(_, r)| r.headers.get("x-gen").map(String::as_str) == Some("2"))
            .filter_map(|(_, r)| r.headers.get("x-ids").and_then(|v| serde_json::from_str::<Value>(v).ok()))
            .filter_map(|v| v[0][0].as_u64())
            .collect();
        ids.iter().filter_map(|x| lines.iter().map(range_of).filter(|(a, _)| a <= x).map(|(a, _)| a).max()).collect::<BTreeSet<u64>>().len()
    };
    assert!(wait(|| gen2_owners() >= 2), "generation 2 from both workers");
    assert_eq!(h.mock.count("GET", &format!("/v1/artifact/ckpt/{RUN}/2/model.onnx")), 1, "fetched once");
    h.stop();
    let done = h.wait_done("l_CG");
    ids_accounted(&h, "l_CG", &done);
    assert!(h.crashes().is_empty(), "{:?}", h.crashes());
}

#[test]
fn the_memory_plan_holds_the_whole_crew() {
    // too little memory for two workers' bases and games: one worker, whatever the setting
    let mut h = harness(Some(2), false);
    h.machine.lock().unwrap().settings.max_ram_mb = Some(1600);
    h.push(crew_lease(&h, "l_CM1", &fake(40, json!([1.0, 1.3])), json!(8), 100_000));
    h.start();
    assert!(wait(|| !job_lines(&h).is_empty()));
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(job_lines(&h).len(), 1, "one worker: {}", crew_log(&h));
    assert!(job_lines(&h)[0].get("infer_workers").is_none(), "no crew: the fixed rules of before");
    h.stop();

    // enough for two: each worker gets half the job's limit; one that commits past its half runs into it (Windows:
    // its job object; macOS: the agent's cap on its group) and the crew goes on with fewer games each
    let mut h = harness(Some(2), false);
    h.machine.lock().unwrap().settings.max_ram_mb = Some(8000);
    let mut fk = fake(40, json!([1.0, 1.3]));
    fk["mem_per_game_mb"] = json!(1100);
    h.push(crew_lease(&h, "l_CM2", &fk, json!(8), 120));
    h.start();
    let done = h.wait_done("l_CM2");
    assert_eq!(done["given_back"], false, "{done}\n{}", crew_log(&h));
    ids_accounted(&h, "l_CM2", &done);
    assert!(crew_log(&h).contains("memory limit 4000 MB"), "each worker half of the 8,000 MB limit: {}", crew_log(&h));
    let lines = job_lines(&h);
    assert!(lines.iter().any(|j| j["conc"].as_u64().unwrap() < 4), "fewer games a worker after running into the limit: {lines:?}");
    let learned: Value = serde_json::from_slice(&std::fs::read(h.layout.state().join("rolling_memory.json")).unwrap()).unwrap();
    assert!(learned["crew-run/cpu"].as_u64().unwrap() >= 800, "the cost per game remembered: {learned}");
    assert!(h.crashes().len() <= 1, "{:?}", h.crashes());
    h.stop();
}

#[test]
fn the_tuner_chooses_the_worker_count_and_reports_it() {
    let mut h = harness(None, true);
    // the machine plays 30 % more in all with two workers (the M4 Max's measurement), every layout else the same
    h.push(crew_lease(&h, "l_CT", &fake(40, json!([1.0, 1.3])), json!(16), 1_000_000));
    h.start();
    let entry = |h: &H| -> Value {
        std::fs::read(h.layout.state().join("tuning.json")).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).map_or(Value::Null, |v| v["entries"][0].clone())
    };
    let t0 = Instant::now();
    assert!(
        h.mock.wait(Duration::from_secs(150), |_| entry(&h)["state"] == "tuned"),
        "not tuned after {:?}: {}\n{}",
        t0.elapsed(),
        entry(&h),
        crew_log(&h)
    );
    let e = entry(&h);
    assert_eq!(e["start"]["workers"], 1, "Windows (and a small Mac) starts with one: {e}");
    assert_eq!(e["incumbent"]["workers"], 2, "two pay 30 %: {e}\n{}", crew_log(&h));
    assert_eq!(e["incumbent"]["conc"], 8, "the run's 16 games in flight, 8 a worker");
    // the lease goes on with two workers; the next request reports the worker count
    assert!(wait(|| h.status.lock().unwrap().tuning.as_deref().is_some_and(|t| t.starts_with("tuned: 2 workers x ("))), "{:?}", h.status.lock().unwrap().tuning);
    h.stop();
    let done = h.wait_done("l_CT");
    ids_accounted(&h, "l_CT", &done);
    h.push(crew_lease(&h, "l_CT2", &fake(40, json!([1.0, 1.3])), json!(16), 1_000_000));
    let before = job_lines(&h).len();
    h.start();
    assert!(wait(|| job_lines(&h).len() >= before + 2), "the next lease starts with its two workers: {}", crew_log(&h));
    let req = h.mock.with(|s| s.lease_requests.last().cloned().unwrap());
    assert_eq!(req["tuning"][0]["layout"]["workers"], 2, "{req}");
    h.stop();
}

/// What the lease's fakeworkers held at once, from their `holds` / `exits` lines in the worker log (in the order
/// written): (the most MB at once, the most workers at once).
fn held_at_once(h: &H) -> (u64, usize) {
    let mut held: std::collections::BTreeMap<u64, u64> = Default::default();
    let (mut most, mut alive) = (0, 0);
    for l in h.agent_log().lines() {
        let Some(i) = l.find("fakeworker: pid ") else { continue };
        let rest: Vec<&str> = l[i + 16..].split_whitespace().collect();
        let Some(pid) = rest.first().and_then(|p| p.parse::<u64>().ok()) else { continue };
        match rest.get(1).copied() {
            Some("holds") => {
                held.insert(pid, rest.get(2).and_then(|n| n.parse().ok()).unwrap_or(0));
            }
            Some("exits") => {
                held.remove(&pid);
            }
            _ => continue,
        }
        most = most.max(held.values().sum::<u64>());
        alive = alive.max(held.len());
    }
    (most, alive)
}

/// Agent 0.3.3, the M4 Max's crew of 2026-11-19: every handoff left the old workers finishing their games, and with new
/// workers admitted on what they committed at their start (little) 4 drainers and the crew held ~4,700 games in flight,
/// 38.9 GB against a 31.7 GB cap. Now a new worker counts at its plan until it commits more, a drainer at what it
/// commits, a worker starts only when its plan fits beside them, one drain at a time: the lease's workers never hold
/// more than the cap, and never more than a crew and the crew it replaced.
#[test]
fn handoffs_and_drains_never_hold_more_than_the_memory_cap() {
    let mut h = harness(Some(2), true);
    // the run's cost remembered here: 300 MB a game over the platform's base; each worker of 2 games in flight is planned
    // at base + 600 MB, and its fakeworker grows to exactly that (its base at the start, 300 MB a finished game, twice)
    let base = farm_agent::rolling::cpu_cost().base_mb;
    let plan = base + 2 * 300;
    std::fs::create_dir_all(h.layout.state()).unwrap();
    std::fs::write(h.layout.state().join("rolling_memory.json"), json!({format!("{RUN}/cpu"): 300}).to_string()).unwrap();
    // a cap whose 85 % holds three workers' plans, not four: a crew of two and one more beside it
    let limit = (3.2 * plan as f64 / farm_agent::rolling::PLAN_SHARE).ceil() as u64;
    h.machine.lock().unwrap().settings.max_ram_mb = Some(limit);
    let mut fk = fake(150, json!([1.0, 1.3]));
    fk["rate"]["best"] = json!({"threads": 2});
    fk["rate"]["penalty"] = json!(0.25);
    (fk["mem_base_mb"], fk["mem_grow_mb"], fk["mem_grow_until"]) = (json!(base), json!(300), json!(2));
    h.push(crew_lease(&h, "l_CMD", &fk, json!(4), 1_000_000));
    h.start();
    let handoffs = |h: &H| h.agent_log().matches("tuning [l_CMD]: handing the job over to").count();
    // (polled: `workers_uploading` reads the mock's state, which `mock.wait` holds while it asks)
    assert!(mockcoord::testkit::wait_until(Duration::from_secs(150), || handoffs(&h) >= 3 && workers_uploading(&h, "l_CMD") >= 3), "{}", crew_log(&h));
    h.stop();
    let done = h.wait_done("l_CMD");
    ids_accounted(&h, "l_CMD", &done);
    let (most, alive) = held_at_once(&h);
    eprintln!("held at once: {most} MB, {alive} workers; cap {limit} MB, plan {plan} MB, {} handoffs", handoffs(&h));
    assert!(most >= 2 * plan, "the crew ran at its plan: {most} MB");
    assert!(most <= limit, "{most} MB at once against the {limit} MB cap
{}", crew_log(&h));
    assert!(alive <= 4, "{alive} workers at once: a crew and the one it replaced at most
{}", crew_log(&h));
    assert!(h.crashes().is_empty(), "{:?}", h.crashes());
}

/// Agent 0.3.3, the same crew, the handoff no trial measures (the revert to the incumbent after a discarded trial; 0.3.2
/// also handed over unmeasured whenever a trial couldn't start at spawn time): the next crew's first worker starts
/// beside the old crew - finishing its games for a long while, at its full memory - and its second only once its plan
/// fits beside them. 0.3.2 admitted the second on what the first committed right after its start (its games not grown
/// yet) and the lease ended up holding four workers' memory against a cap that holds three.
#[test]
fn an_unmeasured_handoff_starts_the_next_crew_only_as_memory_allows() {
    let mut h = harness(Some(2), true);
    // the owner's computer: work goes on while it is used, trials don't; trials last long enough to be interrupted
    h.machine.lock().unwrap().settings.mode = farm_agent::settings::Mode::Always;
    h.timings.tune_window = Duration::from_secs(8);
    // the run's cost remembered here, each worker of 2 games in flight planned at base + 2 x that: on Windows the games
    // weigh twice the base (as a real worker's grown games do), so that 0.3.2's rule - the old crew's commit, the first
    // new worker's (its base so far) and a plan within 85 % - admits a fourth worker while 4 plans pass the cap (3.95);
    // a Mac's fakeworkers touch every page they hold: lighter there
    let base = farm_agent::rolling::cpu_cost().base_mb;
    let per = if cfg!(windows) { base } else { 300 };
    let plan = base + 2 * per;
    std::fs::create_dir_all(h.layout.state()).unwrap();
    std::fs::write(h.layout.state().join("rolling_memory.json"), json!({format!("{RUN}/cpu"): per}).to_string()).unwrap();
    let limit = (3.95 * plan as f64) as u64;
    h.machine.lock().unwrap().settings.max_ram_mb = Some(limit);
    // games of 4 s (a drain of its 2 games in flight takes up to 8 s); a worker commits its base at once and its games'
    // memory over 1.5 s
    let mut fk = fake(4000, json!([1.0, 1.3]));
    fk["rate"]["best"] = json!({"threads": 2});
    fk["rate"]["penalty"] = json!(0.25);
    (fk["mem_base_mb"], fk["mem_per_game_mb"], fk["mem_ramp_ms"]) = (json!(base), json!(per), json!(1500));
    h.push(crew_lease(&h, "l_CMU", &fk, json!(4), 1_000_000));
    h.start();
    // a candidate's trial with its whole crew, its memory grown: then the owner uses the computer
    let trial_crew = |h: &H| {
        h.agent_log().contains("tuning [l_CMU]: handing the job over to")
            && h.status.lock().unwrap().tuning.as_deref().is_some_and(|t| t.starts_with("measuring") && !t.contains("workers running"))
    };
    assert!(mockcoord::testkit::wait_until(Duration::from_secs(90), || trial_crew(&h)), "{}", crew_log(&h));
    std::thread::sleep(Duration::from_millis(1800));
    h.user_active(true);
    let revert = |h: &H| h.agent_log().contains("(the incumbent, not measured)");
    assert!(wait(|| revert(&h)), "the incumbent back, unmeasured: {}", crew_log(&h));
    // the old crew's drain (up to 8 s) and the new crew complete
    let since = job_lines(&h).len();
    assert!(mockcoord::testkit::wait_until(Duration::from_secs(40), || job_lines(&h).len() >= since + 2), "{}", crew_log(&h));
    std::thread::sleep(Duration::from_secs(3));
    h.stop();
    let done = h.wait_done("l_CMU");
    ids_accounted(&h, "l_CMU", &done);
    let (most, alive) = held_at_once(&h);
    eprintln!("held at once: {most} MB, {alive} workers; cap {limit} MB, plan {plan} MB");
    assert!(most <= limit, "{most} MB at once against the {limit} MB cap
{}", crew_log(&h));
    assert!(alive <= 3, "{alive} workers at once: the old crew and one new worker beside it at most
{}", crew_log(&h));
    assert!(h.crashes().is_empty(), "{:?}", h.crashes());
}

/// Agent 0.3.3, the same crew: its memory plan cut each worker to 882 games in flight, and macOS's 192-row floor then
/// left 4 groups (and 3 after a memory restart) - the slow region (PROTOCOL 13: 4 groups starve the search; the measured
/// best was 6 a worker). Now the groups stay where memory cuts the games and the batches shrink (down to 96 rows on
/// macOS): on a 16-core Mac at the validation's 31,717 MB cap, 2 x 691 games in flight in 6 groups of 116 rows.
#[test]
fn a_memory_capped_crew_keeps_its_groups_and_batches_shrink() {
    let limit = 31_717u64;
    let mut h = harness(Some(2), false);
    h.machine.lock().unwrap().settings.max_ram_mb = Some(limit);
    // (a 64 GB machine with most of it free: the cap is the setting's, not the free memory's)
    h.signals.set(|s| (s.mem_avail_mb, s.mem_total_mb) = (60_000, 65_536));
    h.push(crew_lease(&h, "l_CGR", &fake(40, json!([1.0, 1.3])), json!("auto"), 1_000_000));
    h.start();
    assert!(wait(|| job_lines(&h).len() >= 2), "{}", crew_log(&h));
    // what the agent works out (rolling::cpu_shape, tune::Defaults::layout_for, tune::Space::fit), computed here
    let cost = farm_agent::rolling::cpu_cost();
    let (auto_n, auto_g) = farm_agent::rolling::cpu_auto();
    let one = farm_agent::rolling::ram_cap(limit, cost).min(auto_n);
    let groups = if cfg!(target_os = "macos") { farm_agent::rolling::mac_capped_groups(one, auto_g) } else { auto_g };
    let p = harness::fake_profile();
    let d = farm_agent::tune::defaults(
        farm_agent::tune::Platform::current(),
        &farm_agent::tune::Hw { cores: p.cores, logical: p.threads, perf: p.perf_cores, eff: p.eff_cores, ram_mb: p.ram_mb },
    );
    let want = farm_agent::tune::Space::new(&d, None, Some(2), (one / 2).max(1), one, Some((limit, cost))).fit(d.layout_for(2, one, groups));
    for j in &job_lines(&h)[..2] {
        assert_eq!((j["conc"].as_u64(), j["groups"].as_u64()), (Some(want.conc), Some(want.groups)), "{j}");
    }
    assert_eq!(want.groups, groups.div_ceil(2).min(want.conc / d.batch_min).max(1), "the job's groups shared, not its rows");
    if cfg!(target_os = "macos") && auto_n == 2304 {
        assert_eq!((want.conc, want.groups), (691, 6), "16 cores: 2 x 691 in 6 groups (0.3.2: 2 x 778 in 4)");
        assert!(want.batch() < farm_agent::rolling::MACOS_CPU_BATCH, "batches shrink below 192 rows: {want:?}");
    }
    h.stop();
}
