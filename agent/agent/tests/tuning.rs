//! Layout tuning (rev 19, PROTOCOL 13) with the real engine, the real fakeworker (its `rate` model: leaves per second
//! by layout) and the mock coordinator: trials inside a rolling lease (handoffs, no id played twice or lost), the
//! result remembered and used by the next lease, the coordinator's report and hint, a trial discarded when the
//! computer is used (the incumbent back, the trial retried later), no trials while other programs load the CPU, and
//! autotune off.

mod harness;

use harness::{wait, H};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const RUN: &str = "tune-run";

/// A rolling lease with network 1 and a definition whose settings carry `fake` (a rate model).
fn tuning_lease(h: &H, id: &str, fake: Value, inflight: Value, hint: Option<Value>) -> Value {
    let (m, r) = (b"onnx graph tune".to_vec(), b"reference rows tune".to_vec());
    let (ml, rl) = (m.len(), r.len());
    let model = format!("ckpt/{RUN}/1/model.onnx");
    let reference = format!("ckpt/{RUN}/1/reference.npz");
    let sha = h.mock.add_artifact(&model, m);
    let rsha = h.mock.add_artifact(&reference, r);
    let def = json!({"definition": 1, "kind": "run", "id": RUN, "version": 1, "created": zerofarm_common::now_unix() - 60,
        "expires": null, "game": "fake", "builds": [h.build_id], "runtime": null,
        "checkpoints": {"1": {"model_sha256": sha, "model_size": ml, "reference_sha256": rsha, "reference_size": rl}},
        "jobs": {"selfplay": {"settings": {"sims": 8, "fake": fake}}}});
    let dref = h.define(&def);
    let mut l = h.lease(id, json!({"kind": "selfplay", "gid_base": 0, "games": 64, "seed": 7}));
    l["run"] = json!(RUN);
    l["definition"] = dref;
    l["checkpoint"] = json!({"gen": 1, "model": model, "sha256": sha, "reference": reference, "reference_sha256": rsha});
    l["rolling"] = json!({"block": [0, 1_000_000], "inflight": {"cpu": inflight, "gpu": "auto"},
                          "until": zerofarm_common::now_unix() + 7200, "max_s": 7200, "flush_s": 10});
    if let Some(x) = hint {
        l["tuning_hint"] = x;
    }
    l
}

/// The fake's rate model: fastest with few search threads and batches of 8 rows.
fn fake(rate: Value) -> Value {
    json!({"game_ms": 40, "shard_every": 4, "flush_ms": 300, "status_ms": 200, "rate": rate})
}

fn peaked() -> Value {
    json!({"leaves_s": 1000, "best": {"threads": 2, "batch": 8}, "penalty": 0.25})
}

