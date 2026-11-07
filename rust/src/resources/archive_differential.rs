//! Differential archive / web bundle reader fuzzing against the TypeScript
//! reference (`tests/fixtures/archive/generate.mjs`, format in `record.mjs`).
//!
//! For every recorded archive (tar, tar.gz, zip, web bundle b1/b2; built and
//! then mutated from a seed), read with the recorded limits: the tree (paths,
//! modes, digests) or the error (code and message) must equal TypeScript's,
//! the web bundle's parsed entries must too, nothing may panic, and an
//! accepted tree must stay within the limits.

use crate::error::SitefileError;
use crate::resources::archive::extract_archive;
use crate::resources::types::ArchiveLimits;
use crate::resources::wbn_reader::{is_web_bundle, read_web_bundle};
use base64::Engine;
use serde_json::{json, Value};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::{Duration, Instant};

const ORIGIN: &str = "fuzz.bin";
/// Generous: the slowest recorded case (a few MB inflated) takes milliseconds in a release build.
const SLOW: Duration = Duration::from_secs(20);

fn error(e: &SitefileError) -> Value {
    json!({ "error": [e.code.as_str(), e.detail] })
}

fn panics<T>(what: &str, f: impl FnOnce() -> T) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(f)).map_err(|p| {
        let msg = p
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default();
        format!("{what} panicked: {msg}")
    })
}

/// Every disagreement, panic or limit violation for one recorded case.
fn check(index: usize, case: &Value) -> Vec<String> {
    let input = case["input"].as_str().expect("input");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(input)
        .expect("base64 input");
    let l = &case["limits"];
    let limits = ArchiveLimits {
        max_entries: l[0].as_u64().expect("maxEntries"),
        max_total_bytes: l[1].as_u64().expect("maxTotalBytes"),
        max_ratio: l[2].as_u64().expect("maxRatio"),
    };
    let short = if input.len() > 120 {
        format!("{}…", &input[..120])
    } else {
        input.to_string()
    };
    let label = format!("case {index} ({} bytes, limits {l}) {short}", bytes.len());
    let mut bad = Vec::new();

    let started = Instant::now();
    let got = match panics("extract_archive", || {
        extract_archive(&bytes, ORIGIN, &limits, None)
    }) {
        Err(m) => return vec![format!("{label}\n  {m}")],
        Ok(Err(e)) => error(&e),
        Ok(Ok(tree)) => {
            let files: Vec<_> = tree
                .entries()
                .map(|(p, n)| json!([p, n.mode, n.digest]))
                .collect();
            let total: u64 = tree.entries().map(|(_, n)| n.bytes.len() as u64).sum();
            if files.len() as u64 > limits.max_entries {
                bad.push(format!(
                    "{label}\n  limit violated: {} files > {}",
                    files.len(),
                    limits.max_entries
                ));
            }
            if total > limits.max_total_bytes {
                bad.push(format!(
                    "{label}\n  limit violated: {total} bytes > {}",
                    limits.max_total_bytes
                ));
            }
            json!({ "ok": files })
        }
    };
    if started.elapsed() > SLOW {
        bad.push(format!("{label}\n  took {:?}", started.elapsed()));
    }
    if got != case["result"] {
        bad.push(format!("{label}\n  want {}\n  got  {got}", case["result"]));
    }

    if is_web_bundle(&bytes) {
        let got = match panics("read_web_bundle", || {
            read_web_bundle(&bytes, ORIGIN, &limits, None)
        }) {
            Err(m) => return vec![format!("{label}\n  {m}")],
            Ok(Err(_)) => Value::Null,
            Ok(Ok(b)) => json!({
                "version": b.version,
                "primary": b.primary_url,
                "entries": b.entries.iter().map(|e| json!([e.url, e.status, e.headers.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>(), crate::digest::sha256(&e.body)])).collect::<Vec<_>>(),
            }),
        };
        if got != case["bundle"] {
            bad.push(format!(
                "{label}\n  bundle want {}\n  bundle got  {got}",
                case["bundle"]
            ));
        }
    }
    bad
}

fn run(cases: &[Value]) {
    let mut failures = Vec::new();
    for (i, case) in cases.iter().enumerate() {
        let r = &case["result"];
        assert!(
            r.get("crash").is_none() && r.get("violation").is_none(),
            "TypeScript bug recorded for case {i}: {r}"
        );
        failures.extend(check(i, case));
    }
    let shown: Vec<_> = failures.iter().take(15).cloned().collect();
    assert!(
        failures.is_empty(),
        "{} disagreements in {} archives:\n{}",
        failures.len(),
        cases.len(),
        shown.join("\n")
    );
}

fn load(path: &std::path::Path) -> Vec<Value> {
    serde_json::from_str(
        &std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())),
    )
    .unwrap()
}

/// The committed fixture (`tests/fixtures/archive/fuzz.json`).
#[test]
fn archive_differential_fixtures() {
    let cases = load(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/archive/fuzz.json"),
    );
    assert!(cases.len() >= 300, "only {} cases", cases.len());
    let ok = cases
        .iter()
        .filter(|c| c["result"].get("ok").is_some())
        .count();
    assert!(ok >= 50, "only {ok} archives are accepted");
    run(&cases);
}

/// A larger, locally generated fuzz file (see generate.mjs).
#[test]
#[ignore]
fn archive_differential_external() {
    let Ok(path) = std::env::var("SITEFILE_ARCHIVE_FUZZ") else {
        return;
    };
    let cases = load(std::path::Path::new(&path));
    run(&cases);
}
