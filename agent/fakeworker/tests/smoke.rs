//! The fake worker speaks aw/1. (Also makes `cargo test --workspace` build the fakeworker and
//! fakeagent binaries that the agent and launcher integration tests run.)

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn events(job: Value, ops_after_hello: &[Value]) -> Vec<Value> {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_fakeworker"))
        .arg("serve")
        .current_dir(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    out.read_line(&mut line).unwrap();
    let hello: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(hello["ev"], "hello");
    assert_eq!(hello["aw"], 1);
    let mut j = job;
    j["op"] = json!("job");
    j["out_dir"] = json!("out");
    writeln!(stdin, "{j}").unwrap();
    for op in ops_after_hello {
        writeln!(stdin, "{op}").unwrap();
    }
    let mut evs = vec![hello];
    loop {
        line.clear();
        if out.read_line(&mut line).unwrap() == 0 {
            break;
        }
        let v: Value = serde_json::from_str(&line).unwrap();
        let end = v["ev"] == "end";
        evs.push(v);
        if end {
            break;
        }
    }
    child.wait().unwrap();
    // Every announced shard exists.
    for e in evs.iter().filter(|e| e["ev"] == "shard") {
        assert!(dir.path().join("out").join(e["path"].as_str().unwrap()).exists());
    }
    evs
}

#[test]
fn selfplay_plays_flushes_and_ends() {
    let evs = events(json!({"kind": "selfplay", "games": 5, "gid_base": 100, "fake": {"game_ms": 5, "shard_every": 2}}), &[]);
    let fins: u64 = evs.iter().filter(|e| e["ev"] == "fin").map(|e| e["n"].as_u64().unwrap()).sum();
    assert_eq!(fins, 5);
    let shards: Vec<&Value> = evs.iter().filter(|e| e["ev"] == "shard").collect();
    assert_eq!(shards.len(), 3);
    assert_eq!(shards[0]["gids"], json!([[100, 102]]));
    assert_eq!(shards[2]["gids"], json!([[104, 105]]));
    let end = evs.last().unwrap();
    assert_eq!(end["reason"], "done");
    assert_eq!(end["unfinished"], Value::Null);
}

#[test]
fn quit_flushes_finished_games_and_reports_unfinished() {
    let evs = events(
        json!({"kind": "selfplay", "games": 50, "gid_base": 0, "fake": {"game_ms": 30, "shard_every": 100}}),
        &[json!({"op": "ping"})],
    );
    // No quit was sent, so this plays out; now with a quit right away:
    assert_eq!(evs.last().unwrap()["reason"], "done");
    let evs = events(json!({"kind": "selfplay", "games": 50, "gid_base": 0, "fake": {"game_ms": 30}}), &[json!({"op": "quit"})]);
    let end = evs.last().unwrap();
    assert_eq!(end["reason"], "quit");
    assert!(end["unfinished"]["gids"].is_array());
}

#[test]
fn fuzz_reports_failures() {
    let evs = events(json!({"kind": "fuzz", "mode": "delver", "first": 1000, "count": 3000,
                            "fake": {"game_ms": 5, "chunk": 1000, "fail_seeds": [1500, 3999, 9999]}}), &[]);
    let fails: Vec<u64> = evs.iter().filter(|e| e["ev"] == "failure").map(|e| e["seed"].as_u64().unwrap()).collect();
    assert_eq!(fails, vec![1500, 3999]);
    assert_eq!(evs.last().unwrap()["reason"], "done");
}
