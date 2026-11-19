//! Rolling self-play leases (rev 15, PROTOCOL 10) with the real engine, the real fakeworker and the mock
//! coordinator: more ids on `low` (`POST .../extend`), `wait_s`, the coordinator's `finish`, a new generation with
//! its definition verified and switched to in place, a changed template or a bad definition ending the lease
//! gracefully, an old worker running the lease as a classic one, an agent update finishing it, a lost lease, and
//! classic leases unchanged.

mod harness;

use farm_agent::engine::EngineExit;
use harness::{wait, H};
use mockcoord::testkit::{bin, make_agent_package, sign_definition};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const RUN: &str = "roll-run";

/// Serves a network (model + reference) for `gen`: (lease checkpoint, definition entry).
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

fn definition(h: &H, version: u64, checkpoints: Value, settings: Value) -> Value {
    json!({"definition": 1, "kind": "run", "id": RUN, "version": version, "created": zerofarm_common::now_unix() - 60,
           "expires": null, "game": "fake", "builds": [h.build_id], "runtime": null, "checkpoints": checkpoints,
           "jobs": {"selfplay": {"settings": settings}}})
}

fn settings(game_ms: u64) -> Value {
    json!({"sims": 8, "fake": {"game_ms": game_ms, "shard_every": 2, "flush_ms": 500}})
}

/// Network 1 and definition version 1 (signing it, with `settings(game_ms)`): (checkpoint, definition reference).
fn generation_1(h: &H, game_ms: u64) -> (Value, Value) {
    let (c1, d1) = network(h, 1);
    let dref = h.define(&definition(h, 1, json!({"1": d1}), settings(game_ms)));
    (c1, dref)
}

/// A rolling lease of ids `[base, hi)`, `games` released, `cpu` games in flight on the CPU.
fn rolling_lease(h: &H, id: &str, (ckpt, dref): &(Value, Value), base: u64, games: u64, hi: u64, cpu: Value) -> Value {
    let mut l = h.lease(id, json!({"kind": "selfplay", "gid_base": base, "games": games, "seed": 7}));
    l["run"] = json!(RUN);
    l["definition"] = dref.clone();
    l["checkpoint"] = ckpt.clone();
    l["rolling"] = json!({"block": [base, hi], "inflight": {"cpu": cpu, "gpu": "auto"},
                          "until": zerofarm_common::now_unix() + 7200, "max_s": 7200, "flush_s": 10});
    l
}

fn extends(h: &H, id: &str) -> Vec<Value> {
    h.mock.with(|s| s.extends.iter().filter(|(l, _)| l == id).map(|(_, b)| b.clone()).collect())
}

/// Every id the coordinator credited to `id`'s chunks.
fn credited(h: &H, id: &str) -> u64 {
    h.mock.with(|s| s.credits.iter().filter(|(l, ..)| l == id).map(|(_, _, c, _)| *c).sum())
}

/// How many ids `done` reports unfinished.
fn unfinished_n(done: &Value) -> u64 {
    done["unfinished"]["gids"].as_array().map_or(0, |u| u.iter().map(|r| r[1].as_u64().unwrap() - r[0].as_u64().unwrap()).sum())
}

/// Every released id is either credited or reported unfinished: `finish` (the block used up) starts no new game, so
/// a released id not started yet (fewer than the games in flight) goes back.
fn all_accounted(h: &H, id: &str, done: &Value, released: u64, games_in_flight: u64) {
    let (c, u) = (credited(h, id), unfinished_n(done));
    assert_eq!(c + u, released, "{done}");
    assert!(u < games_in_flight, "at most the unstarted ids go back: {done}");
}

fn beats(h: &H, id: &str) -> Vec<Value> {
    h.mock.with(|s| s.beats.iter().filter(|(l, _)| l == id).map(|(_, b)| b.clone()).collect())
}

fn x_gens(h: &H, id: &str) -> Vec<u64> {
    h.mock.results_for(id).iter().filter_map(|(_, r)| r.headers.get("x-gen").and_then(|g| g.parse().ok())).collect()
}

