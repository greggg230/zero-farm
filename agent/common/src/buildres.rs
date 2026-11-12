//! Included by build scripts (`#[path]`, next to `icon.rs` and `winres.rs` as `crate::icon` and
//! `crate::winres`): writes `<bin>.res` (round "Z" icon + VERSIONINFO with the crate version) and
//! links it into that binary. MSVC only; other toolchains get no icon.

pub fn embed(bin: &str, description: &str) {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    let out = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join(format!("{bin}.res"));
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let res = crate::winres::app_res(description, &version, &|b| miniz_oxide::deflate::compress_to_vec_zlib(b, 10));
    std::fs::write(&out, res).unwrap();
    println!("cargo:rustc-link-arg-bin={bin}={}", out.display());
}

pub fn rerun_if_changed() {
    for f in ["buildres.rs", "icon.rs", "winres.rs"] {
        println!("cargo:rerun-if-changed=../common/src/{f}");
    }
}
