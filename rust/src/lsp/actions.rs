//! Code actions (quick fixes) for lint warnings and the unnamed-stage warning.
//!
//! - `render-escape`: add `--escape=html` after `RENDER`;
//! - `unpinned-import`: add `INTEGRITY <digest>` from the `Sitefile.lock` next to the document,
//!   when it has an entry for the URL (the only file the server reads; never the network);
//! - `unpinned-image`: append `@sha256:…` to the reference from the lockfile's `images` entry;
//! - unnamed stage: name it (`FROM … AS stageN`, or a `STAGE stageN` line before an implicit stage);
//! - every lint rule: disable it for this instruction or for the file with a `# sitefile-lint:` comment.

use crate::base_image::base_ref;
use crate::lint::LINT_RULES;
use crate::lsp::diagnostics::{Analysis, Diagnostic};
use crate::lsp::index::Index;
use crate::lsp::text::{range, Lines};
use crate::planner::ImportLocation;
use crate::resources::lockfile::Lockfile;
use serde_json::{json, Value};

/// The unnamed-stage planner warning starts with this text.
pub const UNNAMED_STAGE: &str = "unnamed stage is not the last one";

fn edit(uri: &str, line: u32, col: u32, text: &str) -> Value {
    json!({ "changes": { uri: [{ "range": range(line, col, line, col), "newText": text }] } })
}

fn action(title: String, d: &Diagnostic, uri: &str, line: u32, col: u32, text: &str, preferred: bool) -> Value {
    let mut a = json!({ "title": title, "kind": "quickfix", "diagnostics": [d.to_json()], "edit": edit(uri, line, col, text) });
    if preferred {
        a["isPreferred"] = json!(true);
    }
    a
}

/// A stage name no definition uses yet: `stage1`, `stage2`, …
fn free_stage_name(ix: &Index) -> String {
    (1..).map(|n| format!("stage{n}")).find(|n| !ix.defs.iter().any(|d| &d.name == n)).unwrap_or_default()
}

