//! The agent against the REAL coordinator (wrangler dev), not the mock. Ignored by default.
//!
//! ```text
//! cd coordinator && npm ci && npm run db:local
//! node node_modules/wrangler/bin/wrangler.js dev --ip 0.0.0.0 --port 5400   (ADMIN_TOKEN in .dev.vars)
//! ZEROFARM_REAL_COORDINATOR=http://127.0.0.1:5400 cargo test -p farm-agent --test real_coordinator -- --ignored --nocapture
//! ```
//!
//! Publishes a signed fake worker build, a run with a checkpoint and a fuzz campaign through the
//! admin API, enrolls with an invite, runs the real engine (scripted idle signals, the fakeworker)
//! and checks through the admin API that every self-play game id and fuzz seed was credited and
//! the failure arrived.

mod harness;

use farm_agent::engine::{Control, Engine};
use farm_agent::settings::Machine;
use farm_agent::signals::SharedSignals;
use farm_agent::status::Status;
use farm_agent::AgentConfig;
use mockcoord::testkit::{bin, make_worker_build, test_key, wait_until};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zerofarm_common::http::Client;
use zerofarm_common::keys::TrustedKeys;
use zerofarm_common::layout::Layout;

fn admin(url: &str) -> Client {
    let token = std::env::var("ZEROFARM_REAL_ADMIN_TOKEN").unwrap_or_else(|_| "dev-admin-token".into());
    Client::new(url, Some(token), "real-coordinator-test").unwrap().without_agent_headers()
}

fn put(c: &Client, key: &str, bytes: &[u8]) {
    c.put_bytes(&format!("/v1/admin/artifact/{key}"), bytes, &[]).unwrap_or_else(|e| panic!("PUT {key}: {e}"));
}

fn post(c: &Client, path: &str, body: Value) -> Value {
    c.post_json::<_, Value>(path, &body).unwrap_or_else(|e| panic!("POST {path}: {e}"))
}

fn put_json(c: &Client, path: &str, body: Value) -> Value {
    c.put_bytes(path, &serde_json::to_vec(&body).unwrap(), &[("content-type", "application/json".into())])
        .unwrap_or_else(|e| panic!("PUT {path}: {e}"))
}

fn credited(rows: &Value) -> u64 {
    rows["results"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|r| r["ids"].as_array().cloned().unwrap_or_default())
        .map(|p| p[1].as_u64().unwrap() - p[0].as_u64().unwrap())
        .sum()
}