fn definition_crashes(h: &H) -> usize {
    h.crashes().iter().filter(|c| c["kind"] == "definition").count()
}

#[test]
fn extends_on_low_until_the_block_is_used_up() {
    let mut h = H::new();
    let g1 = generation_1(&h, 100);
    h.push(rolling_lease(&h, "l_R1", &g1, 1000, 4, 1016, json!(2)));
    h.start();
    let done = h.wait_done("l_R1");
    assert_eq!(done["given_back"], false, "{done}");
    assert!(done.get("refused").is_none(), "{done}");
    // 4 released at grant, then 4 more three times, then the block is used up: `finish`
    assert_eq!(extends(&h, "l_R1"), vec![json!({"games": 4}); 4], "about 2 x the games in flight each time");
    all_accounted(&h, "l_R1", &done, 16, 2);
    assert!(h.agent_log().contains("rolling job ended (finish)"), "{}", h.agent_log());
    let j = h.job_line();
    assert_eq!((j["rolling"].clone(), j["flush_s"].clone(), j["games"].clone(), j["gid_base"].clone()), (json!(true), json!(10), json!(4), json!(1000)), "{j}");
    assert_eq!((j["conc"].clone(), j["groups"].clone()), (json!(2), json!(1)), "the run's CPU games in flight, the template's groups");
    assert_eq!((done["stats"]["inflight"].clone(), done["stats"]["released"].clone()), (json!(2), json!(16)), "{done}");
    let b = beats(&h, "l_R1");
    assert!(!b.is_empty() && b.iter().all(|b| b["gen"] == 1), "rolling beats carry the generation: {b:?}");
    assert_eq!(h.mock.with(|s| s.lease_requests[0]["features"].clone()), json!(["rolling", "tuning"]), "rev 19: tuning hints too");
    assert!(h.crashes().is_empty(), "{:?}", h.crashes());
    h.stop();
}

#[test]
fn wait_s_is_honoured_before_asking_again() {
    let mut h = H::new();
    let g1 = generation_1(&h, 100);
    h.push(rolling_lease(&h, "l_RW", &g1, 0, 4, 8, json!(2)));
    h.mock.script_extend("l_RW", json!({"wait_s": 1}));
    h.start();
    assert!(wait(|| !extends(&h, "l_RW").is_empty()));
    let t0 = Instant::now();
    assert!(wait(|| extends(&h, "l_RW").len() >= 2));
    assert!(t0.elapsed() >= Duration::from_millis(800), "asked again after {:?}, before wait_s", t0.elapsed());
    let done = h.wait_done("l_RW");
    assert_eq!(done["given_back"], false, "{done}");
    all_accounted(&h, "l_RW", &done, 8, 2);
    h.stop();
}

#[test]
fn the_coordinators_finish_ends_the_lease_gracefully() {
    let mut h = H::new();
    let g1 = generation_1(&h, 150);
    h.push(rolling_lease(&h, "l_RF", &g1, 0, 4, 100, json!(2)));
    h.mock.script_extend("l_RF", json!({"finish": true, "reason": "max_s"}));
    h.start();
    let done = h.wait_done("l_RF");
    assert_eq!(done["given_back"], false, "{done}");
    assert_eq!(extends(&h, "l_RF").len(), 1);
    assert!(h.agent_log().contains("rolling job ended (finish)"), "{}", h.agent_log());
    // the released ids not finished, inside the released ones
    if let Some(u) = done["unfinished"]["gids"].as_array() {
        assert!(u.iter().all(|r| r[0].as_u64().unwrap() >= 2 && r[1].as_u64().unwrap() <= 4), "{done}");
    }
    all_accounted(&h, "l_RF", &done, 4, 2);
    h.stop();
}

