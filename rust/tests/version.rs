//! The version is package.json's (johnhenry/sitefile#146): Cargo.toml's `version`, which the
//! binary reports through CARGO_PKG_VERSION, must equal it, so a release bump that forgets
//! rust/Cargo.toml fails here (test/version.test.ts checks the same from the npm side).

use std::path::Path;
use std::process::Command;

fn package_json_version() -> String {
    let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("../package.json");
    let pkg: serde_json::Value = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
    pkg["version"].as_str().unwrap().to_string()
}

#[test]
fn cargo_version_matches_package_json() {
    assert_eq!(
        env!("CARGO_PKG_VERSION"),
        package_json_version(),
        "rust/Cargo.toml's version must equal package.json's"
    );
    assert_eq!(
        sitefile::export::attest::SITEFILE_VERSION,
        package_json_version()
    );
}

#[test]
fn version_flag_prints_package_json_version() {
    let out = Command::new(env!("CARGO_BIN_EXE_sitefile"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("{}\n", package_json_version())
    );
}