#[test]
#[ignore = "needs a running coordinator: set ZEROFARM_REAL_COORDINATOR (see the file header)"]
fn agent_against_the_real_coordinator() {
    let url = std::env::var("ZEROFARM_REAL_COORDINATOR").expect("ZEROFARM_REAL_COORDINATOR is not set");
    let a = admin(&url);
    let tag = format!("{}{}", std::process::id(), zerofarm_common::now_unix() % 100_000);
    let (run, campaign) = (format!("t{tag}"), format!("f{tag}"));

    // A signed fake worker build, uploaded and registered.
    let dir = tempfile::tempdir().unwrap();
    let key = test_key();
    let bdir = dir.path().join("build");
    let build_id = make_worker_build(&bdir, &key.key, &bin("fakeworker"), &[]);
    for f in std::fs::read_dir(&bdir).unwrap().flatten() {
        let name = f.file_name().to_string_lossy().to_string();
        put(&a, &format!("builds/{build_id}/{name}"), &std::fs::read(f.path()).unwrap());
    }
    let reg = post(&a, "/v1/admin/build", json!({"build_id": build_id}));
    assert_eq!(reg["kind"], "worker", "{reg}");

    // A self-play run (12 games, 6 per lease) with checkpoint 1, and a fuzz campaign.
    let model = b"fake onnx model".to_vec();
    let reference = b"fake reference".to_vec();
    put(&a, &format!("ckpt/{run}/1/model.onnx"), &model);
    put(&a, &format!("ckpt/{run}/1/reference.npz"), &reference);
    put_json(
        &a,
        &format!("/v1/admin/run/{run}"),
        json!({"game": "fake", "build_id": build_id, "runtime": null, "priority": 1.0, "state": "active",
               "selfplay": {"quota": 12, "slack": 0.0, "max_stale": 1, "games_per_lease": 6, "gpu": "any",
                            "needs": {"threads": 1, "ram_mb": 100},
                            "job": {"settings": {}, "fake": {"game_ms": 50, "shard_every": 2}}},
               "eval": null}),
    );
    put_json(
        &a,
        &format!("/v1/admin/run/{run}/checkpoint/1"),
        json!({"model": format!("ckpt/{run}/1/model.onnx"), "sha256": zerofarm_common::hash::sha256_hex(&model),
               "reference": format!("ckpt/{run}/1/reference.npz"), "reference_sha256": zerofarm_common::hash::sha256_hex(&reference)}),
    );
    put_json(
        &a,
        &format!("/v1/admin/campaign/{campaign}"),
        json!({"build_id": build_id, "mode": "fake", "seed_base": 5_000_000, "chunk": 3000, "priority": 0.1, "state": "active",
               "env": {"FAKE": "{\"game_ms\": 50, \"chunk\": 1000, \"fail_seeds\": [5000500]}"}}),
    );

    // Enroll with a real invite.
    let codes = post(&a, "/v1/admin/invites", json!({"count": 1, "note": "agent test"}));
    let code = codes["codes"][0].as_str().unwrap().to_string();
    let root = dir.path().join("ZeroFarm");
    let layout = Layout::new(&root);
    let mut machine = Machine::default();
    farm_agent::enroll::enroll(&layout, &url, "0.1.0", &mut machine, &code, "agent test PC", &harness::fake_profile()).unwrap();
    let token = machine.token().unwrap();

    // The real engine, idle signals, fast timings.
    let cfg = AgentConfig {
        layout: layout.clone(),
        coordinator: url.clone(),
        version: "0.1.0".into(),
        build_id: None,
        keys: TrustedKeys::parse(&[key.pubkey.clone()]).unwrap(),
        timings: harness::fast_timings(),
        kinds: vec!["fuzz".into(), "eval".into(), "selfplay".into(), "diag".into()],
        channel: "stable".into(),
        launcher_level: 0,
    };
    let control = Arc::new(Control::default());
    let mut eng = Engine::new(
        cfg,
        token,
        Arc::new(Mutex::new(machine)),
        Arc::new(Mutex::new(Status::default())),
        Box::new(SharedSignals::new(farm_agent::policy::Signals::all_clear(0))),
        control.clone(),
        Box::new(harness::fake_profile),
    )
    .unwrap();
    let t = std::thread::spawn(move || eng.run());

    let get = |p: &str| a.get_json::<Value>(p).unwrap_or_else(|e| panic!("GET {p}: {e}"));
    let ok = wait_until(Duration::from_secs(180), || {
        let r = get(&format!("/v1/admin/run/{run}/results?after=0"));
        let f = get(&format!("/v1/admin/campaign/{campaign}/failures?after=0"));
        let c = get(&format!("/v1/admin/campaign/{campaign}/results?after=0"));
        credited(&r) >= 12 && !f["failures"].as_array().unwrap().is_empty() && credited(&c) >= 3000
    });
    control.request_quit();
    let _ = t.join();
    let r = get(&format!("/v1/admin/run/{run}/results?after=0"));
    let c = get(&format!("/v1/admin/campaign/{campaign}/results?after=0"));
    let f = get(&format!("/v1/admin/campaign/{campaign}/failures?after=0"));
    let rs = get(&format!("/v1/admin/run/{run}"));
    let log = std::fs::read_to_string(layout.logs().join("worker.log")).unwrap_or_default();
    assert!(ok, "run results {r}\ncampaign results {c}\nfailures {f}\nrun {rs}\nworker log:\n{log}");
    // Every game id credited exactly once, all with checkpoint 1.
    assert_eq!(credited(&r), 12, "{r}");
    for row in r["results"].as_array().unwrap() {
        assert_eq!(row["gen"], 1, "{row}");
        assert!(row["dup_ids"].as_array().is_none_or(|d| d.is_empty()), "{row}");
    }
    let fail = &f["failures"][0];
    assert_eq!(fail["seed"], 5_000_500);
    assert_eq!(fail["site"], "engine/src/fake.rs:42:9");
    println!("OK: run {run}: {} chunks, 12 games; campaign {campaign}: {} seeds, failure at {}", r["results"].as_array().unwrap().len(), credited(&c), fail["seed"]);
}
