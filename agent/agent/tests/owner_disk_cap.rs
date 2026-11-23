//! Rev 28 (PROTOCOL 20.7): the farm owner's disk cap per machine, with the real engine and the mock coordinator - a lease
//! answer's `owner_disk_cap_mb` replaces the friend's own `disk_cap_mb` while it is set (the next ask's caps.disk_mb
//! follows it, soon after a "no work" answer), it is kept in machine.json, and the friend's setting applies again once
//! the owner clears it. An answer without the key (a coordinator before rev 28) changes nothing.

mod harness;

use farm_agent::settings::Machine;
use harness::H;
use serde_json::{json, Value};
use std::time::Duration;

fn asks(h: &H) -> Vec<Value> {
    h.mock.with(|s| s.lease_requests.clone())
}

fn wait_asks(h: &H, n: usize, within: Duration) -> bool {
    h.mock.wait(within, |s| s.lease_requests.len() >= n)
}

fn disk_mb(r: &Value) -> u64 {
    r["caps"]["disk_mb"].as_u64().unwrap()
}

#[test]
fn the_owners_disk_cap_replaces_the_friends_until_cleared() {
    let mut h = H::new();
    h.machine.lock().unwrap().settings.disk_cap_mb = 3072;
    // waits are long here (30 s): only the agent's own quick re-ask after a changed cap gets a second ask in time
    h.timings.max_wait = Duration::from_secs(30);
    h.mock.with(|s| {
        s.wait_s = 300;
        s.owner_disk_cap = Some(json!(12288));
    });
    h.start();
    assert!(wait_asks(&h, 2, Duration::from_secs(15)), "a changed cap with no work asks again within seconds: {:?}", asks(&h).len());
    let a = asks(&h);
    assert!(disk_mb(&a[0]) <= 3072, "the first ask is under the friend's cap: {}", a[0]["caps"]);
    assert!(disk_mb(&a[1]) > 3072, "the next ask is under the owner's cap: {}", a[1]["caps"]);
    {
        let m = h.machine.lock().unwrap();
        assert_eq!((m.owner_disk_cap_mb, m.settings.disk_cap_mb), (Some(12288), 3072), "the friend's own setting is kept");
    }
    assert_eq!(Machine::load(&h.layout).owner_disk_cap_mb, Some(12288), "kept in machine.json");
    h.stop();

    // the same cap again: no quick re-ask (the wait is the answer's, cut to max_wait)
    h.timings.max_wait = Duration::from_secs(30);
    h.start();
    assert!(wait_asks(&h, 3, Duration::from_secs(10)));
    assert!(!wait_asks(&h, 4, Duration::from_secs(8)), "an unchanged cap waits as told");
    assert!(disk_mb(&asks(&h)[2]) > 3072, "the cap held across a restart");
    h.stop();

    // an answer without the key (an older coordinator): unchanged
    h.mock.with(|s| s.owner_disk_cap = None);
    h.timings.max_wait = Duration::from_millis(500);
    h.start();
    assert!(wait_asks(&h, 6, Duration::from_secs(15)));
    assert_eq!(h.machine.lock().unwrap().owner_disk_cap_mb, Some(12288));
    h.stop();

    // cleared by the owner: the friend's own cap again
    h.mock.with(|s| s.owner_disk_cap = Some(Value::Null));
    h.start();
    let n = asks(&h).len();
    assert!(wait_asks(&h, n + 2, Duration::from_secs(15)));
    assert_eq!(h.machine.lock().unwrap().owner_disk_cap_mb, None);
    assert!(disk_mb(asks(&h).last().unwrap()) <= 3072, "back under the friend's cap: {}", asks(&h).last().unwrap()["caps"]);
    assert_eq!(Machine::load(&h.layout).owner_disk_cap_mb, None);
    h.stop();
}
