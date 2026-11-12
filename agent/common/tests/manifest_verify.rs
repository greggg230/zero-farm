//! manifest/1 verification: anything not exactly as signed by a trusted key is refused.

use std::fs;
use std::path::Path;
use zerofarm_common::keys::{SigError, TrustedKeys};
use zerofarm_common::manifest::{self, verify_dir, Expect, Kind, ManifestError, MANIFEST_FILE, SIG_FILE};
use zerofarm_common::sign::{sign_dir, template, SigningKey};

fn make_build(dir: &Path, key: &SigningKey) -> String {
    fs::create_dir_all(dir.join("lib")).unwrap();
    fs::write(dir.join("w.exe"), b"MZ fake worker").unwrap();
    fs::write(dir.join("lib").join("onnxruntime.dll"), vec![7u8; 4096]).unwrap();
    let mut t = template(Kind::Worker, "mtg-60397f3");
    t.game = Some("mtg".into());
    t.aw = Some(1);
    t.zero_onnx = Some(vec![1]);
    t.entry = Some("w.exe".into());
    let (id, m) = sign_dir(dir, t, key).unwrap();
    assert_eq!(m.files.len(), 2);
    assert_eq!(m.files[0].path, "lib/onnxruntime.dll");
    id
}

fn keys_for(k: &SigningKey) -> TrustedKeys {
    TrustedKeys::parse(&[k.public_b64()]).unwrap()
}

#[test]
fn good_build_verifies_and_signature_is_minisign_format() {
    let d = tempfile::tempdir().unwrap();
    let k = SigningKey::generate().unwrap();
    let id = make_build(d.path(), &k);
    let v = verify_dir(d.path(), &keys_for(&k), &Expect { build_id: Some(&id), kind: Some(Kind::Worker), version: None }).unwrap();
    assert_eq!(v.build_id, id);
    assert_eq!(v.entry_path().unwrap(), d.path().join("w.exe"));
    let sig = fs::read_to_string(d.path().join(SIG_FILE)).unwrap();
    let lines: Vec<&str> = sig.lines().collect();
    assert_eq!(lines.len(), 4);
    assert!(lines[0].starts_with("untrusted comment: "));
    use base64::Engine as _;
    let raw = base64::engine::general_purpose::STANDARD.decode(lines[1]).unwrap();
    assert_eq!(&raw[..2], b"ED", "prehashed ED signature");
    assert_eq!(raw.len(), 74);
    assert_eq!(lines[2], format!("trusted comment: build_id={id} kind=worker"));
    // Public key file is minisign.pub format.
    let pubf = k.public_file();
    let pk = minisign_verify::PublicKey::decode(&pubf).unwrap();
    let s = minisign_verify::Signature::decode(&sig).unwrap();
    pk.verify(&fs::read(d.path().join(MANIFEST_FILE)).unwrap(), &s, false).unwrap();
}

#[test]
fn tampered_file_is_refused() {
    let d = tempfile::tempdir().unwrap();
    let k = SigningKey::generate().unwrap();
    make_build(d.path(), &k);
    // Same size, different content.
    fs::write(d.path().join("w.exe"), b"MZ evil worker").unwrap();
    let e = verify_dir(d.path(), &keys_for(&k), &Expect::default()).unwrap_err();
    assert_eq!(e, ManifestError::HashMismatch { path: "w.exe".into() });
}

#[test]
fn size_change_missing_and_extra_files_are_refused() {
    let k = SigningKey::generate().unwrap();
    let d = tempfile::tempdir().unwrap();
    make_build(d.path(), &k);
    fs::write(d.path().join("w.exe"), b"MZ fake worker!").unwrap();
    assert!(matches!(verify_dir(d.path(), &keys_for(&k), &Expect::default()), Err(ManifestError::SizeMismatch { .. })));

    let d = tempfile::tempdir().unwrap();
    make_build(d.path(), &k);
    fs::remove_file(d.path().join("lib").join("onnxruntime.dll")).unwrap();
    assert!(matches!(verify_dir(d.path(), &keys_for(&k), &Expect::default()), Err(ManifestError::MissingFile(_))));

    let d = tempfile::tempdir().unwrap();
    make_build(d.path(), &k);
    fs::write(d.path().join("lib").join("planted.dll"), b"x").unwrap();
    assert_eq!(
        verify_dir(d.path(), &keys_for(&k), &Expect::default()).unwrap_err(),
        ManifestError::ExtraFile("lib/planted.dll".into())
    );
}

#[test]
fn edited_manifest_breaks_the_signature() {
    let d = tempfile::tempdir().unwrap();
    let k = SigningKey::generate().unwrap();
    make_build(d.path(), &k);
    // Point the manifest at a different hash of w.exe after replacing it: the attacker's dream.
    fs::write(d.path().join("w.exe"), b"MZ evil worker").unwrap();
    let evil = zerofarm_common::hash::sha256_hex(b"MZ evil worker");
    let text = fs::read_to_string(d.path().join(MANIFEST_FILE)).unwrap();
    let mut m: serde_json::Value = serde_json::from_str(&text).unwrap();
    for f in m["files"].as_array_mut().unwrap() {
        if f["path"] == "w.exe" {
            f["sha256"] = evil.clone().into();
        }
    }
    fs::write(d.path().join(MANIFEST_FILE), serde_json::to_vec_pretty(&m).unwrap()).unwrap();
    assert_eq!(
        verify_dir(d.path(), &keys_for(&k), &Expect::default()).unwrap_err(),
        ManifestError::Signature(SigError::BadSignature)
    );
}

