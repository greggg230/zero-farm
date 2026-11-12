//! An in-process mock of the zero-farm coordinator (PROTOCOL 3 and 4) for tests, plus a small
//! test kit (temporary signing keys, signed fake builds, binary lookup, process guards).
//!
//! It follows PROTOCOL v1 rev 2 and is deliberately strict where the real coordinator is (auth,
//! `x-farm-agent` on `/v1/lease`, result hash checks, `x-ids` inside the lease, first copy of an id
//! wins, fuzz results validated, 410 for cancelled leases and stale chunks, `done` answering 409
//! `{"missing"}`) and scriptable everywhere else: queued leases, scripted beat answers, injected
//! HTTP failures, an agent "latest" answer, `upload_diag` requests, a minimum enroll version.

pub mod testkit;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use tiny_http::{Header, Request, Response, Server};
use zerofarm_common::keys::TrustedKeys;
use zerofarm_common::manifest::{verify_manifest_bytes, MANIFEST_FILE, SIG_FILE};

#[derive(Debug, Clone)]
pub struct Logged {
    pub method: String,
    pub path: String,
    pub agent: Option<String>,
    pub status: u16,
}

#[derive(Debug, Clone)]
pub struct StoredResult {
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub headers: HashMap<String, String>,
}

#[derive(Debug, Clone, Default)]
pub struct LeaseState {
    pub lease: Value,
    pub cancelled: bool,
    pub expired: bool,
    pub done: bool,
}

pub struct FailRule {
    pub method: String,
    pub prefix: String,
    pub remaining: u32,
    pub status: u16,
}

#[derive(Default)]
pub struct State {
    /// This mock's base URL.
    pub url: String,
    pub admin_token: String,
    pub trusted: TrustedKeys,
    pub invites: HashSet<String>,
    /// token -> machine id
    pub machines: HashMap<String, String>,
    pub revoked: HashSet<String>,
    pub artifacts: HashMap<String, Vec<u8>>,
    pub lease_queue: VecDeque<Value>,
    pub wait_s: u64,
    pub update_min_agent: Option<String>,
    pub latest: Option<Value>,
    pub need_profile: bool,
    /// Enroll answers 426 to agents older than this.
    pub enroll_min_agent: Option<String>,
    /// The next lease answer / beat carries `"upload_diag": ["logs"]`.
    pub upload_diag_next: bool,
    pub diag_requested: bool,
    /// Self-play chunks with `x-gen` below this are stale (410).
    pub stale_below_gen: Option<u64>,
    /// Accept result PUTs but don't keep them (to exercise `done`'s 409 `missing`).
    pub forget_results: bool,
    /// Ids credited per scope (run / campaign): first copy wins.
    pub claimed: HashMap<String, Vec<(u64, u64)>>,
    pub credits: Vec<(String, u64, u64, u64)>,
    pub known_digests: HashSet<String>,
    pub beat_script: HashMap<String, VecDeque<Value>>,
    pub leases: HashMap<String, LeaseState>,
    pub lease_requests: Vec<Value>,
    pub beats: Vec<(String, Value)>,
    pub results: HashMap<(String, u64), StoredResult>,
    pub dones: Vec<(String, Value)>,
    pub crashes: Vec<Value>,
    pub diags: Vec<(String, usize)>,
    /// Bodies of user-initiated uploads (`x-farm-diag: user`).
    pub user_diags: Vec<Vec<u8>>,
    /// Answer user-initiated uploads with 429 (retry-after 1800), as after one in the last hour.
    pub user_diag_limited: bool,
    pub enrolls: Vec<Value>,
    pub fail_rules: Vec<FailRule>,
    pub log: Vec<Logged>,
    pub builds_registered: Vec<String>,
    pub agent_releases: Vec<Value>,
}

