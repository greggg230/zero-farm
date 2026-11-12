//! DPAPI, CurrentUser scope: data protected here can only be read back by the same Windows user
//! (on the same machine unless roaming credentials are set up). Used for the machine token
//! (agent) and the release secret key (desktop tool).

use std::io;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 }
}

/// Encrypts `data` for the current user. `purpose` is mixed in as DPAPI's optional entropy, so a
/// blob made for one purpose cannot be decrypted as another.
pub fn protect(data: &[u8], purpose: &str) -> io::Result<Vec<u8>> {
    let input = blob(data);
    let entropy = blob(purpose.as_bytes());
    let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
    let ok = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),
            &entropy,
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    };
    take(ok, out)
}

pub fn unprotect(data: &[u8], purpose: &str) -> io::Result<Vec<u8>> {
    let input = blob(data);
    let entropy = blob(purpose.as_bytes());
    let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            &entropy,
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    };
    take(ok, out)
}

fn take(ok: i32, out: CRYPT_INTEGER_BLOB) -> io::Result<Vec<u8>> {
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let v = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
    unsafe { LocalFree(out.pbData as _) };
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip_and_purpose_binding() {
        let b = protect(b"secret token", "zero-farm test").unwrap();
        assert_ne!(&b[..], b"secret token");
        assert_eq!(unprotect(&b, "zero-farm test").unwrap(), b"secret token");
        assert!(unprotect(&b, "other purpose").is_err());
    }
}
