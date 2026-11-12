//! Keeps the Windows user name and the user-profile path out of logs and uploads (security
//! review M5). Everything written to `logs\*.log` goes through [`redact`] (the rotating log does
//! it), and so does everything uploaded that can carry a path: the diagnostics body, crash
//! reports and Stats.
//!
//! `C:\Users\Sam\AppData\Local\ZeroFarm` becomes `%USERPROFILE%\AppData\Local\ZeroFarm`, also in
//! its `/` and JSON-escaped (`\\`) spellings and its 8.3 short form (`C:\Users\SAMSMI~1`, which
//! `%TEMP%` often uses); the user name (`%USERNAME%`, and the profile folder's own name when it
//! differs) becomes `<user>` wherever it stands as a word. Matching ignores case, as Windows
//! paths do.

use serde_json::Value;
use std::borrow::Cow;
use std::sync::OnceLock;

/// What the user-profile path becomes.
pub const PROFILE: &str = "%USERPROFILE%";
/// What the user name becomes.
pub const USER: &str = "<user>";

/// Names shorter than this are replaced only as a whole path component (`\Al\`), never as a word
/// in text, where they would match everywhere.
const SHORT_NAME: usize = 3;

pub struct Redactor {
    /// Spellings of the profile path, case-folded.
    profiles: Vec<Vec<char>>,
    /// User names, case-folded.
    names: Vec<Vec<char>>,
}