pub struct MockCoord {
    pub url: String,
    pub state: Arc<Mutex<State>>,
    server: Arc<Server>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for MockCoord {
    fn drop(&mut self) {
        self.server.unblock();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn sha(b: &[u8]) -> String {
    zerofarm_common::hash::hex(&Sha256::digest(b))
}

fn reply(req: Request, status: u16, body: Value) -> u16 {
    let bytes = serde_json::to_vec(&body).unwrap();
    let resp = Response::from_data(bytes)
        .with_status_code(status)
        .with_header(Header::from_bytes(&b"content-type"[..], &b"application/json"[..]).unwrap());
    let _ = req.respond(resp);
    status
}

fn err(req: Request, status: u16, code: &str, msg: &str) -> u16 {
    reply(req, status, json!({"error": code, "message": msg}))
}

impl MockCoord {
    pub fn start() -> MockCoord {
        let server = Arc::new(Server::http("127.0.0.1:0").expect("bind mock"));
        let port = server.server_addr().to_ip().unwrap().port();
        let url = format!("http://127.0.0.1:{port}");
        let state = Arc::new(Mutex::new(State { url: url.clone(), wait_s: 1, admin_token: "admin-secret".into(), ..Default::default() }));
        let (s2, st2) = (server.clone(), state.clone());
        let thread = std::thread::spawn(move || {
            for req in s2.incoming_requests() {
                let st = st2.clone();
                std::thread::spawn(move || handle(req, &st));
            }
        });
        MockCoord { url, state, server, thread: Some(thread) }
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.state.lock().unwrap())
    }

    pub fn add_invite(&self, code: &str) {
        self.with(|s| s.invites.insert(code.to_string()));
    }

    /// Registers a machine directly; returns its token.
    pub fn add_machine(&self, machine_id: &str) -> String {
        let token = format!("tok-{machine_id}-{}", std::process::id());
        self.with(|s| s.machines.insert(token.clone(), machine_id.to_string()));
        token
    }

    pub fn trust(&self, pubkey_b64: &str) {
        self.with(|s| s.trusted = TrustedKeys::parse(&[pubkey_b64]).unwrap());
    }

    /// Serves every file of a build directory under `builds/<build_id>/`.
    pub fn add_build_dir(&self, dir: &std::path::Path) -> String {
        let mbytes = std::fs::read(dir.join(MANIFEST_FILE)).unwrap();
        let id = zerofarm_common::manifest::build_id(&mbytes);
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap() {
                let e = e.unwrap();
                if e.file_type().unwrap().is_dir() {
                    stack.push(e.path());
                    continue;
                }
                let rel = e.path().strip_prefix(dir).unwrap().to_string_lossy().replace('\\', "/");
                self.with(|s| s.artifacts.insert(format!("builds/{id}/{rel}"), std::fs::read(e.path()).unwrap()));
            }
        }
        id
    }

    pub fn add_artifact(&self, key: &str, bytes: Vec<u8>) -> String {
        let h = sha(&bytes);
        self.with(|s| s.artifacts.insert(key.to_string(), bytes));
        h
    }

    pub fn push_lease(&self, lease: Value) {
        self.with(|s| s.lease_queue.push_back(lease));
    }

    pub fn script_beat(&self, lease_id: &str, answer: Value) {
        self.with(|s| s.beat_script.entry(lease_id.to_string()).or_default().push_back(answer));
    }

    /// The next `count` requests matching `method` and path prefix get `status`.
    pub fn fail_next(&self, method: &str, prefix: &str, count: u32, status: u16) {
        self.with(|s| s.fail_rules.push(FailRule { method: method.into(), prefix: prefix.into(), remaining: count, status }));
    }

    pub fn clear_failures(&self) {
        self.with(|s| s.fail_rules.clear());
    }

    pub fn cancel_lease(&self, id: &str) {
        self.with(|s| s.leases.entry(id.to_string()).or_default().cancelled = true);
    }

    pub fn results_for(&self, lease_id: &str) -> Vec<(u64, StoredResult)> {
        self.with(|s| {
            let mut v: Vec<(u64, StoredResult)> =
                s.results.iter().filter(|((l, _), _)| l == lease_id).map(|((_, q), r)| (*q, r.clone())).collect();
            v.sort_by_key(|(q, _)| *q);
            v
        })
    }

    pub fn done_for(&self, lease_id: &str) -> Option<Value> {
        self.with(|s| s.dones.iter().find(|(l, _)| l == lease_id).map(|(_, v)| v.clone()))
    }

    pub fn count(&self, method: &str, prefix: &str) -> usize {
        self.with(|s| s.log.iter().filter(|l| l.method == method && l.path.starts_with(prefix)).count())
    }

    /// A lease JSON with sensible defaults.
    pub fn lease(lease_id: &str, kind: &str, build_id: &str, job: Value) -> Value {
        json!({
            "lease_id": lease_id, "kind": kind, "issued_at": 0, "expires_at": 0,
            "beat_s": 60, "slack_s": 600, "run": null, "campaign": null,
            "build": {"build_id": build_id, "manifest": format!("builds/{build_id}/manifest.json")},
            "runtime": null, "checkpoint": null, "job": job,
        })
    }
}

fn now() -> i64 {
    zerofarm_common::now_unix()
}

fn handle(mut req: Request, st: &Arc<Mutex<State>>) {
    let method = req.method().to_string().to_uppercase();
    let url = req.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (url.clone(), String::new()),
    };
    let hdr = |req: &Request, name: &str| -> Option<String> {
        req.headers().iter().find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name)).map(|h| h.value.as_str().to_string())
    };
    let agent = hdr(&req, "x-farm-agent");
    let token = hdr(&req, "authorization").and_then(|v| v.strip_prefix("Bearer ").map(String::from));
    let range = hdr(&req, "range");
    let x_sha = hdr(&req, "x-sha256");
    let mut extra_headers = HashMap::new();
    for h in ["x-ids", "x-gen", "x-farm-aw", "x-farm-diag"] {
        if let Some(v) = hdr(&req, h) {
            extra_headers.insert(h.to_string(), v);
        }
    }
    let mut body = Vec::new();
    let _ = req.as_reader().take(64 << 20).read_to_end(&mut body);
    let json_body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);

    let mut s = st.lock().unwrap();
    // Injected failures first.
    if let Some(rule) = s.fail_rules.iter_mut().find(|r| r.remaining > 0 && r.method == method && path.starts_with(&r.prefix)) {
        rule.remaining -= 1;
        let status = rule.status;
        s.log.push(Logged { method, path, agent, status });
        drop(s);
        err(req, status, "internal", "injected failure");
        return;
    }
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let is_admin = segs.get(1) == Some(&"admin");
    let authed_machine = token.as_ref().and_then(|t| s.machines.get(t).cloned());
    let status: u16;

    macro_rules! done {
        ($e:expr) => {{
            let code = $e;
            status = code;
        }};
    }

    if is_admin {
        if token.as_deref() != Some(s.admin_token.as_str()) {
            drop(s);
            let code = err(req, 401, "unauthorized", "admin token");
            st.lock().unwrap().log.push(Logged { method, path, agent, status: code });
            return;
        }
        match (method.as_str(), &segs[2..]) {
            ("PUT", ["artifact", rest @ ..]) => {
                s.artifacts.insert(rest.join("/"), body.clone());
                done!(reply(req, 200, json!({"ok": true})))
            }
            ("POST", ["build"]) => {
                let id = json_body["build_id"].as_str().unwrap_or("").to_string();
                let m = s.artifacts.get(&format!("builds/{id}/{MANIFEST_FILE}")).cloned();
                let sig = s.artifacts.get(&format!("builds/{id}/{SIG_FILE}")).cloned();
                let check = match (m, sig) {
                    (Some(m), Some(sig)) => match verify_manifest_bytes(&s.trusted, &m, &String::from_utf8_lossy(&sig)) {
                        Ok((man, bid)) if bid == id => man
                            .files
                            .iter()
                            .map(|f| match s.artifacts.get(&format!("builds/{id}/{}", f.path)) {
                                Some(b) if sha(b) == f.sha256 && b.len() as u64 == f.size => Ok(()),
                                Some(_) => Err(format!("{} hash/size mismatch", f.path)),
                                None => Err(format!("{} missing", f.path)),
                            })
                            .collect::<Result<Vec<()>, String>>()
                            .map(|_| ()),
                        Ok((_, bid)) => Err(format!("manifest hashes to {bid}")),
                        Err(e) => Err(e.to_string()),
                    },
                    _ => Err("manifest or signature missing".into()),
                };
                match check {
                    Ok(()) => {
                        s.builds_registered.push(id);
                        done!(reply(req, 200, json!({"ok": true})))
                    }
                    Err(e) => done!(err(req, 400, "bad_request", &e)),
                }
            }
            ("POST", ["agent", "release"]) => {
                s.agent_releases.push(json_body.clone());
                let id = json_body["build_id"].as_str().unwrap_or("");
                s.latest = Some(json!({"version": json_body["version"], "build_id": id,
                    "manifest": format!("builds/{id}/manifest.json"), "min_agent": json_body["min_agent"]}));
                done!(reply(req, 200, json!({"ok": true})))
            }
            ("POST", ["invites"]) => {
                let n = json_body["count"].as_u64().unwrap_or(1);
                let codes: Vec<String> = (0..n).map(|i| format!("TEST-CODE-{i:04}")).collect();
                for c in &codes {
                    s.invites.insert(c.clone());
                }
                done!(reply(req, 200, json!({"codes": codes})))
            }
            _ => done!(err(req, 404, "not_found", &path)),
        }
        s.log.push(Logged { method, path, agent, status });
        return;
    }

    // Friend-facing API.
    let route: Vec<&str> = segs[1..].to_vec();
    match (method.as_str(), route.as_slice()) {
        ("POST", ["enroll"]) => {
            s.enrolls.push(json_body.clone());
            let code = json_body["invite"].as_str().unwrap_or("").to_uppercase();
            let too_old = match (&s.enroll_min_agent, &agent) {
                (Some(min), Some(a)) => semver_lt(a, min),
                _ => false,
            };
            if too_old && s.invites.contains(&code) {
                let min = s.enroll_min_agent.clone().unwrap();
                let download = format!("{}/download", s.url);
                done!(reply(req, 426, json!({"error": "update_required", "message": "agent too old", "min_agent": min, "download": download})))
            } else if s.invites.remove(&code) {
                if let Some(d) = json_body["profile_digest"].as_str() {
                    s.known_digests.insert(d.to_string());
                }
                let n = s.machines.len() + 1;
                let mid = format!("m_TESTMACHINE{n:014}");
                let tok = format!("enrolled-token-{n}-{}", sha(code.as_bytes()));
                s.machines.insert(tok.clone(), mid.clone());
                done!(reply(req, 200, json!({"machine_id": mid, "token": tok, "channel": "stable"})))
            } else {
                done!(err(req, 403, "unauthorized", "unknown or used invite code"))
            }
        }
        ("GET", ["artifact", rest @ ..]) => {
            let key = rest.join("/");
            let allowed = key.starts_with("builds/") || key.starts_with("ckpt/");
            match s.artifacts.get(&key).cloned().filter(|_| allowed) {
                None => done!(err(req, 404, "not_found", &key)),
                Some(bytes) => {
                    let start = range
                        .as_deref()
                        .and_then(|r| r.strip_prefix("bytes="))
                        .and_then(|r| r.split('-').next())
                        .and_then(|n| n.parse::<usize>().ok());
                    drop(s);
                    let code = match start {
                        Some(n) if n >= bytes.len() => {
                            let _ = req.respond(Response::from_data(Vec::new()).with_status_code(416));
                            416
                        }
                        Some(n) => {
                            let cr = format!("bytes {n}-{}/{}", bytes.len() - 1, bytes.len());
                            let _ = req.respond(
                                Response::from_data(bytes[n..].to_vec())
                                    .with_status_code(206)
                                    .with_header(Header::from_bytes(&b"content-range"[..], cr.as_bytes()).unwrap()),
                            );
                            206
                        }
                        None => {
                            let _ = req.respond(Response::from_data(bytes));
                            200
                        }
                    };
                    st.lock().unwrap().log.push(Logged { method, path, agent, status: code });
                    return;
                }
            }
        }
        _ if authed_machine.is_none() => {
            let revoked = token.as_ref().is_some_and(|t| s.revoked.contains(t));
            if revoked {
                done!(err(req, 403, "revoked", "machine revoked"))
            } else {
                done!(err(req, 401, "unauthorized", "bad token"))
            }
        }
        ("GET", ["agent", "latest"]) => {
            let _ = query;
            match s.latest.clone() {
                Some(l) => done!(reply(req, 200, l)),
                None => done!(err(req, 404, "not_found", "no agent release")),
            }
        }
        ("POST", ["lease"]) if !agent.as_deref().is_some_and(is_semver) => {
            done!(err(req, 400, "bad_request", "x-farm-agent header (the agent's semver) is required"))
        }
        ("POST", ["lease"]) => {
            s.lease_requests.push(json_body.clone());
            let digest = json_body["profile_digest"].as_str().unwrap_or("").to_string();
            if !json_body["profile"].is_null() {
                s.known_digests.insert(digest.clone());
            }
            if s.need_profile && !s.known_digests.contains(&digest) {
                done!(reply(req, 409, json!({"error": "conflict", "message": "unknown profile", "need_profile": true})))
            } else if let Some(min) = s.update_min_agent.clone() {
                done!(reply(req, 200, json!({"update": {"min_agent": min}})))
            } else if let Some(mut l) = s.lease_queue.pop_front() {
                let t = now();
                if l["issued_at"] == 0 {
                    l["issued_at"] = json!(t);
                }
                if l["expires_at"] == 0 {
                    l["expires_at"] = json!(t + 900);
                }
                let id = l["lease_id"].as_str().unwrap().to_string();
                s.leases.insert(id, LeaseState { lease: l.clone(), ..Default::default() });
                let ans = with_diag(&mut s, json!({"lease": l}));
                done!(reply(req, 200, ans))
            } else {
                let w = s.wait_s;
                let ans = with_diag(&mut s, json!({"wait_s": w, "reason": "no_work"}));
                done!(reply(req, 200, ans))
            }
        }
        ("POST", ["lease", id, "beat"]) => {
            let id = id.to_string();
            s.beats.push((id.clone(), json_body.clone()));
            match s.leases.get(&id).cloned() {
                None => done!(err(req, 404, "not_found", "lease")),
                Some(ls) if ls.cancelled || ls.expired || ls.done => done!(err(req, 410, "gone", "lease")),
                Some(_) => {
                    let mut ans = s.beat_script.get_mut(&id).and_then(|q| q.pop_front()).unwrap_or(json!({"action": "continue"}));
                    if ans.get("expires_at").is_none() {
                        let beat_s = s.leases[&id].lease["beat_s"].as_i64().unwrap_or(60);
                        ans["expires_at"] = json!(now() + 3 * beat_s);
                    }
                    let ans_d = with_diag(&mut s, Value::Null);
                    if let Some(d) = ans_d.get("upload_diag") {
                        ans["upload_diag"] = d.clone();
                    }
                    if ans.get("checkpoint").is_none() {
                        ans["checkpoint"] = Value::Null;
                    }
                    if ans["action"] == "cancel" {
                        s.leases.get_mut(&id).unwrap().cancelled = true;
                    }
                    if ans["action"] == "expire" {
                        s.leases.get_mut(&id).unwrap().expired = true;
                        done!(err(req, 410, "gone", "lease expired"));
                        s.log.push(Logged { method, path, agent, status });
                        return;
                    }
                    done!(reply(req, 200, ans))
                }
            }
        }
        ("PUT", ["lease", id, "result", seq]) => {
            let id = id.to_string();
            let seq: u64 = seq.parse().unwrap_or(u64::MAX);
            let h = sha(&body);
            let ids = extra_headers.get("x-ids").map(|v| serde_json::from_str::<Value>(v).ok());
            let gen = extra_headers.get("x-gen").and_then(|v| v.parse::<u64>().ok());
            let lease = s.leases.get(&id).cloned();
            if x_sha.as_deref() != Some(h.as_str()) {
                done!(err(req, 400, "bad_request", "x-sha256 does not match body"))
            } else if lease.is_none() {
                done!(err(req, 404, "not_found", "lease"))
            } else if let Some(r) = s.results.get(&(id.clone(), seq)) {
                if r.sha256 == h {
                    done!(reply(req, 200, json!({"ok": true, "duplicate": true, "credited": 0, "already_credited": 0})))
                } else {
                    done!(err(req, 409, "conflict", "different bytes for this seq"))
                }
            } else if lease.as_ref().unwrap().cancelled {
                done!(err(req, 410, "gone", "lease cancelled"))
            } else {
                let ls = lease.unwrap();
                let (scope, range) = lease_scope_range(&ls.lease);
                let parsed: Result<Vec<(u64, u64)>, String> = match ids {
                    None => Ok(vec![]),
                    Some(None) => Err("x-ids must be JSON".into()),
                    Some(Some(v)) => parse_ranges_in(&v, range),
                };
                let kind = ls.lease["kind"].as_str().unwrap_or("").to_string();
                let fuzz_ok = kind != "fuzz" || valid_fuzz(&body);
                match parsed {
                    Err(e) => done!(err(req, 400, "bad_request", &e)),
                    Ok(_) if !fuzz_ok => done!(err(req, 400, "bad_request", "a fuzz result must be PROTOCOL 5.1 JSON")),
                    Ok(_) if kind == "selfplay" && gen.zip(s.stale_below_gen).is_some_and(|(g, min)| g < min) => {
                        done!(err(req, 410, "gone", "stale chunk"))
                    }
                    Ok(rs) => {
                        let claimed = s.claimed.entry(scope).or_default();
                        let mut credited = 0;
                        let mut already = 0;
                        for (a, b) in rs {
                            for x in a..b {
                                if claimed.iter().any(|(c, d)| x >= *c && x < *d) {
                                    already += 1;
                                } else {
                                    claimed.push((x, x + 1));
                                    credited += 1;
                                }
                            }
                        }
                        s.credits.push((id.clone(), seq, credited, already));
                        if !s.forget_results {
                            s.results.insert((id, seq), StoredResult { bytes: body.clone(), sha256: h, headers: extra_headers });
                        }
                        done!(reply(req, 200, json!({"ok": true, "duplicate": false, "credited": credited, "already_credited": already})))
                    }
                }
            }
        }
        ("POST", ["lease", id, "done"]) => {
            let id = id.to_string();
            match s.leases.get(&id).cloned() {
                None => done!(err(req, 404, "not_found", "lease")),
                Some(ls) if ls.cancelled => done!(err(req, 410, "gone", "lease cancelled")),
                Some(ls) if ls.done => done!(reply(req, 200, json!({"ok": true, "returned": 0}))),
                Some(ls) => {
                    let wanted: Vec<u64> = json_body["results"].as_array().map(|a| a.iter().filter_map(Value::as_u64).collect()).unwrap_or_default();
                    let missing: Vec<u64> = wanted.iter().copied().filter(|q| !s.results.contains_key(&(id.clone(), *q))).collect();
                    let (scope, range) = lease_scope_range(&ls.lease);
                    let field = if ls.lease["kind"] == "fuzz" { "seeds" } else { "gids" };
                    let bad_unfinished = match json_body.get("unfinished").and_then(|u| u.get(field)) {
                        Some(v) => parse_ranges_in(v, range).err(),
                        None => None,
                    };
                    if !missing.is_empty() {
                        done!(reply(req, 409, json!({"error": "conflict", "message": "results never received", "missing": missing})))
                    } else if let Some(e) = bad_unfinished {
                        done!(err(req, 400, "bad_request", &e))
                    } else {
                        s.leases.get_mut(&id).unwrap().done = true;
                        s.dones.push((id, json_body.clone()));
                        // The coordinator works out what goes back itself.
                        let returned = if ls.expired {
                            0
                        } else {
                            range.map(|(a, b)| {
                                let c = s.claimed.get(&scope).cloned().unwrap_or_default();
                                (a..b).filter(|x| !c.iter().any(|(p, q)| x >= p && x < q)).count()
                            })
                            .unwrap_or(0)
                        };
                        done!(reply(req, 200, json!({"ok": true, "returned": returned})))
                    }
                }
            }
        }
        ("POST", ["crash"]) => {
            s.crashes.push(json_body.clone());
            done!(reply(req, 200, json!({"ok": true})))
        }
        ("POST", ["diag", _]) if !s.diag_requested && extra_headers.get("x-farm-diag").map(String::as_str) == Some("user") => {
            if s.user_diag_limited {
                let body = serde_json::to_vec(&json!({"error": "rate_limited", "message": "once an hour", "retry_after": 1800})).unwrap();
                let resp = Response::from_data(body)
                    .with_status_code(429)
                    .with_header(Header::from_bytes(&b"retry-after"[..], &b"1800"[..]).unwrap())
                    .with_header(Header::from_bytes(&b"content-type"[..], &b"application/json"[..]).unwrap());
                let _ = req.respond(resp);
                done!(429)
            } else {
                s.user_diags.push(body.clone());
                s.user_diag_limited = true;
                done!(reply(req, 200, json!({"ok": true, "key": "diag/m/user-logs"})))
            }
        }
        ("POST", ["diag", _]) if !s.diag_requested => done!(err(req, 409, "conflict", "no diagnostics were requested")),
        ("POST", ["diag", name]) => {
            s.diags.push((name.to_string(), body.len()));
            s.diag_requested = false;
            done!(reply(req, 200, json!({"ok": true, "key": format!("diag/m/{name}")})))
        }
        _ => done!(err(req, 404, "not_found", &path)),
    }
    s.log.push(Logged { method, path, agent, status });
}

