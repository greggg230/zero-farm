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
