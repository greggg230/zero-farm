//! "Send diagnostics" (tray): the logs go up as a user-initiated upload (`x-farm-diag: user`), and
//! the friend is told how it went, including the coordinator's once-an-hour limit.

use farm_agent::logging::{logs_body, send_user_diag, user_diag_message};
use mockcoord::MockCoord;
use zerofarm_common::http::Client;
use zerofarm_common::layout::Layout;

#[test]
fn send_diagnostics_uploads_the_logs_once_an_hour() {
    let d = tempfile::tempdir().unwrap();
    let layout = Layout::new(d.path());
    std::fs::create_dir_all(layout.logs()).unwrap();
    std::fs::write(layout.logs().join("agent.log"), "agent line\n").unwrap();
    std::fs::write(layout.logs().join("worker.log"), vec![b'w'; 5 << 20]).unwrap(); // tail-capped
    let mock = MockCoord::start();
    let token = mock.add_machine("m_DIAG00000000000000000000001");
    let client = Client::new(&mock.url, Some(token), "0.2.0").unwrap();

    let r = send_user_diag(&client, &layout, "Zero Farm 0.2.0, sent from the tray");
    assert_eq!(user_diag_message(&r), "Diagnostics sent. Thank you!");
    let body = mock.with(|s| s.user_diags[0].clone());
    let text = String::from_utf8_lossy(&body);
    assert!(text.starts_with("===== about =====\nZero Farm 0.2.0, sent from the tray\n"), "{}", &text[..80]);
    assert!(text.contains("===== agent.log =====\nagent line"));
    assert!(text.contains("===== worker.log ====="));
    assert!(body.len() < (4 << 20) + 200, "each log is tail-capped at 4 MB: {}", body.len());
    assert_eq!(body, logs_body(&layout, "Zero Farm 0.2.0, sent from the tray"));

    // Again within the hour: the coordinator says 429; the friend is told when to try again.
    let again = send_user_diag(&client, &layout, "");
    assert_eq!(again.as_ref().unwrap_err().status(), Some(429));
    assert_eq!(user_diag_message(&again), "Diagnostics were already sent in the last hour. You can send them again in 30 min.");
    assert_eq!(mock.with(|s| s.user_diags.len()), 1);

    // Without the header, an unrequested upload is still refused (409).
    let plain = client.post_bytes_json::<serde_json::Value>("/v1/diag/logs", b"x", "text/plain");
    assert_eq!(plain.unwrap_err().status(), Some(409));
}

/// True when `s` still carries this Windows user's profile path or user name.
fn leaks(s: &str) -> bool {
    let low = s.to_lowercase();
    let profile = std::env::var("USERPROFILE").unwrap_or_default().to_lowercase();
    let user = std::env::var("USERNAME").unwrap_or_default().to_lowercase();
    (!profile.is_empty() && low.contains(&profile)) || (user.chars().count() >= 3 && low.contains(&user))
}

#[test]
fn logs_crash_reports_and_stats_leave_without_the_user_name_or_profile_path() {
    // Security review M5: the install path (and so the Windows user name) was in every upload.
    let profile = std::env::var("USERPROFILE").expect("USERPROFILE");
    let user = std::env::var("USERNAME").unwrap_or_default();
    let root = format!(r"{profile}\AppData\Local\ZeroFarm");
    let d = tempfile::tempdir().unwrap();
    let layout = Layout::new(d.path());
    std::fs::create_dir_all(layout.logs()).unwrap();

    // Lines are redacted as they are written ...
    let log = zerofarm_common::rotlog::RotatingFile::new(layout.logs().join("agent.log"), 1 << 20, 1);
    log.write_line(&format!("farm-agent 0.2.5 starting, root {root}, user {user}"));
    let written = std::fs::read_to_string(layout.logs().join("agent.log")).unwrap();
    assert!(written.contains(r"root %USERPROFILE%\AppData\Local\ZeroFarm"), "{written}");
    assert!(!leaks(&written), "{written}");
    // ... and logs written before 0.2.5 on upload, JSON-escaped and `/` spellings included.
    let old = format!("start 0.2.4, root {root}\n{{\"model\": {:?}}}\n{}\n", format!(r"{root}\cache\m.onnx"), root.replace('\\', "/"));
    std::fs::write(layout.logs().join("launcher.log"), &old).unwrap();
    assert!(leaks(&old));
    let body = String::from_utf8(logs_body(&layout, &format!("status: downloading to {root}"))).unwrap();
    assert!(!leaks(&body), "{body}");
    assert!(body.contains(r"status: downloading to %USERPROFILE%\AppData\Local\ZeroFarm"), "{body}");

    // Crash reports: the message and the worker's stderr tail.
    let mock = MockCoord::start();
    let token = mock.add_machine("m_DIAG00000000000000000000002");
    let client = Client::new(&mock.url, Some(token), "0.2.5").unwrap();
    let crash = zerofarm_common::protocol::CrashReq {
        kind: "crash".into(),
        message: format!(r"worker fatal error: {root}\cache\ckpt\r\1\model.onnx has sha256 00"),
        backtrace: Some(format!("fakeworker: job {{\"model\":{:?}}}", format!(r"{root}\cache\m.onnx"))),
        ..Default::default()
    };
    farm_agent::engine::send_crash(&client, &crash).unwrap();
    let sent = mock.with(|s| s.crashes[0].clone());
    assert!(!leaks(&sent.to_string()), "{sent}");
    assert!(sent["message"].as_str().unwrap().contains(r"%USERPROFILE%\AppData\Local\ZeroFarm\cache"), "{sent}");

    // Stats (beats and done): every string.
    let mut st = zerofarm_common::protocol::Stats::new();
    st.insert("error".into(), serde_json::json!(format!(r"setup failed: {root}\work: denied")));
    st.insert("selftest".into(), serde_json::json!({"1": {"provider": format!("cpu ({user})")}}));
    let st = serde_json::Value::Object(farm_agent::lease::redacted(st));
    assert!(!leaks(&st.to_string()), "{st}");
}