fn is_semver(v: &str) -> bool {
    let core = v.split(['-', '+']).next().unwrap_or("");
    let p: Vec<&str> = core.split('.').collect();
    p.len() == 3 && p.iter().all(|x| !x.is_empty() && x.bytes().all(|c| c.is_ascii_digit()))
}

fn semver_lt(a: &str, b: &str) -> bool {
    let n = |v: &str| -> Vec<u64> { v.split(['-', '+']).next().unwrap_or("").split('.').map(|x| x.parse().unwrap_or(0)).collect() };
    n(a) < n(b)
}

/// Adds `upload_diag` to an answer when one is pending.
fn with_diag(s: &mut State, mut ans: Value) -> Value {
    if s.upload_diag_next {
        s.upload_diag_next = false;
        s.diag_requested = true;
        if ans.is_null() {
            ans = json!({});
        }
        ans["upload_diag"] = json!(["logs"]);
    }
    ans
}

/// (scope, the lease's id range) — `[gid_base, gid_base + games)` or `[first, first + count)`.
fn lease_scope_range(l: &Value) -> (String, Option<(u64, u64)>) {
    let scope = l["run"].as_str().or(l["campaign"].as_str()).unwrap_or(l["lease_id"].as_str().unwrap_or("")).to_string();
    let j = &l["job"];
    let range = match (j["gid_base"].as_u64(), j["games"].as_u64(), j["first"].as_u64(), j["count"].as_u64()) {
        (Some(b), Some(g), _, _) => Some((b, b + g)),
        (_, _, Some(f), Some(c)) => Some((f, f + c)),
        _ => None,
    };
    (scope, range)
}

fn parse_ranges_in(v: &Value, range: Option<(u64, u64)>) -> Result<Vec<(u64, u64)>, String> {
    let Some((lo, hi)) = range else { return Err("this lease has no ids".into()) };
    let a = v.as_array().ok_or("ranges must be a list")?;
    let mut out = Vec::new();
    for r in a {
        let p = r.as_array().filter(|p| p.len() == 2).ok_or("each range is [lo, hi]")?;
        let (x, y) = (p[0].as_u64().ok_or("integer ids")?, p[1].as_u64().ok_or("integer ids")?);
        if x >= y || x < lo || y > hi {
            return Err(format!("range [{x}, {y}) is not inside the lease's [{lo}, {hi})"));
        }
        out.push((x, y));
    }
    Ok(out)
}

fn valid_fuzz(body: &[u8]) -> bool {
    let Ok(v) = serde_json::from_slice::<Value>(body) else { return false };
    v["failures"].as_array().is_some_and(|f| f.iter().all(|x| x["seed"].is_u64() && x["site"].is_string()))
}
