//! Enrollment against the mock coordinator: an invite code becomes a machine id and a
//! DPAPI-protected token; used codes are refused; a too-old agent gets 426 (rev 2) and keeps the
//! invite usable.

use farm_agent::enroll::{enroll, EnrollError};
use farm_agent::settings::Machine;
use mockcoord::MockCoord;
use zerofarm_common::layout::Layout;

fn profile() -> zerofarm_common::protocol::Profile {
    zerofarm_common::protocol::Profile { os: "Windows 11 26100".into(), cpu: "Test".into(), cores: 4, threads: 8, ..Default::default() }
}

#[test]
fn invite_becomes_a_protected_token() {
    let d = tempfile::tempdir().unwrap();
    let layout = Layout::new(d.path());
    let mock = MockCoord::start();
    mock.add_invite("ABCD-EFGH-JKM2");
    let mut m = Machine::default();
    enroll(&layout, &mock.url, "0.1.0", &mut m, "abcd efgh jkm2", "Sam's desktop", &profile()).unwrap();
    assert!(m.enrolled());
    let saved = Machine::load(&layout);
    assert_eq!(saved.machine_id, m.machine_id);
    let token = saved.token().unwrap();
    assert!(mock.with(|s| s.machines.contains_key(&token)));
    assert!(!std::fs::read_to_string(layout.machine_json()).unwrap().contains(&token));
    let req = mock.with(|s| s.enrolls[0].clone());
    assert_eq!(req["invite"], "ABCD-EFGH-JKM2");
    assert_eq!(req["name"], "Sam's desktop");
    assert_eq!(req["profile_digest"], profile().digest().as_str());
    // The code is single-use.
    let mut m2 = Machine::default();
    assert!(matches!(enroll(&layout, &mock.url, "0.1.0", &mut m2, "ABCD-EFGH-JKM2", "x", &profile()), Err(EnrollError::Refused(_))));
    assert!(matches!(enroll(&layout, &mock.url, "0.1.0", &mut m2, "nope", "x", &profile()), Err(EnrollError::BadCode)));
}

#[test]
fn too_old_agent_gets_426_and_the_invite_survives() {
    let d = tempfile::tempdir().unwrap();
    let layout = Layout::new(d.path());
    let mock = MockCoord::start();
    mock.add_invite("ABCD-EFGH-JKM2");
    mock.with(|s| s.enroll_min_agent = Some("0.3.0".into()));
    let mut m = Machine::default();
    match enroll(&layout, &mock.url, "0.1.0", &mut m, "ABCD-EFGH-JKM2", "x", &profile()) {
        Err(e @ EnrollError::UpdateRequired { .. }) => {
            let EnrollError::UpdateRequired { min_agent, download } = &e else { unreachable!() };
            assert_eq!(min_agent, "0.3.0");
            assert_eq!(download.as_deref(), Some(format!("{}/download", mock.url).as_str()));
            assert!(e.to_string().contains(&format!("ZeroFarmSetup.exe from {}/download", mock.url)), "{e}");
        }
        other => panic!("{other:?}"),
    }
    assert!(!m.enrolled());
    // A current agent can still use the same code.
    enroll(&layout, &mock.url, "0.3.0", &mut m, "ABCD-EFGH-JKM2", "x", &profile()).unwrap();
    assert!(m.enrolled());
}