/// Quick fixes for the diagnostics on lines `first..=last` (0-based).
pub fn code_actions(lines: &Lines, ix: &Index, analysis: &Analysis, uri: &str, first: u32, last: u32, lock: Option<&Lockfile>) -> Value {
    let mut out = Vec::new();
    for d in analysis.diagnostics.iter().filter(|d| d.line >= first && d.line <= last) {
        let Some(ins) = ix.instr_at(d.line).map(|i| &ix.instrs[i]) else { continue };
        let (Some(head), Some(tail)) = (ins.words.first(), ins.words.last()) else { continue };
        let rule = d.code.filter(|c| LINT_RULES.contains(c));
        match rule {
            Some("render-escape") => {
                out.push(action("Add --escape=html".into(), d, uri, head.line, head.end, " --escape=html", true));
            }
            Some("unpinned-import") => {
                let url = analysis.plan.as_ref().and_then(|p| {
                    p.imports.iter().find(|i| i.span.line == d.line + 1).and_then(|i| match &i.location {
                        ImportLocation::Remote { url } => Some(url.clone()),
                        ImportLocation::Local { .. } => None,
                    })
                });
                if let Some(entry) = url.as_deref().and_then(|u| lock.and_then(|l| l.get(u))) {
                    let text = format!(" INTEGRITY {}", entry.integrity);
                    out.push(action("Add INTEGRITY from Sitefile.lock".into(), d, uri, tail.line, tail.end, &text, true));
                }
            }
            Some("unpinned-image") => {
                // The reference as written on the line (a literal word), and its tag's digest in Sitefile.lock.
                // The word after `image` (FROM / STAGE … FROM) or `BASE` (EXPORT).
                let after = ins.words.iter().position(|w| w.text.eq_ignore_ascii_case("image") || w.text.eq_ignore_ascii_case("BASE"));
                let pin = after.and_then(|i| ins.words.get(i + 1)).and_then(|w| {
                    let key = base_ref(&w.text, None).ok()?.lock_key;
                    let digest = &lock?.get_image(&key)?.digest;
                    (!w.text.contains('@')).then(|| (w, format!("@{digest}")))
                });
                if let Some((w, text)) = pin {
                    out.push(action("Pin the digest from Sitefile.lock".into(), d, uri, w.line, w.end, &text, true));
                }
            }
            _ => {}
        }
        if d.code.is_none() && d.message.starts_with(UNNAMED_STAGE) {
            let name = free_stage_name(ix);
            if ins.keyword == "FROM" {
                out.push(action(format!("Name this stage '{name}'"), d, uri, tail.line, tail.end, &format!(" AS {name}"), true));
            } else {
                // The implicit FROM scratch stage starts at this instruction: start a named one instead.
                let indent: String = lines.get(ins.start_line).chars().take_while(|c| *c == ' ' || *c == '\t').collect();
                out.push(action(format!("Name this stage '{name}'"), d, uri, ins.start_line, 0, &format!("{indent}STAGE {name}\n"), true));
            }
        }
        if let Some(id) = rule {
            let indent: String = lines.get(ins.start_line).chars().take_while(|c| *c == ' ' || *c == '\t').collect();
            let next = format!("{indent}# sitefile-lint: disable-next-line={id}\n");
            out.push(action(format!("Disable {id} for this instruction"), d, uri, ins.start_line, 0, &next, false));
            out.push(action(format!("Disable {id} in this file"), d, uri, 0, 0, &format!("# sitefile-lint: disable={id}\n"), false));
        }
    }
    Value::Array(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::diagnostics::analyze;
    use crate::lsp::index::build;

    fn titles_and_edits(src: &str, line: u32, lock: Option<&Lockfile>) -> Vec<(String, Value)> {
        let lines = Lines::new(src);
        let ix = build(&lines);
        let a = analyze(src, "Sitefile");
        let v = code_actions(&lines, &ix, &a, "file:///S", line, line, lock);
        v.as_array().unwrap().iter().map(|a| (a["title"].as_str().unwrap().to_string(), a["edit"]["changes"]["file:///S"][0].clone())).collect()
    }

    #[test]
    fn render_escape_and_disable_actions() {
        let src = "RENDER <<H TO /a.html\nx\nH\nEXPORT /";
        let got = titles_and_edits(src, 0, None);
        let titles: Vec<&str> = got.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(titles, ["Add --escape=html", "Disable render-escape for this instruction", "Disable render-escape in this file"]);
        assert_eq!(got[0].1, json!({ "range": range(0, 6, 0, 6), "newText": " --escape=html" }));
        assert_eq!(got[1].1["newText"], "# sitefile-lint: disable-next-line=render-escape\n");
        assert_eq!(got[2].1["newText"], "# sitefile-lint: disable=render-escape\n");
    }

    #[test]
    fn name_the_unnamed_stage() {
        let got = titles_and_edits("FROM scratch\nCOPY <<H /a\nx\nH\nFROM scratch\nEXPORT /", 0, None);
        assert_eq!(got[0].0, "Name this stage 'stage1'");
        assert_eq!(got[0].1, json!({ "range": range(0, 12, 0, 12), "newText": " AS stage1" }));
        // The implicit stage: a STAGE line before its first instruction.
        let got = titles_and_edits("COPY <<H /a\nx\nH\nFROM scratch\nEXPORT /", 0, None);
        assert_eq!(got[0].1, json!({ "range": range(0, 0, 0, 0), "newText": "STAGE stage1\n" }));
    }

    #[test]
    fn pins_from_the_lockfile() {
        let dir = std::env::temp_dir().join(format!("sitefile-actions-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let digest = format!("sha256:{}", "c".repeat(64));
        std::fs::write(
            dir.join("Sitefile.lock"),
            format!(
                "{{\"version\":1,\"resources\":{{\"https://e.x/a.json\":{{\"integrity\":\"{digest}\",\"size\":1,\"resolved\":\"https://e.x/a.json\"}}}},\"images\":{{\"ghcr.io/a/b:1\":{{\"digest\":\"{digest}\"}}}}}}"
            ),
        )
        .unwrap();
        let lock = Lockfile::load(dir.join("Sitefile.lock")).unwrap();
        let src = "IMPORT r FROM https://e.x/a.json AS json\nFROM image ghcr.io/a/b:1\nRENDER --escape=html <<H TO /a.html\n${r.x}\nH\nEXPORT /";
        let find = |line: u32, lock: Option<&Lockfile>, title: &str| titles_and_edits(src, line, lock).into_iter().find(|(t, _)| t == title).map(|(_, e)| e);
        let e = find(0, Some(&lock), "Add INTEGRITY from Sitefile.lock").unwrap();
        assert_eq!(e, json!({ "range": range(0, 40, 0, 40), "newText": format!(" INTEGRITY {digest}") }));
        let e = find(1, Some(&lock), "Pin the digest from Sitefile.lock").unwrap();
        assert_eq!(e, json!({ "range": range(1, 24, 1, 24), "newText": format!("@{digest}") }));
        // Without a lockfile entry there is nothing to pin.
        assert!(find(0, None, "Add INTEGRITY from Sitefile.lock").is_none() && find(1, None, "Pin the digest from Sitefile.lock").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