#[test]
fn a_new_generation_with_a_verified_definition_is_switched_to_in_place() {
    let mut h = H::new();
    let g1 = generation_1(&h, 100);
    h.push(rolling_lease(&h, "l_RG", &g1, 0, 4, 40, json!(2)));
    h.start();
    assert!(wait(|| !h.mock.results_for("l_RG").is_empty()));
    // the trainer publishes gen 2: definition version 2 signs gens 1 and 2 with the same template
    let (_, d1) = network(&h, 1);
    let (c2, d2) = network(&h, 2);
    let dref2 = h.define(&definition(&h, 2, json!({"1": d1, "2": d2}), settings(100)));
    h.mock.set_run_current(RUN, c2, dref2);
    assert!(wait(|| beats(&h, "l_RG").iter().any(|b| b["gen"] == 2)), "{}", h.agent_log());
    assert!(h.agent_log().contains("checkpoint Some(2) self-tested (sha256 given: true)"), "{}", h.agent_log());
    let done = h.wait_done("l_RG");
    assert_eq!(done["given_back"], false, "{done}");
    all_accounted(&h, "l_RG", &done, 40, 2);
    let gens = x_gens(&h, "l_RG");
    assert!(gens.contains(&1) && gens.contains(&2), "chunks of both generations: {gens:?}");
    assert!(gens.windows(2).all(|w| w[0] <= w[1]), "never back to the old network: {gens:?}");
    assert!(h.job_line_n(1).is_none(), "switched in place: one worker, one job line");
    assert_eq!(h.mock.count("GET", &format!("/v1/artifact/ckpt/{RUN}/2/model.onnx")), 1, "fetched once, whatever the repeats");
    let seen: Value = serde_json::from_slice(&std::fs::read(h.layout.state().join("definitions.json")).unwrap()).unwrap();
    assert_eq!(seen, json!({"run/roll-run": 2}), "the new version is remembered");
    assert!(h.crashes().is_empty(), "{:?}", h.crashes());
    assert_eq!(done["stats"]["selftest"]["2"]["ok"], true, "{done}");
    h.stop();
}

#[test]
fn a_changed_template_finishes_the_lease() {
    let mut h = H::new();
    let g1 = generation_1(&h, 100);
    h.push(rolling_lease(&h, "l_RT", &g1, 0, 4, 100_000, json!("auto")));
    h.start();
    assert!(wait(|| !h.mock.results_for("l_RT").is_empty()));
    let (_, d1) = network(&h, 1);
    let (c2, d2) = network(&h, 2);
    let mut other = settings(100);
    other["sims"] = json!(16);
    let dref2 = h.define(&definition(&h, 2, json!({"1": d1, "2": d2}), other));
    h.mock.set_run_current(RUN, c2, dref2);
    let done = h.wait_done("l_RT");
    assert_eq!(done["given_back"], false, "{done}");
    assert!(h.agent_log().contains("rolling job ended (finish)"), "{}", h.agent_log());
    assert_eq!(h.mock.count("GET", &format!("/v1/artifact/ckpt/{RUN}/2/")), 0, "the new network is never fetched");
    assert_eq!(definition_crashes(&h), 0, "a new template is no fault");
    // "auto" on the CPU: 32 in flight in 4 groups (macOS: per core, rev 17), never more than fits the memory cap
    // (rolling.rs)
    let j = h.job_line();
    let (conc, groups) = (j["conc"].as_u64().unwrap(), j["groups"].as_u64().unwrap());
    let (auto_n, auto_g) = farm_agent::rolling::cpu_auto();
    assert!((1..=auto_n).contains(&conc) && groups == conc.min(auto_g), "{j}");
    h.stop();
}

#[test]
fn a_definition_that_fails_a_check_finishes_the_lease_reported_once() {
    let mut h = H::new();
    let g1 = generation_1(&h, 100);
    h.push(rolling_lease(&h, "l_RB", &g1, 0, 4, 100_000, json!(2)));
    h.start();
    assert!(wait(|| !h.mock.results_for("l_RB").is_empty()));
    let (_, d1) = network(&h, 1);
    let (c2, d2) = network(&h, 2);
    // signed with the release key, which never signs a definition
    let (bytes, sig) = sign_definition(&definition(&h, 2, json!({"1": d1, "2": d2}), settings(100)), &h.key.key);
    let dref2 = h.mock.add_definition(bytes, &sig);
    h.mock.set_run_current(RUN, c2, dref2);
    let done = h.wait_done("l_RB");
    assert_eq!(done["given_back"], false, "{done}");
    assert!(done.get("refused").is_none(), "the lease ran: it is finished, not refused: {done}");
    assert_eq!(h.mock.count("GET", &format!("/v1/artifact/ckpt/{RUN}/2/")), 0);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(definition_crashes(&h), 1, "{:?}", h.crashes());
    let seen: Value = serde_json::from_slice(&std::fs::read(h.layout.state().join("definitions.json")).unwrap()).unwrap();
    assert_eq!(seen, json!({"run/roll-run": 1}));
    h.stop();
}

