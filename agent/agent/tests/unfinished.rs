//! 0.2.11 with the real engine and the mock coordinator: a probe the PC never finished (its marker found at start)
//! is reported and holds its pack; the coordinator's evict list (rev 12) empties cache entries at start, before any
//! probe. Agent 0.3.11 (rev 22): an AMD-only PC reports its GPU (DirectML only), and the owner's `gpu_vendors_off` makes
//! it a CPU-only PC again, as 0.2.11-0.3.10 made every AMD PC.
// GPU probes: Windows only (no GPU work on macOS in rev 17)
#![cfg(windows)]

mod harness;

use farm_agent::policy::GpuSignal;
use harness::{wait, H};
use serde_json::{json, Value};
use zerofarm_common::protocol::Gpu;

fn nvidia() -> Gpu {
    Gpu { vendor: "nvidia".into(), name: "RTX 4070 Ti".into(), vram_mb: 12282, driver: "560.94".into(), luid: Some("000000000000d1f3".into()) }
}

fn amd() -> Gpu {
    Gpu { vendor: "amd".into(), name: "RX 9070 XT".into(), vram_mb: 16304, driver: "32.0.21001.9024".into(), luid: Some("00000000000000a1".into()) }
}

fn latest(h: &H, extra: Value) {
    let mut l = json!({"version": h.version, "build_id": "a".repeat(24), "manifest": format!("builds/{}/manifest.json", "a".repeat(24)), "min_agent": "0.1.0"});
    l.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    h.mock.with(|s| s.latest = Some(l));
}

#[test]
fn an_unfinished_probe_is_reported_at_start_and_holds_its_pack() {
    let mut h = H::new();
    h.version = "0.2.11".into();
    h.gpus = vec![nvidia()];
    let key = farm_agent::probe::adapters_key(&h.gpus);
    let marker = json!({"kind": "probe", "runtime": "855d21629a0246f0f91e6790", "adapters": key, "worker": "9127027b129f00b2f1882c6e", "t": 1});
    std::fs::create_dir_all(h.layout.state()).unwrap();
    std::fs::write(h.layout.state().join("probe-inflight.json"), marker.to_string()).unwrap();
    h.start();
    assert!(wait(|| h.crashes().iter().any(|c| c["kind"] == "probe_unfinished")), "{:?}", h.crashes());
    let c = h.crashes().into_iter().find(|c| c["kind"] == "probe_unfinished").unwrap();
    let msg = c["message"].as_str().unwrap();
    assert!(msg.contains("855d21629a0246f0f91e6790") && msg.contains("unfinished 1 time(s), not tried again for a day"), "{msg}");
    assert_eq!(c["build_id"], "9127027b129f00b2f1882c6e");
    assert!(!h.layout.state().join("probe-inflight.json").exists());
    let u = farm_agent::probe::load_unfinished(&h.layout);
    assert_eq!((u.len(), u[0].count), (1, 1));
    // the profile lists the pack with no provider, so the coordinator stops offering it here (the harness's own profile
    // closure doesn't read probes: asked directly)
    let (_, per) = farm_agent::probe::profile_providers(&h.layout, &h.gpus);
    assert_eq!(per.unwrap()["855d21629a0246f0f91e6790"], Vec::<String>::new());
    h.stop();
}

#[test]
fn the_evict_list_empties_cache_entries_at_start() {
    let mut h = H::new();
    h.version = "0.2.11".into();
    let gone = "d27809faeb648b649f2e4e44";
    let kept = "b72aa2385be5362839792668";
    for id in [gone, kept] {
        let d = h.layout.cache_builds().join(id);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("manifest.json"), b"{}").unwrap();
    }
    latest(&h, json!({"evict": [gone, "not-a-build-id", "../../x"]}));
    h.start();
    assert!(wait(|| !h.layout.cache_builds().join(gone).exists()));
    assert!(h.layout.cache_builds().join(kept).exists(), "only what the list names");
    assert!(h.mock.count("GET", "/v1/agent/latest") >= 1);
    h.stop();
}

#[test]
fn an_amd_only_pc_reports_its_gpu_and_may_be_offered_gpu_work() {
    let mut h = H::new();
    h.version = "0.3.11".into();
    h.gpus = vec![amd()];
    h.signals.set(|s| s.gpu = Some(GpuSignal { other_util_pct: 0, temp_c: None }));
    h.start();
    assert!(wait(|| h.mock.with(|s| !s.lease_requests.is_empty())));
    let r = h.mock.with(|s| s.lease_requests.last().cloned().unwrap());
    assert_eq!(r["caps"]["gpu"], false, "nothing probed yet: {r}");
    assert_eq!(r["caps"]["gpu_allowed"], true, "{r}");
    let p = h.mock.with(|s| s.lease_requests.iter().find(|r| !r["profile"].is_null()).cloned()).expect("a profile was sent");
    assert_eq!(p["profile"]["gpus"][0]["vendor"], "amd", "{p}");
    assert_eq!(p["profile"]["gpus"][0]["luid"], "00000000000000a1");
    h.stop();
}

#[test]
fn the_owners_gpu_vendors_off_makes_an_amd_pc_cpu_only_until_it_is_lifted() {
    let mut h = H::new();
    h.version = "0.3.11".into();
    h.gpus = vec![amd(), nvidia()];
    h.timings.update_check = std::time::Duration::from_millis(300);
    h.signals.set(|s| s.gpu = Some(GpuSignal { other_util_pct: 0, temp_c: None }));
    latest(&h, json!({"gpu_vendors_off": ["amd"]}));
    h.start();
    // applied at start, before the first lease request: the AMD GPU is gone from the profile, the NVIDIA one stays
    assert!(wait(|| h.mock.with(|s| s.lease_requests.iter().any(|r| !r["profile"].is_null()))));
    let p = h.mock.with(|s| s.lease_requests.iter().find(|r| !r["profile"].is_null()).cloned().unwrap());
    let vendors: Vec<&str> = p["profile"]["gpus"].as_array().unwrap().iter().filter_map(|g| g["vendor"].as_str()).collect();
    assert_eq!(vendors, ["nvidia"], "{p}");
    assert_eq!(farm_agent::profile::vendors_off(&h.layout), vec!["amd"], "kept through restarts");
    h.stop();
    // an AMD-only PC with AMD off: no usable GPU at all (it keeps the list while the coordinator can't be reached)
    h.gpus = vec![amd()];
    h.mock.with(|s| s.latest = None);
    let n = h.mock.with(|s| s.lease_requests.len());
    h.start();
    assert!(wait(|| h.mock.with(|s| s.lease_requests.len() > n)));
    let r = h.mock.with(|s| s.lease_requests.last().cloned().unwrap());
    assert!(r["caps"]["gpu_allowed"] != json!(true) && r["caps"]["gpu"] == false, "{r}");
    // lifted (the key absent from the coordinator's answer): reported again at the next update check
    latest(&h, json!({}));
    assert!(wait(|| farm_agent::profile::vendors_off(&h.layout).is_empty()));
    assert!(wait(|| h.mock.with(|s| s.lease_requests.last().is_some_and(|r| r["caps"]["gpu_allowed"] == true))), "{:?}", h.mock.with(|s| s.lease_requests.last().cloned()));
    h.stop();
}
