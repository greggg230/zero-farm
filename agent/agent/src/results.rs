//! `results\pending\<lease_id>\`: results waiting to be uploaded, surviving agent restarts, and
//! the uploader thread that drains them (PROTOCOL 3: idempotent `PUT .../result/{seq}`, then
//! `done`).
//!
//! ```text
//! lease.json            the lease (to compute unfinished ids if the agent died mid-lease)
//! <seq>, <seq>.meta.json  a result not yet acknowledged
//! seqs.jsonl            every result ever produced for the lease (meta per line)
//! dropped.txt           seqs the coordinator refused (410 stale/cancelled, 409, 4xx): not in `done`
//! done.json             the done body, sent once every result is uploaded
//! ```
//!
//! A lease directory without `done.json` that is not the running lease belongs to an agent
//! that stopped mid-lease: once its results are up, it is closed with `given_back: true` and
//! the unfinished ids computed from what was delivered.

use crate::util::{normalize, parse_ranges, ranges_json, subtract, Ranges};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use zerofarm_common::fsutil::{atomic_write, dir_size, remove_dir_all_retry};
use zerofarm_common::http::{Backoff, CallError, Client};
use zerofarm_common::layout::Layout;
use zerofarm_common::protocol::{headers, DoneReq, Lease, Stats};

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ResultMeta {
    pub seq: u64,
    /// `shard` | `records` | `fuzz`
    pub kind: String,
    pub games: u64,
    /// The game ids / seeds this chunk completes (half-open ranges), sent as `x-ids`.
    #[serde(default, alias = "gids")]
    pub ids: Option<Value>,
    /// Self-play: the oldest checkpoint generation in the chunk, sent as `x-gen`.
    #[serde(default)]
    pub gen: Option<u64>,
    pub sha256: String,
    pub size: u64,
    /// Fuzz: the end-of-lease summary (vs one sent per failure as it happens).
    #[serde(default, rename = "final")]
    pub is_final: bool,
}

pub enum Source<'a> {
    File(&'a Path),
    Bytes(&'a [u8]),
}

#[derive(Clone)]
pub struct ResultStore {
    pub layout: Layout,
}

fn is_seq_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 12 && n.bytes().all(|c| c.is_ascii_digit())
}

impl ResultStore {
    pub fn new(layout: Layout) -> Self {
        ResultStore { layout }
    }

    pub fn lease_dir(&self, id: &str) -> PathBuf {
        self.layout.results_pending().join(id)
    }

    pub fn begin(&self, lease: &Lease) -> std::io::Result<()> {
        let d = self.lease_dir(&lease.lease_id);
        std::fs::create_dir_all(&d)?;
        atomic_write(&d.join("lease.json"), &serde_json::to_vec_pretty(lease).expect("json"))
    }