#[test]
fn a_worker_without_the_feature_runs_the_lease_as_a_classic_one() {
    let mut h = H::with_build(&[("startup.json", br#"{"features": []}"#)]);
    let g1 = generation_1(&h, 100);
    h.push(rolling_lease(&h, "l_RO", &g1, 50, 6, 1000, json!(2)));
    h.start();
    let done = h.wait_done("l_RO");
    assert_eq!(done["given_back"], false, "{done}");
    assert!(done["unfinished"].is_null(), "{done}");
    assert_eq!(credited(&h, "l_RO"), 6, "the released ids, then done");
    assert!(extends(&h, "l_RO").is_empty(), "no extension for an old worker");
    let j = h.job_line();
    assert!(j.get("rolling").is_none() && j.get("flush_s").is_none(), "{j}");
    assert_eq!(j["games"], 6);
    assert!(!h.agent_log().contains("unknown op"), "no `extend` sent: {}", h.agent_log());
    h.stop();
}

/// Builds and serves an agent package for `v`.
fn publish(h: &H, v: &str) {
    let dir = h.dir.path().join(format!("pkg-{v}"));
    let launcher = std::fs::read(bin("ZeroFarm")).unwrap();
    let id = make_agent_package(&dir, v, &bin("fakeagent"), &[(zerofarm_common::layout::LAUNCHER_EXE, &launcher)], &h.key.key);
    h.mock.add_build_dir(&dir);
    h.mock.with(|s| s.latest = Some(json!({"version": v, "build_id": id, "manifest": format!("builds/{id}/manifest.json"), "min_agent": "0.1.0"})));
}

#[test]
fn a_ready_agent_update_finishes_the_lease_then_switches() {
    let mut h = H::new();
    h.timings.update_check = Duration::from_secs(1);
    let g1 = generation_1(&h, 100);
    h.push(rolling_lease(&h, "l_RU", &g1, 0, 4, 100_000, json!(2)));
    h.start();
    assert!(wait(|| !h.mock.results_for("l_RU").is_empty()));
    publish(&h, "0.2.0");
    assert!(wait(|| h.finished()), "{}", h.agent_log());
    assert_eq!(h.join(), EngineExit::Update("0.2.0".into()));
    let done = h.mock.done_for("l_RU").expect("done before switching");
    assert_eq!(done["given_back"], false, "{done}");
    assert!(h.agent_log().contains("rolling job ended (finish)"), "{}", h.agent_log());
    assert_eq!(h.layout.pending().as_deref(), Some("0.2.0"));
}

#[test]
fn a_lost_lease_stops_like_an_expired_one() {
    let mut h = H::new();
    let g1 = generation_1(&h, 100);
    h.push(rolling_lease(&h, "l_RL", &g1, 0, 4, 100, json!(2)));
    h.mock.script_extend("l_RL", json!({"status": 410}));
    h.start();
    let done = h.wait_done("l_RL");
    assert_eq!(done["given_back"], true, "{done}");
    assert_eq!(extends(&h, "l_RL").len(), 1);
    h.stop();
}

#[test]
fn classic_leases_are_unchanged() {
    let mut h = H::new();
    let g1 = generation_1(&h, 100);
    let mut l = rolling_lease(&h, "l_RC", &g1, 0, 6, 100, json!(2));
    l.as_object_mut().unwrap().remove("rolling");
    h.push(l);
    h.start();
    let done = h.wait_done("l_RC");
    assert_eq!(done["given_back"], false, "{done}");
    assert_eq!(credited(&h, "l_RC"), 6);
    assert!(extends(&h, "l_RC").is_empty());
    let j = h.job_line();
    for k in ["rolling", "flush_s", "conc", "groups"] {
        assert!(j.get(k).is_none(), "{k} in a classic CPU job line: {j}");
    }
    assert!(beats(&h, "l_RC").iter().all(|b| b.get("gen").is_none()), "classic beats carry no gen");
    assert!(done["stats"].get("released").is_none() && done["stats"].get("inflight").is_none(), "{done}");
    h.stop();
}

// ---- rev 15 (PROTOCOL 10.6): memory. A rolling worker that runs into its job's memory limit is started again with
// fewer games in flight and the lease goes on; one that comes near it finishes its games first; the run's cost per game
// measured on the PC plans its next leases.

/// `settings(game_ms)` with more of the fakeworker's script (its memory).
fn settings_with(game_ms: u64, fake: Value) -> Value {
    let mut s = settings(game_ms);
    for (k, v) in fake.as_object().unwrap() {
        s["fake"][k] = v.clone();
    }
    s
}

/// Network 1 and definition version 1 signing it with `settings`.
fn generation_1_with(h: &H, settings: Value) -> (Value, Value) {
    let (c1, d1) = network(h, 1);
    let dref = h.define(&definition(h, 1, json!({"1": d1}), settings));
    (c1, dref)
}

/// The rolling memory the PC remembers (`state\rolling_memory.json`).
fn learned(h: &H) -> Value {
    std::fs::read(h.layout.state().join("rolling_memory.json")).map(|b| serde_json::from_slice(&b).unwrap()).unwrap_or(json!(null))
}

/// No id credited twice, and every released id credited or reported unfinished.
fn accounted_once(h: &H, id: &str, done: &Value, released: u64) {
    let dup: u64 = h.mock.with(|s| s.credits.iter().filter(|(l, ..)| l == id).map(|(.., a)| *a).sum());
    assert_eq!(dup, 0, "an id credited twice: {done}");
    assert_eq!(credited(h, id) + unfinished_n(done), released, "{done}");
}

/// With a 1,600 MB limit (the CPU cost's 600 MB base and 45 MB per game plan 16 games in flight), a run's 16 games in
/// flight at 100 MB each run into the limit at once.
fn out_of_memory_lease(oom_abort: bool) {
    let mut h = H::new();
    h.machine.lock().unwrap().settings.max_ram_mb = Some(1600);
    let g1 = generation_1_with(&h, settings_with(50, json!({"mem_per_game_mb": 100, "oom_abort": oom_abort})));
    h.push(rolling_lease(&h, "l_RM", &g1, 0, 32, 80, json!(16)));
    h.start();
    let done = h.wait_done("l_RM");
    assert_eq!(done["given_back"], false, "the lease went on and ended as any: {done}");
    let (j0, j1) = (h.job_line(), h.job_line_n(1).expect("a second worker"));
    if cfg!(not(windows)) {
        // macOS (PROTOCOL 11.5): no job object fails the worker's allocations; the agent watches its physical footprint
        // and either finishes its games near the cap (92 %) or kills it past the cap - either way the lease goes on
        // with fewer games in flight, nothing credited twice, the cost per game remembered
        assert_eq!((j0["conc"].clone(), j0["out_dir"].clone()), (json!(16), json!("out")), "{j0}");
        assert!(j1["conc"].as_u64().unwrap() < 16 && j1["out_dir"] == "out-r1", "{j1}");
        accounted_once(&h, "l_RM", &done, 80);
        assert!(learned(&h)["roll-run/cpu"].as_u64().is_some(), "{}", learned(&h));
        assert!(h.crashes().len() <= 1, "{:?}", h.crashes());
        h.stop();
        return;
    }
    assert_eq!((j0["conc"].clone(), j0["gid_base"].clone(), j0["games"].clone(), j0["out_dir"].clone()), (json!(16), json!(0), json!(32), json!("out")), "{j0}");
    // measured (1,600 - 600) / 16 = 63 MB per game: (1,600 x 0.85 - 600) / 63 = 12 (= 3/4 of 16); the first worker's
    // ids were all lost: the second's start after them
    assert_eq!((j1["conc"].clone(), j1["gid_base"].clone(), j1["games"].clone(), j1["out_dir"].clone()), (json!(12), json!(32), json!(0), json!("out-r1")), "{j1}");
    assert!(h.job_line_n(2).is_none(), "one restart: {}", h.agent_log());
    accounted_once(&h, "l_RM", &done, 80);
    let u = done["unfinished"]["gids"].as_array().unwrap();
    assert_eq!(u[0], json!([0, 32]), "the dead worker's ids go back: {done}");
    assert_eq!(done["stats"]["inflight"], 12, "{done}");
    assert_eq!(learned(&h), json!({"roll-run/cpu": 63}));
    let crashes = h.crashes();
    assert_eq!(crashes.len(), 1, "reported once: {crashes:?}");
    assert!(crashes[0]["message"].as_str().unwrap().contains("memory limit"), "{crashes:?}");
    // the run's next lease: its number is capped by the cost measured here
    h.push(rolling_lease(&h, "l_RM2", &g1, 100, 12, 112, json!(16)));
    let done2 = h.wait_done("l_RM2");
    assert_eq!(done2["given_back"], false, "{done2}");
    assert_eq!(h.job_line_n(2).expect("the next lease's job")["conc"], 12);
    assert_eq!(h.crashes().len(), 1);
    h.stop();
}

#[test]
fn a_worker_out_of_memory_is_started_again_with_fewer_games_and_the_lease_goes_on() {
    out_of_memory_lease(false);
}

#[test]
fn a_worker_that_aborts_at_the_memory_limit_is_started_again_too() {
    out_of_memory_lease(true);
}

#[test]
fn a_worker_near_its_memory_limit_finishes_its_games_and_goes_on_with_fewer() {
    let mut h = H::new();
    h.machine.lock().unwrap().settings.max_ram_mb = Some(1600);
    // 8 x 50 MB, then 100 MB more per finished game for its first 11: past 92 % of the limit (1,472 MB) at the 11th
    let g1 = generation_1_with(&h, settings_with(60, json!({"mem_per_game_mb": 50, "mem_grow_mb": 100, "mem_grow_until": 11})));
    h.push(rolling_lease(&h, "l_RH", &g1, 0, 16, 40, json!(8)));
    h.start();
    let done = h.wait_done("l_RH");
    assert_eq!(done["given_back"], false, "{done}");
    let (j0, j1) = (h.job_line(), h.job_line_n(1).expect("a second worker"));
    assert_eq!(j0["conc"], 8, "{j0}");
    let c1 = j1["conc"].as_u64().unwrap();
    assert!((1..=6).contains(&c1), "fewer games in flight, at most 3/4: {j1}");
    // nothing lost: the second worker starts at the first id its predecessor never started
    let first_played = credited(&h, "l_RH");
    let base1 = j1["gid_base"].as_u64().unwrap();
    assert!((11..=16).contains(&base1), "{j1}");
    assert!(h.agent_log().contains("rolling job ended (finish)"), "the first worker finished its games: {}", h.agent_log());
    accounted_once(&h, "l_RH", &done, 40);
    assert!(unfinished_n(&done) < 8, "only ids never started go back: {done}");
    assert!(first_played >= base1);
    let per = learned(&h)["roll-run/cpu"].as_u64().expect("measured");
    // (macOS plans with a larger base in physical footprint, rolling.rs `MACOS_CPU_COST`)
    let base = farm_agent::rolling::cpu_cost().base_mb;
    let least = if cfg!(windows) { 100 } else { (1472 - base) / 8 };
    assert!(per >= least, "(~1,500 - {base}) / 8: {per}");
    assert!(h.crashes().is_empty(), "coming near the limit is no crash: {:?}", h.crashes());
    h.stop();
}