fn tuning_json(h: &H) -> Value {
    std::fs::read(h.layout.state().join("tuning.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(json!({}))
}

fn entry(h: &H) -> Value {
    tuning_json(h)["entries"].as_array().and_then(|a| a.first().cloned()).unwrap_or(Value::Null)
}

/// The agent's tuning lines in the worker log (trials, handoffs, discards), for failure messages.
fn tuning_log(h: &H) -> String {
    h.agent_log().lines().filter(|l| l.contains("tuning [") || l.contains("leaves/s at this layout")).collect::<Vec<_>>().join("
")
}

/// Every job line the fakeworkers logged.
fn job_lines(h: &H) -> Vec<Value> {
    (0..).map_while(|n| h.job_line_n(n)).collect()
}

fn layout_of(j: &Value) -> (u64, u64, u64, u64, u64) {
    let n = |k: &str| j[k].as_u64().unwrap_or(0);
    (n("threads"), n("infer_workers"), n("infer_threads"), n("groups"), n("conc"))
}

fn layout_json(l: &Value) -> (u64, u64, u64, u64, u64) {
    layout_of(l)
}

/// No id was played twice (the coordinator credited every chunk's ids for the first time) and every released id is
/// credited or reported unfinished.
fn ids_accounted(h: &H, id: &str, done: &Value) {
    let (credited, again) = h.mock.with(|s| s.credits.iter().filter(|(l, ..)| l == id).fold((0, 0), |(c, a), (_, _, x, y)| (c + x, a + y)));
    assert_eq!(again, 0, "an id played twice: {done}");
    let unfinished: u64 = done["unfinished"]["gids"].as_array().map_or(0, |u| u.iter().map(|r| r[1].as_u64().unwrap() - r[0].as_u64().unwrap()).sum());
    let released = done["stats"]["released"].as_u64().unwrap();
    assert_eq!(credited + unfinished, released, "{done}");
}

/// The harness, its fake worker never counted as starved by the test machine's own load (it sleeps); one worker per
/// job (these tests search one worker's layout; several workers, PROTOCOL 13.5: crew.rs).
fn harness() -> H {
    let mut h = H::new();
    h.signals.real_starvation = false;
    h.machine.lock().unwrap().settings.workers = Some(1);
    h
}

#[test]
fn tunes_inside_a_rolling_lease_remembers_it_and_reports_it() {
    let mut h = harness();
    h.push(tuning_lease(&h, "l_T1", fake(peaked()), json!(16), None));
    h.start();
    assert!(
        h.mock.wait(Duration::from_secs(120), |_| entry(&h)["state"] == "tuned"),
        "not tuned: {}\n{}",
        tuning_json(&h),
        h.agent_log()
    );
    let e = entry(&h);
    let rate = |l: &Value| -> f64 {
        let ms: Vec<f64> = e["measured"].as_array().unwrap().iter().filter(|m| m["layout"] == *l).map(|m| m["rate"].as_f64().unwrap()).collect();
        ms.iter().sum::<f64>() / ms.len().max(1) as f64
    };
    let (start, best) = (e["start"].clone(), e["incumbent"].clone());
    assert_ne!(start, best, "the fake's peak is elsewhere: {e}
{}", tuning_log(&h));
    assert!(rate(&best) >= rate(&start) * 1.03, "{e}");
    if cfg!(windows) {
        // (macOS keeps batches of 96-384 rows: 16 games in flight are one group there)
        assert_eq!(best["groups"], 2, "batches of 8 rows (16 in flight, the run's number, in 2 groups): {e}");
    }
    assert!(best["threads"].as_u64().unwrap() < start["threads"].as_u64().unwrap(), "fewer search threads: {e}");
    assert_eq!(best["conc"], 16, "the run's number is kept");
    assert!(e["trials"].as_u64().unwrap() <= 9, "{e}");
    // every trial ran as a worker of its own: the job lines carry the layouts
    let lines = job_lines(&h);
    assert!(lines.len() >= 3, "{}", h.agent_log());
    // never past a cap: search threads and inference threads each at most the cores (8)
    assert!(
        lines.iter().all(|j| j["conc"] == 16 && j["threads"].as_u64().unwrap() <= 8 && j["infer_workers"].as_u64().unwrap() * j["infer_threads"].as_u64().unwrap() <= 8),
        "{lines:?}"
    );
    // the lease goes on with the tuned layout, nothing played twice
    assert!(wait(|| job_lines(&h).last().map(layout_of) == Some(layout_json(&best))), "the main worker runs the tuned layout: {:?}", job_lines(&h).last());
    h.stop();
    let done = h.wait_done("l_T1");
    ids_accounted(&h, "l_T1", &done);
    assert!(h.crashes().is_empty(), "{:?}", h.crashes());

    // the next lease of the same key starts with the tuned layout and runs no trial; its request reports it
    let before = job_lines(&h).len();
    h.push(tuning_lease(&h, "l_T2", fake(peaked()), json!(16), None));
    h.start();
    assert!(wait(|| job_lines(&h).len() > before));
    std::thread::sleep(Duration::from_secs(4));
    let lines = job_lines(&h);
    assert_eq!(lines.len(), before + 1, "no trial once tuned: {:?}", &lines[before..]);
    assert_eq!(layout_of(&lines[before]), layout_json(&best));
    let req = h.mock.with(|s| s.lease_requests.last().cloned().unwrap());
    let rep = &req["tuning"][0];
    assert_eq!((rep["run"].clone(), rep["build"].clone(), rep["state"].clone()), (json!(RUN), json!(h.build_id), json!("tuned")), "{req}");
    assert_eq!(rep["layout"], best);
    assert!(rep["leaves_s"].as_f64().unwrap() > 0.0 && rep["net"].as_str().unwrap().len() == 16);
    assert!(req["features"].as_array().unwrap().contains(&json!("tuning")));
    h.stop();
}

#[test]
fn a_trial_is_discarded_when_the_computer_is_used_and_retried_later() {
    let mut h = harness();
    // the owner's computer: work goes on while it is used, trials don't
    h.machine.lock().unwrap().settings.mode = farm_agent::settings::Mode::Always;
    let mut fk = fake(peaked());
    fk["game_ms"] = json!(60);
    h.push(tuning_lease(&h, "l_TD", fk, json!(16), None));
    h.start();
    // wait for a measured trial of a candidate (not the start)
    let trial_line = |h: &H| h.agent_log().lines().filter(|l| l.contains("tuning [l_TD]: handing the job over to") && l.contains("(a measured trial)")).count();
    assert!(wait(|| trial_line(&h) >= 1), "{}", h.agent_log());
    let n_jobs = job_lines(&h).len();
    assert!(wait(|| job_lines(&h).len() > n_jobs), "the trial's worker started");
    let trial = layout_of(job_lines(&h).last().unwrap());
    std::thread::sleep(Duration::from_millis(400));
    h.user_active(true);
    assert!(wait(|| h.agent_log().contains("is discarded: the computer is in use")), "{}", h.agent_log());
    // back to the incumbent while the computer is used, and no trial meanwhile
    let incumbent = layout_json(&entry(&h)["incumbent"]);
    assert!(wait(|| job_lines(&h).last().map(layout_of) == Some(incumbent)), "the incumbent is back: {:?} vs {incumbent:?}", job_lines(&h).last());
    let trials = trial_line(&h);
    std::thread::sleep(Duration::from_secs(5));
    assert_eq!(trial_line(&h), trials, "no trial starts while the computer is used");
    let e = entry(&h);
    let discards = e["discards"].as_array().unwrap();
    assert!(discards.iter().any(|d| layout_json(&d[0]) == trial && d[1] == 1), "{e}");
    // idle again: the trial is retried
    h.user_active(false);
    assert!(
        wait(|| job_lines(&h).iter().filter(|j| layout_of(j) == trial).count() >= 2),
        "the discarded trial runs again: {:?}",
        job_lines(&h).iter().map(layout_of).collect::<Vec<_>>()
    );
    h.stop();
    let done = h.wait_done("l_TD");
    ids_accounted(&h, "l_TD", &done);
}

#[test]
fn no_trial_while_other_programs_load_the_cpu() {
    let mut h = harness();
    h.signals.set_other_load(Some(0.5));
    h.push(tuning_lease(&h, "l_TL", fake(peaked()), json!(16), None));
    h.start();
    assert!(wait(|| !job_lines(&h).is_empty()));
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(4) {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!h.agent_log().contains("measured"), "nothing is measured on a busy machine: {}", h.agent_log());
    assert!(h.agent_log().contains("is discarded: other programs used 50 %"), "the lease's first worker's measurement: {}", h.agent_log());
    assert_eq!(job_lines(&h).len(), 1, "no handoff to a trial");
    h.signals.set_other_load(Some(0.0));
    assert!(wait(|| h.agent_log().contains("measured")), "quiet again: trials go on: {}", h.agent_log());
    h.stop();
}

#[test]
fn the_coordinators_hint_is_the_starting_point() {
    let mut h = harness();
    let hint = json!({"layout": {"threads": 3, "infer_workers": 1, "infer_threads": 5, "groups": 2, "conc": 16}, "leaves_s": 900, "machines": 2});
    h.push(tuning_lease(&h, "l_TH", fake(peaked()), json!(16), Some(hint)));
    h.start();
    assert!(wait(|| !job_lines(&h).is_empty()));
    // (fitted to this machine: macOS keeps batches of 96-384 rows, so 16 games in flight are one group)
    let want = (3, 1, 5, if cfg!(windows) { 2 } else { 1 }, 16);
    assert_eq!(layout_of(&job_lines(&h)[0]), want, "the hint's layout first");
    assert!(wait(|| entry(&h)["hint"] == true), "{}", tuning_json(&h));
    assert_eq!(layout_json(&entry(&h)["start"]), want);
    h.stop();
    // a hint out of bounds is ignored (the platform's start instead)
    let mut h = harness();
    let bad = json!({"layout": {"threads": 0, "infer_workers": 1, "infer_threads": 5, "groups": 2, "conc": 16}});
    h.push(tuning_lease(&h, "l_TB", fake(peaked()), json!(16), Some(bad)));
    h.start();
    assert!(wait(|| !job_lines(&h).is_empty()));
    assert_ne!(job_lines(&h)[0]["threads"], 0);
    assert!(wait(|| entry(&h)["hint"] == false), "{}", tuning_json(&h));
    h.stop();
}

#[test]
fn autotune_off_keeps_the_fixed_rules() {
    let mut h = harness();
    h.machine.lock().unwrap().settings.autotune = false;
    h.push(tuning_lease(&h, "l_TO", fake(peaked()), json!(16), None));
    h.start();
    assert!(wait(|| !job_lines(&h).is_empty()));
    std::thread::sleep(Duration::from_secs(3));
    let lines = job_lines(&h);
    assert_eq!(lines.len(), 1, "no trial");
    let j = &lines[0];
    assert!(j.get("infer_workers").is_none() && j.get("infer_threads").is_none(), "{j}");
    assert_eq!((j["threads"].clone(), j["conc"].clone(), j["groups"].clone()), (json!(8), json!(16), json!(1)), "physical cores, the run's number, the template's groups");
    assert!(!h.layout.state().join("tuning.json").exists());
    assert!(h.status.lock().unwrap().tuning.is_none());
    h.stop();
}

#[test]
fn a_trial_is_discarded_when_work_pauses_and_the_incumbent_comes_back_after() {
    // the default policy: no work at all while the computer is used
    let mut h = harness();
    let mut fk = fake(peaked());
    fk["game_ms"] = json!(60);
    h.push(tuning_lease(&h, "l_TP", fk, json!(16), None));
    h.start();
    let trials = |h: &H| h.agent_log().lines().filter(|l| l.contains("tuning [l_TP]: handing the job over to") && l.contains("(a measured trial)")).count();
    assert!(wait(|| trials(&h) >= 1), "{}", tuning_log(&h));
    let n = job_lines(&h).len();
    assert!(wait(|| job_lines(&h).len() > n), "the trial's worker started");
    let trial = layout_of(job_lines(&h).last().unwrap());
    std::thread::sleep(Duration::from_millis(400));
    h.user_active(true);
    assert!(wait(|| h.agent_log().contains("tuning [l_TP]: the trial of")), "{}", tuning_log(&h));
    let incumbent = layout_json(&entry(&h)["incumbent"]);
    assert_ne!(trial, incumbent);
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(job_lines(&h).last().map(layout_of), Some(trial), "nothing starts while work is paused");
    h.user_active(false);
    assert!(wait(|| job_lines(&h).last().map(layout_of) == Some(incumbent)), "the incumbent comes back once work goes on: {}", tuning_log(&h));
    h.stop();
    let done = h.wait_done("l_TP");
    ids_accounted(&h, "l_TP", &done);
}

/// The layouts (threads, sessions, threads per session, groups, games in flight) of every job line from the `n`th on.
fn layouts_from(h: &H, n: usize) -> Vec<(u64, u64, u64, u64, u64)> {
    job_lines(h)[n..].iter().map(layout_of).collect()
}

/// Agent 0.3.3, the M4 Max's crew of 2026-11-19: the remembered entry held a layout from a lease planned with less memory
/// per game (2 x 958 games in flight, its trial discarded) while the next lease's memory plan held only 2 x 778. 0.3.2
/// recorded the fitted layout's measurements and discards as another layout's, so the incumbent stayed "unmeasured" and
/// the job was handed over to the same layout again and again (3 times in 7 minutes, nothing measured). Now the
/// remembered layout is compared as it runs: the lease's first worker measures it, and the search goes on from there.
#[test]
fn a_remembered_layout_the_memory_no_longer_holds_is_measured_once_not_handed_over_to_again() {
    let mut h = harness();
    // a first lease with room for the run's 16 games in flight leaves the key's entry
    h.machine.lock().unwrap().settings.max_ram_mb = Some(8000);
    h.push(tuning_lease(&h, "l_TS1", fake(peaked()), json!(16), None));
    h.start();
    assert!(h.mock.wait(Duration::from_secs(60), |_| entry(&h)["measured"].as_array().is_some_and(|m| !m.is_empty())), "{}", tuning_json(&h));
    h.stop();
    let start = entry(&h)["start"].clone();
    assert_eq!(start["conc"], 16);
    // as 0.3.2 left it: the incumbent never measured, discarded once, still searching
    let mut t = tuning_json(&h);
    let e = &mut t["entries"][0];
    e["incumbent"] = start.clone();
    e["measured"] = json!([]);
    e["discards"] = json!([[start.clone(), 1]]);
    (e["trials"], e["state"], e["phase"], e["momentum"]) = (json!(0), json!("searching"), Value::Null, Value::Null);
    e.as_object_mut().unwrap().remove("space");
    std::fs::write(h.layout.state().join("tuning.json"), serde_json::to_vec_pretty(&t).unwrap()).unwrap();
    // the next lease's memory holds fewer games in flight: (0.85 x 1,400 - base) / cost a game (13 on Windows, 9 on a Mac)
    let limit = 1400u64;
    h.machine.lock().unwrap().settings.max_ram_mb = Some(limit);
    let cost = farm_agent::rolling::cpu_cost();
    let room = ((limit as f64 * farm_agent::rolling::PLAN_SHARE) as u64 - cost.base_mb) / cost.per_game_mb;
    assert!(room < 16, "{room}");
    let before = job_lines(&h).len();
    h.push(tuning_lease(&h, "l_TS2", fake(peaked()), json!(16), None));
    h.start();
    assert!(
        h.mock.wait(Duration::from_secs(120), |_| entry(&h)["state"] == "tuned"),
        "not tuned: {}
{}",
        tuning_json(&h),
        tuning_log(&h)
    );
    let layouts = layouts_from(&h, before);
    let first = layouts[0];
    assert_eq!(first.4, room, "the remembered layout as the memory holds it: {layouts:?}");
    // its first worker measured it - the incumbent's measurement - and no worker ran it again (no discards here)
    assert_eq!(layouts.iter().filter(|l| **l == first).count(), 1, "handed over to the same layout again: {layouts:?}
{}", tuning_log(&h));
    let e = entry(&h);
    let m0 = &e["measured"][0];
    assert_eq!((m0["layout"]["conc"].as_u64(), m0["layout"]["threads"].clone()), (Some(room), start["threads"].clone()), "{e}");
    assert!(e["trials"].as_u64().unwrap() <= 9 && layouts.len() <= 11, "{e}
{layouts:?}");
    assert!(m0["load"].as_f64().is_some(), "the load is recorded with the measurement: {e}");
    // (both leases play the same block of ids: only the first one's are accounted for here)
    h.stop();
}

/// Agent 0.3.3: a real Mac's background daemons burst (a sync daemon ~55 % of a core, syncthing to ~90 %): 0.3.2
/// discarded a trial on any sample over 10 % of the CPUs, so every trial died within seconds of a burst. Short bursts
/// now pass (the load is judged as a mean over the trial and before it), sustained load still discards.
#[test]
fn bursty_background_load_doesnt_discard_every_trial_but_sustained_load_does() {
    let mut h = harness();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let bursts = {
        let (signals, stop) = (h.signals.clone(), stop.clone());
        // 25 % of the CPUs for 0.2 s every second (5 % on average), 0 otherwise
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                signals.set_other_load(Some(0.25));
                std::thread::sleep(Duration::from_millis(200));
                if stop.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }
                signals.set_other_load(Some(0.0));
                std::thread::sleep(Duration::from_millis(800));
            }
        })
    };
    let mut fk = fake(peaked());
    fk["game_ms"] = json!(60);
    h.push(tuning_lease(&h, "l_TB", fk, json!(16), None));
    h.start();
    let measured = |h: &H| h.agent_log().lines().filter(|l| l.contains("tuning [l_TB]:") && l.contains(" measured ")).count();
    assert!(wait(|| measured(&h) >= 3), "trials count despite the bursts: {}", tuning_log(&h));
    let discarded = h.agent_log().lines().filter(|l| l.contains("tuning [l_TB]: the trial of") && l.contains("other programs")).count();
    assert!(discarded <= 1, "a burst discards no trial: {}", tuning_log(&h));
    assert!(h.agent_log().contains("(other programs "), "each measurement says its load: {}", tuning_log(&h));
    // sustained load during a trial: discarded
    let running = |h: &H| h.status.lock().unwrap().tuning.as_deref().is_some_and(|t| t.starts_with("measuring"));
    assert!(wait(|| running(&h) || entry(&h)["state"] == "tuned"), "{}", tuning_log(&h));
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    bursts.join().unwrap();
    if running(&h) {
        h.signals.set_other_load(Some(0.5));
        assert!(wait(|| h.agent_log().lines().any(|l| l.contains("tuning [l_TB]: the trial of") && l.contains("is discarded: other programs used"))), "{}", tuning_log(&h));
        let n = measured(&h);
        std::thread::sleep(Duration::from_secs(3));
        assert_eq!(measured(&h), n, "nothing is measured under sustained load: {}", tuning_log(&h));
    }
    h.signals.set_other_load(Some(0.0));
    h.stop();
    let done = h.wait_done("l_TB");
    ids_accounted(&h, "l_TB", &done);
}