    pub fn metas(&self, id: &str) -> Vec<ResultMeta> {
        std::fs::read_to_string(self.lease_dir(id).join("seqs.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    pub fn next_seq(&self, id: &str) -> u64 {
        self.metas(id).iter().map(|m| m.seq + 1).max().unwrap_or(0)
    }

    /// Stores a result for upload (moving a worker's file into the store). Returns its meta.
    ///
    /// The uploader runs concurrently and picks up files named `<seq>`, so the data appears under
    /// that name last: staged as `<seq>.staging`, meta written, then renamed into place.
    pub fn add(&self, id: &str, mut meta: ResultMeta, src: Source) -> std::io::Result<ResultMeta> {
        let d = self.lease_dir(id);
        std::fs::create_dir_all(&d)?;
        meta.seq = self.next_seq(id);
        let staging = d.join(format!("{}.staging", meta.seq));
        match src {
            Source::File(p) => {
                if std::fs::rename(p, &staging).is_err() {
                    std::fs::copy(p, &staging)?;
                    let _ = std::fs::remove_file(p);
                }
            }
            Source::Bytes(b) => std::fs::write(&staging, b)?,
        }
        let (sha, size) = zerofarm_common::hash::sha256_file(&staging)?;
        meta.sha256 = sha;
        meta.size = size;
        atomic_write(&d.join(format!("{}.meta.json", meta.seq)), &serde_json::to_vec(&meta).expect("json"))?;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(d.join("seqs.jsonl"))?;
        writeln!(f, "{}", serde_json::to_string(&meta).expect("json"))?;
        zerofarm_common::fsutil::rename_retry(&staging, &d.join(meta.seq.to_string()))?;
        Ok(meta)
    }

    /// Seqs the coordinator refused; they are left out of `done`.
    pub fn dropped(&self, id: &str) -> Vec<u64> {
        std::fs::read_to_string(self.lease_dir(id).join("dropped.txt"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect()
    }

    pub fn mark_dropped(&self, id: &str, seq: u64) {
        let d = self.lease_dir(id);
        let _ = std::fs::remove_file(d.join(seq.to_string()));
        let _ = std::fs::remove_file(d.join(format!("{seq}.meta.json")));
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(d.join("dropped.txt")) {
            let _ = writeln!(f, "{seq}");
        }
    }

    /// Results the coordinator should have: every produced seq except dropped ones.
    pub fn delivered_seqs(&self, id: &str) -> Vec<u64> {
        let dropped = self.dropped(id);
        self.metas(id).iter().map(|m| m.seq).filter(|s| !dropped.contains(s)).collect()
    }

    pub fn write_done(&self, id: &str, done: &DoneReq) -> std::io::Result<()> {
        atomic_write(&self.lease_dir(id).join("done.json"), &serde_json::to_vec(done).expect("json"))
    }

    pub fn discard(&self, id: &str) {
        let _ = remove_dir_all_retry(&self.lease_dir(id), Duration::from_secs(5));
    }

    pub fn lease_ids(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.layout.results_pending())
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                    .filter_map(|e| e.file_name().to_str().map(String::from))
                    .filter(|n| zerofarm_common::is_safe_id(n))
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    /// Bytes of results not yet acknowledged (DESIGN 3.5: past 200 MB no new self-play leases).
    pub fn pending_bytes(&self) -> u64 {
        dir_size(&self.layout.results_pending())
    }

    fn unsent(&self, id: &str) -> Vec<u64> {
        let mut v: Vec<u64> = std::fs::read_dir(self.lease_dir(id))
            .map(|rd| rd.flatten().filter_map(|e| e.file_name().to_str().filter(|n| is_seq_name(n)).and_then(|n| n.parse().ok())).collect())
            .unwrap_or_default();
        v.sort();
        v
    }
}

/// Which games (or fuzz seeds) of a lease were not delivered (the lease's kind decides; the
/// coordinator's job never does).
pub fn compute_unfinished(lease: &Lease, metas: &[ResultMeta]) -> Option<Value> {
    let job = &lease.job;
    match lease.kind.as_str() {
        "selfplay" | "eval" => {
            let base = job.get("gid_base").and_then(Value::as_u64)?;
            let games = job.get("games").and_then(Value::as_u64)?;
            let delivered: Ranges = normalize(metas.iter().flat_map(|m| m.ids.as_ref().map(parse_ranges).unwrap_or_default()).collect());
            let rest = subtract((base, base.checked_add(games)?), &delivered);
            (!rest.is_empty()).then(|| json!({"gids": ranges_json(&rest)}))
        }
        "fuzz" => {
            if metas.iter().any(|m| m.is_final) {
                return None;
            }
            let first = job.get("first").and_then(Value::as_u64)?;
            let count = job.get("count").and_then(Value::as_u64)?;
            Some(json!({"seeds": [[first, first.checked_add(count)?]]}))
        }
        _ => None,
    }
}

#[derive(Default)]
struct Shared {
    active: Mutex<Option<String>>,
    /// Lease directories that existed when this agent started: only these can be orphans of a
    /// previous agent process. Leases created since are closed by the engine's `done.json`.
    orphans: Mutex<std::collections::HashSet<String>>,
    wake: Mutex<bool>,
    cv: Condvar,
    stop: AtomicBool,
    revoked: AtomicBool,
}

pub struct Uploader {
    shared: Arc<Shared>,
    done_rx: Option<std::sync::mpsc::Receiver<()>>,
}

enum Step {
    Done,
    Retry(CallError),
    Revoked,
}

impl Uploader {
    pub fn start(store: ResultStore, client: Client, rescan: Duration) -> Uploader {
        let shared = Arc::new(Shared::default());
        *shared.orphans.lock().unwrap() = store.lease_ids().into_iter().collect();
        let s2 = shared.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("uploader".into())
            .spawn(move || {
                run(&store, &client, &s2, rescan);
                let _ = tx.send(());
            })
            .expect("thread");
        Uploader { shared, done_rx: Some(rx) }
    }

    pub fn wake(&self) {
        *self.shared.wake.lock().unwrap() = true;
        self.shared.cv.notify_all();
    }

    /// The lease currently running (its directory is not closed by the uploader).
    pub fn set_active(&self, id: Option<&str>) {
        *self.shared.active.lock().unwrap() = id.map(String::from);
        self.wake();
    }

    pub fn revoked(&self) -> bool {
        self.shared.revoked.load(Ordering::SeqCst)
    }

    /// Stops the thread, waiting up to `wait` for an in-flight request to finish.
    pub fn stop(&mut self, wait: Duration) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.wake();
        if let Some(rx) = self.done_rx.take() {
            let _ = rx.recv_timeout(wait);
        }
    }
}

impl Drop for Uploader {
    fn drop(&mut self) {
        self.stop(Duration::from_secs(2));
    }
}

fn run(store: &ResultStore, client: &Client, sh: &Shared, rescan: Duration) {
    let mut backoff = Backoff::default();
    let stopped = || sh.stop.load(Ordering::SeqCst);
    while !stopped() {
        let mut failure = None;
        for id in store.lease_ids() {
            if stopped() {
                return;
            }
            match process(store, client, sh, &id) {
                Step::Done => {}
                Step::Retry(e) => {
                    log::warn!("upload for lease {id} failed: {e}; will retry");
                    failure = Some(e);
                    break;
                }
                Step::Revoked => {
                    log::error!("the coordinator no longer accepts this machine's token");
                    sh.revoked.store(true, Ordering::SeqCst);
                    failure = Some(CallError::Stopped);
                    break;
                }
            }
        }
        let wait = match &failure {
            Some(e) => backoff.delay_for(e),
            None => {
                backoff.reset();
                rescan
            }
        };
        // On failure, honour the backoff (wakes don't shorten it); otherwise wake on demand.
        let mut woken = sh.wake.lock().unwrap();
        if failure.is_none() && *woken {
            *woken = false;
            continue;
        }
        let end = std::time::Instant::now() + wait;
        loop {
            if stopped() {
                return;
            }
            let now = std::time::Instant::now();
            if now >= end {
                break;
            }
            let (g, _) = sh.cv.wait_timeout(woken, end - now).unwrap();
            woken = g;
            if *woken && failure.is_none() {
                break;
            }
        }
        *woken = false;
    }
}

fn process(store: &ResultStore, client: &Client, sh: &Shared, id: &str) -> Step {
    let dir = store.lease_dir(id);
    for seq in store.unsent(id) {
        let mpath = dir.join(format!("{seq}.meta.json"));
        let fpath = dir.join(seq.to_string());
        let meta: ResultMeta = match std::fs::read(&mpath).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
            Some(m) => m,
            None => {
                // Written but its meta never landed (killed in between): re-derive.
                let (sha, size) = match zerofarm_common::hash::sha256_file(&fpath) {
                    Ok(x) => x,
                    Err(_) => continue,
                };
                ResultMeta { seq, kind: "shard".into(), sha256: sha, size, ..Default::default() }
            }
        };
        let bytes = match std::fs::read(&fpath) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let mut hdrs: Vec<(&str, String)> = vec![(headers::SHA256, meta.sha256.clone())];
        if let Some(ids) = &meta.ids {
            hdrs.push((headers::IDS, serde_json::to_string(ids).unwrap_or_default()));
        }
        if let Some(g) = meta.gen {
            hdrs.push((headers::GEN, g.to_string()));
        }
        match client.put_bytes(&format!("/v1/lease/{id}/result/{seq}"), &bytes, &hdrs) {
            Ok(v) => {
                let r: zerofarm_common::protocol::ResultResp = serde_json::from_value(v).unwrap_or_default();
                if r.already_credited > 0 {
                    log::info!("result {id}/{seq}: {} ids were already credited elsewhere", r.already_credited);
                }
                let _ = std::fs::remove_file(&fpath);
                let _ = std::fs::remove_file(&mpath);
            }
            Err(e) if e.is_revoked() => return Step::Revoked,
            // The coordinator refuses the token: keep everything and try again later.
            Err(e) if e.is_unauthorized() => return Step::Retry(e),
            Err(e) if e.status() == Some(404) => {
                log::info!("lease {id} is unknown at the coordinator ({e}); dropping its results");
                store.discard(id);
                return Step::Done;
            }
            Err(e) if e.is_retryable() => return Step::Retry(e),
            Err(e) => {
                // 410: lease cancelled or a stale self-play chunk; 409: other bytes for this seq;
                // other 4xx: refused. Drop this chunk only and leave it out of `done`.
                log::warn!("result {id}/{seq} refused ({e}); dropping it");
                store.mark_dropped(id, seq);
            }
        }
    }
    if sh.active.lock().unwrap().as_deref() == Some(id) {
        return Step::Done;
    }
    let done_path = dir.join("done.json");
    let done: DoneReq = match std::fs::read(&done_path).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
        Some(d) => d,
        // Not closed by this process and not left by an earlier one: still being set up or
        // finished; its `done.json` comes from the engine.
        None if !sh.orphans.lock().unwrap().contains(id) => return Step::Done,
        None => {
            let metas = store.metas(id);
            let lease: Option<Lease> = std::fs::read(dir.join("lease.json")).ok().and_then(|b| serde_json::from_slice(&b).ok());
            let unfinished = lease.as_ref().and_then(|l| compute_unfinished(l, &metas));
            log::info!("closing lease {id} left over from an earlier run ({} results)", metas.len());
            DoneReq { results: store.delivered_seqs(id), stats: Stats::new(), given_back: true, unfinished, refused: None }
        }
    };
    let mut done = done;
    // Stats leave the PC redacted (M5), whichever agent version wrote this done.json.
    done.stats = crate::lease::redacted(done.stats);
    for attempt in 0..3 {
        let dropped = store.dropped(id);
        done.results.retain(|s| !dropped.contains(s));
        match client.post_json::<_, Value>(&format!("/v1/lease/{id}/done"), &done) {
            // rev 2: results the coordinator never received. Upload them if we still have them;
            // otherwise they are lost: leave them out (their ids get reissued) and ask again.
            Err(CallError::Http { status: 409, body: Some(b), .. }) if b.get("missing").is_some() && attempt < 2 => {
                let missing: Vec<u64> = b["missing"].as_array().map(|a| a.iter().filter_map(Value::as_u64).collect()).unwrap_or_default();
                if missing.iter().any(|s| dir.join(s.to_string()).exists()) {
                    return Step::Retry(CallError::Network("results still to upload".into()));
                }
                log::warn!("lease {id}: the coordinator is missing results {missing:?} that are gone here; leaving them out");
                for s in missing {
                    store.mark_dropped(id, s);
                }
                continue;
            }
            other => return finish_done(store, id, other),
        }
    }
    Step::Done
}

fn finish_done(store: &ResultStore, id: &str, r: Result<Value, CallError>) -> Step {
    match r {
        Ok(v) => {
            log::info!("lease {id} closed ({} ids returned to the pool)", v.get("returned").and_then(Value::as_u64).unwrap_or(0));
            store.discard(id);
            Step::Done
        }
        Err(e) if e.is_revoked() => Step::Revoked,
        Err(e) if e.is_unauthorized() => Step::Retry(e),
        Err(e) if matches!(e.status(), Some(404 | 410)) => {
            store.discard(id);
            Step::Done
        }
        Err(e) if e.is_retryable() => Step::Retry(e),
        Err(e) => {
            log::error!("done for lease {id} refused ({e}); dropping it");
            store.discard(id);
            Step::Done
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(job: Value) -> Lease {
        serde_json::from_value(json!({"lease_id": "l_1", "kind": job["kind"], "issued_at": 0, "expires_at": 0, "beat_s": 60,
            "slack_s": 600, "build": {"build_id": "x", "manifest": "m"}, "job": job}))
        .unwrap()
    }

    #[test]
    fn unfinished_from_delivered_gids() {
        let l = lease(json!({"kind": "selfplay", "games": 10, "gid_base": 100}));
        let m = |g: Value| ResultMeta { ids: Some(g), ..Default::default() };
        assert_eq!(compute_unfinished(&l, &[]), Some(json!({"gids": [[100, 110]]})));
        assert_eq!(compute_unfinished(&l, &[m(json!([[100, 103]])), m(json!([[105, 107]]))]), Some(json!({"gids": [[103, 105], [107, 110]]})));
        assert_eq!(compute_unfinished(&l, &[m(json!([[100, 110]]))]), None);
        let f = lease(json!({"kind": "fuzz", "first": 5, "count": 10}));
        assert_eq!(compute_unfinished(&f, &[]), Some(json!({"seeds": [[5, 15]]})));
        assert_eq!(compute_unfinished(&f, &[ResultMeta { is_final: true, ..Default::default() }]), None);
    }

    #[test]
    fn store_assigns_seqs_and_persists_metas() {
        let d = tempfile::tempdir().unwrap();
        let s = ResultStore::new(Layout::new(d.path()));
        let l = lease(json!({"kind": "selfplay", "games": 4, "gid_base": 0}));
        s.begin(&l).unwrap();
        let src = d.path().join("x.bin");
        std::fs::write(&src, b"abc").unwrap();
        let m0 = s.add("l_1", ResultMeta { kind: "shard".into(), games: 2, ..Default::default() }, Source::File(&src)).unwrap();
        let m1 = s.add("l_1", ResultMeta { kind: "fuzz".into(), ..Default::default() }, Source::Bytes(b"{}")).unwrap();
        assert_eq!((m0.seq, m1.seq), (0, 1));
        assert!(!src.exists(), "moved into the store");
        assert_eq!(m0.sha256, zerofarm_common::hash::sha256_hex(b"abc"));
        assert_eq!(s.unsent("l_1"), vec![0, 1]);
        assert_eq!(s.metas("l_1").len(), 2);
        assert_eq!(s.lease_ids(), vec!["l_1".to_string()]);
    }
}
