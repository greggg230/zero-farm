//! Enrollment (PROTOCOL 3, `POST /v1/enroll`): an invite code becomes a machine id and token;
//! the token is stored with DPAPI (CurrentUser) in `state\machine.json`.

use crate::settings::Machine;
use zerofarm_common::http::{CallError, Client};
use zerofarm_common::layout::Layout;
use zerofarm_common::protocol::{EnrollReq, EnrollResp, Profile};

/// Uppercases, drops spaces and dashes, checks 12 characters of `[A-Z2-9]`, and re-adds the
/// dashes: `abcd efgh-jklm` → `ABCD-EFGH-JKLM`.
pub fn normalize_invite(s: &str) -> Option<String> {
    let c: String = s.chars().filter(|c| !c.is_whitespace() && *c != '-').map(|c| c.to_ascii_uppercase()).collect();
    if c.len() != 12 || !c.chars().all(|ch| ch.is_ascii_uppercase() || ('2'..='9').contains(&ch)) {
        return None;
    }
    Some(format!("{}-{}-{}", &c[0..4], &c[4..8], &c[8..12]))
}

#[derive(Debug)]
pub enum EnrollError {
    BadCode,
    Refused(String),
    /// 426: this agent is older than the invite's channel allows. An unenrolled agent can't
    /// update itself (`/v1/agent/latest` needs a machine token): the friend needs a newer
    /// installer, from `download` (the coordinator's `/download` link). The invite is not used up.
    UpdateRequired { min_agent: String, download: Option<String> },
    Network(String),
    Io(String),
}

impl std::fmt::Display for EnrollError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnrollError::BadCode => write!(f, "That doesn't look like an invite code (XXXX-XXXX-XXXX)."),
            EnrollError::Refused(m) => write!(f, "The farm didn't accept that code ({m}). Codes work once; ask for a new one."),
            EnrollError::UpdateRequired { min_agent, download } => write!(
                f,
                "This Zero Farm installer is too old (the farm needs version {min_agent} or newer). Please download the \
                 latest ZeroFarmSetup.exe{} and run it; your invite code still works.",
                download.as_deref().map(|u| format!(" from {u}")).unwrap_or_default()
            ),
            EnrollError::Network(m) => write!(f, "Couldn't reach the farm: {m}"),
            EnrollError::Io(m) => write!(f, "Couldn't save the enrollment: {m}"),
        }
    }
}

/// Enrolls this PC and saves the result in `machine`/`machine.json`.
pub fn enroll(
    layout: &Layout,
    coordinator: &str,
    version: &str,
    machine: &mut Machine,
    invite: &str,
    name: &str,
    profile: &Profile,
) -> Result<(), EnrollError> {
    let code = normalize_invite(invite).ok_or(EnrollError::BadCode)?;
    let client = Client::new(coordinator, None, version).map_err(EnrollError::Network)?;
    let req = EnrollReq {
        invite: code,
        name: name.chars().take(64).collect(),
        profile: profile.clone(),
        profile_digest: Some(profile.digest()),
    };
    let resp: EnrollResp = match client.post_json("/v1/enroll", &req) {
        Ok(r) => r,
        Err(CallError::Http { status: 426, body, .. }) => {
            let field = |k: &str| body.as_ref().and_then(|b| b.get(k)).and_then(|v| v.as_str()).map(String::from);
            // Only a link to the coordinator we enrolled with is shown.
            let download = field("download").filter(|u| u.starts_with(coordinator.trim_end_matches('/')));
            return Err(EnrollError::UpdateRequired { min_agent: field("min_agent").unwrap_or_else(|| "?".into()), download });
        }
        Err(e @ CallError::Http { .. }) if !e.is_retryable() => return Err(EnrollError::Refused(e.code().unwrap_or("").to_string())),
        Err(e) => return Err(EnrollError::Network(e.to_string())),
    };
    if !zerofarm_common::is_safe_id(&resp.machine_id) {
        return Err(EnrollError::Refused("bad machine id".into()));
    }
    machine.machine_id = Some(resp.machine_id);
    machine.set_token(&resp.token).map_err(|e| EnrollError::Io(e.to_string()))?;
    machine.channel = resp.channel;
    machine.name = Some(req.name);
    machine.save(layout).map_err(|e| EnrollError::Io(e.to_string()))?;
    // The coordinator now knows this profile.
    let _ = zerofarm_common::fsutil::atomic_write(&layout.state().join("profile-digest"), profile.digest().as_bytes());
    Ok(())
}

/// This PC's name for the contributors page (the friend can change it in the dialog).
pub fn default_machine_name() -> String {
    use windows_sys::Win32::System::SystemInformation::{ComputerNamePhysicalDnsHostname, GetComputerNameExW};
    let mut buf = [0u16; 256];
    let mut n = buf.len() as u32;
    if unsafe { GetComputerNameExW(ComputerNamePhysicalDnsHostname, buf.as_mut_ptr(), &mut n) } != 0 {
        return crate::util::from_wide(&buf[..n as usize]);
    }
    "My PC".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invite_codes() {
        assert_eq!(normalize_invite("abcd efgh-jkm2").as_deref(), Some("ABCD-EFGH-JKM2"));
        assert_eq!(normalize_invite(" ABCD-EFGH-JKLM ").as_deref(), Some("ABCD-EFGH-JKLM"));
        assert_eq!(normalize_invite("ABCD-EFGH-JKL"), None);
        assert_eq!(normalize_invite("ABCD-EFGH-JK1M"), None, "1 is not in the alphabet");
        assert_eq!(normalize_invite("ABCD-EFGH-JK!M"), None);
    }
}