fn fold(c: char) -> char {
    if c.is_ascii() {
        c.to_ascii_lowercase()
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

fn folded(s: &str) -> Vec<char> {
    s.chars().map(fold).collect()
}

fn is_sep(c: char) -> bool {
    c == '\\' || c == '/'
}

impl Redactor {
    /// `profile_paths`: spellings of the user-profile directory (`C:\Users\Sam`, its short
    /// form); `names`: the user names to hide.
    pub fn new(profile_paths: &[&str], names: &[&str]) -> Redactor {
        let mut profiles = Vec::new();
        let mut all_names: Vec<String> = names.iter().map(|n| n.trim().to_string()).collect();
        for p in profile_paths {
            let parts: Vec<&str> = p.split(['\\', '/']).filter(|s| !s.is_empty()).collect();
            // A drive root or a single component is no profile path worth replacing.
            if parts.len() >= 2 {
                for sep in ["\\", "/", "\\\\"] {
                    profiles.push(folded(&parts.join(sep)));
                }
                if let Some(last) = parts.last() {
                    all_names.push(last.to_string());
                }
            }
        }
        let mut names_f: Vec<Vec<char>> = Vec::new();
        for n in all_names {
            let f = folded(&n);
            if !f.is_empty() && !names_f.contains(&f) {
                names_f.push(f);
            }
        }
        // Longest first, so "sam.smith" goes before "sam".
        names_f.sort_by_key(|n| std::cmp::Reverse(n.len()));
        Redactor { profiles, names: names_f }
    }

    /// From `%USERPROFILE%` (and its short form) and `%USERNAME%`.
    pub fn from_env() -> Redactor {
        let mut paths: Vec<String> = Vec::new();
        if let Some(p) = std::env::var("USERPROFILE").ok().filter(|p| !p.trim().is_empty()) {
            if let Some(short) = short_path(&p).filter(|s| !s.eq_ignore_ascii_case(&p)) {
                paths.push(short);
            }
            paths.push(p);
        }
        let name = std::env::var("USERNAME").ok().filter(|n| !n.trim().is_empty());
        let names: Vec<&str> = name.iter().map(String::as_str).collect();
        let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
        Redactor::new(&paths, &names)
    }

    /// `s` with the profile path and the user name replaced (borrowed when there was nothing to
    /// replace).
    pub fn redact<'a>(&self, s: &'a str) -> Cow<'a, str> {
        let mut out = Cow::Borrowed(s);
        for p in &self.profiles {
            if let Some(r) = replace_ci(&out, p, PROFILE, |_, next| next.is_none_or(|c| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.')))) {
                out = Cow::Owned(r);
            }
        }
        for n in &self.names {
            let short = n.len() < SHORT_NAME;
            let r = replace_ci(&out, n, USER, |prev, next| {
                if short {
                    prev.is_some_and(is_sep) && next.is_none_or(|c| is_sep(c) || !c.is_alphanumeric())
                } else {
                    prev.is_none_or(|c| !c.is_alphanumeric()) && next.is_none_or(|c| !c.is_alphanumeric())
                }
            });
            if let Some(r) = r {
                out = Cow::Owned(r);
            }
        }
        out
    }

    /// Every string (and object key) in `v`, in place.
    pub fn redact_json(&self, v: &mut Value) {
        match v {
            Value::String(s) => {
                if let Cow::Owned(r) = self.redact(s) {
                    *s = r;
                }
            }
            Value::Array(a) => a.iter_mut().for_each(|x| self.redact_json(x)),
            Value::Object(m) => {
                let old = std::mem::take(m);
                for (k, mut x) in old {
                    self.redact_json(&mut x);
                    m.insert(self.redact(&k).into_owned(), x);
                }
            }
            _ => {}
        }
    }
}

/// Replaces every case-insensitive occurrence of `needle` (case-folded) in `hay` whose
/// neighbouring characters pass `boundary(prev, next)`. None when nothing matched.
fn replace_ci(hay: &str, needle: &[char], with: &str, boundary: impl Fn(Option<char>, Option<char>) -> bool) -> Option<String> {
    let first = *needle.first()?;
    let mut out: Option<String> = None;
    let mut copied = 0; // bytes of `hay` already in `out`
    let mut i = 0;
    while i < hay.len() {
        let c = hay[i..].chars().next().expect("char boundary");
        if fold(c) == first {
            // Try a match at i.
            let mut end = i;
            let mut ok = true;
            let mut it = hay[i..].chars();
            for &n in needle {
                match it.next() {
                    Some(h) if fold(h) == n => end += h.len_utf8(),
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok && boundary(hay[..i].chars().next_back(), hay[end..].chars().next()) {
                let o = out.get_or_insert_with(|| String::with_capacity(hay.len()));
                o.push_str(&hay[copied..i]);
                o.push_str(with);
                copied = end;
                i = end;
                continue;
            }
        }
        i += c.len_utf8();
    }
    out.map(|mut o| {
        o.push_str(&hay[copied..]);
        o
    })
}

/// The 8.3 short form of an existing path (`C:\Users\SAMSMI~1`), if Windows has one.
fn short_path(p: &str) -> Option<String> {
    use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;
    let w: Vec<u16> = p.encode_utf16().chain(std::iter::once(0)).collect();
    let mut buf = vec![0u16; 1024];
    let n = unsafe { GetShortPathNameW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
    (n > 0 && n < buf.len()).then(|| String::from_utf16_lossy(&buf[..n]))
}

fn global() -> &'static Redactor {
    static R: OnceLock<Redactor> = OnceLock::new();
    R.get_or_init(Redactor::from_env)
}

/// [`Redactor::redact`] with this process's user (`%USERPROFILE%`, `%USERNAME%`).
pub fn redact(s: &str) -> Cow<'_, str> {
    global().redact(s)
}

/// [`Redactor::redact_json`] with this process's user.
pub fn redact_json(v: &mut Value) {
    global().redact_json(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn r() -> Redactor {
        Redactor::new(&[r"C:\Users\Sam.Smith"], &["sam.smith"])
    }

    #[test]
    fn profile_paths_in_every_spelling() {
        let r = r();
        assert_eq!(r.redact(r"starting, root C:\Users\Sam.Smith\AppData\Local\ZeroFarm"), r"starting, root %USERPROFILE%\AppData\Local\ZeroFarm");
        assert_eq!(r.redact(r"c:\users\SAM.SMITH\x"), r"%USERPROFILE%\x", "case-insensitive");
        assert_eq!(r.redact("C:/Users/Sam.Smith/AppData"), "%USERPROFILE%/AppData");
        assert_eq!(r.redact(r#"{"model":"C:\\Users\\Sam.Smith\\AppData\\m.onnx"}"#), r#"{"model":"%USERPROFILE%\\AppData\\m.onnx"}"#);
        assert_eq!(r.redact(r"\\?\C:\Users\Sam.Smith\AppData"), r"\\?\%USERPROFILE%\AppData");
        assert_eq!(r.redact(r"C:\Users\Sam.Smith"), "%USERPROFILE%");
        // Another profile that merely starts the same: only the name part is a match, not a word.
        assert_eq!(r.redact(r"C:\Users\Sam.Smithers\x"), r"C:\Users\Sam.Smithers\x");
        assert!(matches!(r.redact("nothing to see"), Cow::Borrowed(_)));
    }

    #[test]
    fn user_names_as_words_only() {
        let r = Redactor::new(&[r"C:\Users\Gregor"], &["gregor"]);
        assert_eq!(r.redact(r"D:\Data\Gregor\games and gregor's notes"), r"D:\Data\<user>\games and <user>'s notes");
        assert_eq!(r.redact("Gregorian calendar"), "Gregorian calendar", "a word that only contains the name");
        assert_eq!(r.redact("user Gregor"), "user <user>");
        // The profile folder's name counts even when %USERNAME% differs.
        let r = Redactor::new(&[r"C:\Users\gk.LAB"], &["gk"]);
        assert_eq!(r.redact(r"E:\work\gk.LAB\x"), r"E:\work\<user>\x");
        assert_eq!(r.redact(r"E:\work\gk\x and gk said"), r"E:\work\<user>\x and gk said", "short names: path components only");
        // Non-ASCII names.
        let r = Redactor::new(&[r"C:\Users\Zoë"], &["Zoë"]);
        assert_eq!(r.redact(r"C:\USERS\ZOË\AppData and zoë"), r"%USERPROFILE%\AppData and <user>");
    }

    #[test]
    fn json_values_and_keys() {
        let r = r();
        let mut v = json!({"error": r"setup failed: C:\Users\Sam.Smith\AppData\x: denied", "n": 3,
                           "list": ["sam.smith", 1], r"C:\Users\Sam.Smith": true});
        r.redact_json(&mut v);
        assert_eq!(v, json!({"error": r"setup failed: %USERPROFILE%\AppData\x: denied", "n": 3, "list": ["<user>", 1], "%USERPROFILE%": true}));
    }

    #[test]
    fn nothing_configured_changes_nothing() {
        let r = Redactor::new(&[], &[]);
        assert_eq!(r.redact(r"C:\Users\x"), r"C:\Users\x");
        let r = Redactor::new(&["C:\\"], &[""]);
        assert_eq!(r.redact(r"C:\Users\x"), r"C:\Users\x");
    }

    #[test]
    fn short_profile_paths_too() {
        let r = Redactor::new(&[r"C:\Users\SAMSMI~1", r"C:\Users\Sam Smith"], &["Sam Smith"]);
        assert_eq!(r.redact(r"C:\Users\SAMSMI~1\AppData\Local\Temp\x"), r"%USERPROFILE%\AppData\Local\Temp\x");
        assert_eq!(r.redact(r"C:\Users\Sam Smith\x and Sam Smith"), r"%USERPROFILE%\x and <user>");
        // This process's own profile, in whichever spelling the system gives %TEMP%.
        let tmp = std::env::temp_dir().display().to_string();
        if let Ok(p) = std::env::var("USERPROFILE") {
            let short = short_path(&p).unwrap_or_else(|| p.clone());
            let low = tmp.to_ascii_lowercase();
            if low.starts_with(&p.to_ascii_lowercase()) || low.starts_with(&short.to_ascii_lowercase()) {
                assert!(redact(&tmp).starts_with(PROFILE), "{}", redact(&tmp));
            }
        }
    }
}