#[test]
fn unknown_key_is_refused() {
    let d = tempfile::tempdir().unwrap();
    let signer = SigningKey::generate().unwrap();
    let trusted = SigningKey::generate().unwrap();
    make_build(d.path(), &signer);
    match verify_dir(d.path(), &keys_for(&trusted), &Expect::default()).unwrap_err() {
        ManifestError::Signature(SigError::UnknownKey(id)) => assert_eq!(id, signer.key_id_hex()),
        e => panic!("wrong error {e}"),
    }
    // No trusted keys at all (a release build without a configured key).
    assert_eq!(
        verify_dir(d.path(), &TrustedKeys::default(), &Expect::default()).unwrap_err(),
        ManifestError::Signature(SigError::NoTrustedKeys)
    );
    // Rotation: trusting old + new accepts either.
    let both = TrustedKeys::parse(&[trusted.public_b64(), signer.public_b64()]).unwrap();
    assert!(verify_dir(d.path(), &both, &Expect::default()).is_ok());
}

#[test]
fn signature_over_other_bytes_or_with_wrong_comment_is_refused() {
    let k = SigningKey::generate().unwrap();
    let d = tempfile::tempdir().unwrap();
    let id = make_build(d.path(), &k);
    let bytes = fs::read(d.path().join(MANIFEST_FILE)).unwrap();
    // A valid signature whose trusted comment names another build (replaying an old signature).
    let sig = k.sign(&bytes, "build_id=000000000000000000000000 kind=worker").unwrap();
    fs::write(d.path().join(SIG_FILE), sig).unwrap();
    assert!(matches!(verify_dir(d.path(), &keys_for(&k), &Expect::default()), Err(ManifestError::TrustedComment(_))));
    // Right build id, wrong kind (an agent signature reused for a worker).
    let sig = k.sign(&bytes, &format!("build_id={id} kind=agent")).unwrap();
    fs::write(d.path().join(SIG_FILE), sig).unwrap();
    assert!(matches!(verify_dir(d.path(), &keys_for(&k), &Expect::default()), Err(ManifestError::TrustedComment(_))));
    // Garbage signature file.
    fs::write(d.path().join(SIG_FILE), "untrusted comment: x\nnot base64\n").unwrap();
    assert!(matches!(verify_dir(d.path(), &keys_for(&k), &Expect::default()), Err(ManifestError::Signature(SigError::Encoding(_)))));
    // Missing signature.
    fs::remove_file(d.path().join(SIG_FILE)).unwrap();
    assert!(matches!(verify_dir(d.path(), &keys_for(&k), &Expect::default()), Err(ManifestError::MissingFile(_))));
}

#[test]
fn expectations_are_enforced() {
    let k = SigningKey::generate().unwrap();
    let d = tempfile::tempdir().unwrap();
    let id = make_build(d.path(), &k);
    let keys = keys_for(&k);
    assert!(matches!(
        verify_dir(d.path(), &keys, &Expect { build_id: Some("ffffffffffffffffffffffff"), ..Default::default() }),
        Err(ManifestError::BuildIdMismatch { .. })
    ));
    assert!(matches!(
        verify_dir(d.path(), &keys, &Expect { kind: Some(Kind::Agent), ..Default::default() }),
        Err(ManifestError::KindMismatch { .. })
    ));
    assert!(verify_dir(d.path(), &keys, &Expect { version: Some("other"), ..Default::default() }).is_err());
    assert!(verify_dir(d.path(), &keys, &Expect { build_id: Some(&id), version: Some("mtg-60397f3"), kind: Some(Kind::Worker) }).is_ok());
}

#[test]
fn legacy_unhashed_signatures_are_refused() {
    // Build a legacy "Ed" signature by flipping the algorithm bytes of a real one: the verifier
    // must refuse the legacy mode outright rather than check it.
    let k = SigningKey::generate().unwrap();
    let data = b"{}";
    let sig = k.sign(data, &manifest::trusted_comment(&manifest::build_id(data), Kind::Runtime)).unwrap();
    use base64::Engine as _;
    let mut lines: Vec<String> = sig.lines().map(String::from).collect();
    let mut raw = base64::engine::general_purpose::STANDARD.decode(&lines[1]).unwrap();
    raw[1] = b'd';
    lines[1] = base64::engine::general_purpose::STANDARD.encode(&raw);
    let legacy = lines.join("\n");
    assert_eq!(keys_for(&k).verify(data, &legacy).unwrap_err(), SigError::BadSignature);
}

#[test]
fn secret_key_box_roundtrip_signs_identically_verifiable() {
    let k = SigningKey::generate().unwrap();
    let s = k.secret_box_string().unwrap();
    let k2 = SigningKey::from_secret_box_string(&s).unwrap();
    assert_eq!(k.public_b64(), k2.public_b64());
    let sig = k2.sign(b"x", "t").unwrap();
    assert!(keys_for(&k).verify(b"x", &sig).is_ok());
}
