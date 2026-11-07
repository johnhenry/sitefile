//! The HTML/CSS reference scanner (`sitefile::links`) against the TypeScript reference: every case in
//! tests/fixtures/links/links.json (regenerate with `node rust/tests/fixtures/links/generate.mjs`).
//! `SITEFILE_LINKS_FUZZ=<file>` checks a larger generated file instead.

use serde_json::{json, Value};
use sitefile::fingerprint::hashed_name;
use sitefile::links::{resolve_reference, rewrite_references, scan_css, scan_html, Target};

fn target_text(t: &Target) -> String {
    match t {
        Target::None => "none".into(),
        Target::External(u) => format!("external {u}"),
        Target::Internal { path, dir } => format!(
            "internal {path}{}",
            if *dir && path != "/" { "/" } else { "" }
        ),
    }
}

fn record(kind: &str, input: &str) -> Value {
    let from = if kind == "html" {
        "/a/b.html"
    } else {
        "/a/b.css"
    };
    let refs = if kind == "html" {
        scan_html(input)
    } else {
        scan_css(input)
    };
    let rename = |p: &str| Some(hashed_name(p, "0123456789"));
    json!({
        "kind": kind,
        "input": input,
        "refs": refs
            .iter()
            .map(|r| json!([r.line, r.raw, r.url, r.css, target_text(&resolve_reference(&r.url, from, "/")), target_text(&resolve_reference(&r.url, from, "/a"))]))
            .collect::<Vec<_>>(),
        "rewritten": rewrite_references(input, &refs, from, "/", rename),
        "rewrittenBelow": rewrite_references(input, &refs, from, "/a", rename),
    })
}

#[test]
fn links_differential() {
    let file = std::env::var("SITEFILE_LINKS_FUZZ").unwrap_or_else(|_| {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/links/links.json"
        )
        .to_string()
    });
    let cases: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert!(cases.len() >= 300, "only {} cases", cases.len());
    let mut failed = 0;
    for c in &cases {
        let now = record(c["kind"].as_str().unwrap(), c["input"].as_str().unwrap());
        if &now != c {
            failed += 1;
            if failed <= 3 {
                eprintln!("expected {c}\n     got {now}\n");
            }
        }
    }
    assert_eq!(
        failed,
        0,
        "{failed} of {} cases differ from the TypeScript scanner",
        cases.len()
    );
}
