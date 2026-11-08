//! Path portability (port of `src/portable.ts`; docs/LANGUAGE.md, "Portable paths"): names that work
//! locally but break on other systems or hosts. Messages match the TypeScript implementation.

/// Windows device names: a file named so (with any extension) cannot be created there.
fn reserved(stem: &str) -> bool {
    if matches!(stem, "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    for prefix in ["COM", "LPT"] {
        if let Some(n) = stem.strip_prefix(prefix) {
            let mut c = n.chars();
            if let (Some(d), None) = (c.next(), c.next()) {
                if ('1'..='9').contains(&d) || matches!(d, '¹' | '²' | '³') {
                    return true;
                }
            }
        }
    }
    false
}

/// Why a path cannot be written on Windows, or None (`windowsProblem` in the TypeScript module).
pub fn windows_problem(path: &str) -> Option<String> {
    for seg in path.split('/').skip(1) {
        if let Some(c) = seg
            .chars()
            .find(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '\\') || (*c as u32) < 0x20)
        {
            let shown = if (c as u32) < 0x20 {
                format!("U+{:04X}", c as u32)
            } else {
                format!("'{c}'")
            };
            return Some(format!("{shown} is not allowed in a Windows file name"));
        }
        if seg.ends_with('.') || seg.ends_with(' ') {
            let what = if seg.ends_with('.') { "a dot" } else { "a space" };
            return Some(format!("'{seg}' ends with {what}, which Windows drops"));
        }
        let stem = seg.split('.').next().unwrap_or("").trim_end_matches(' ');
        if reserved(&stem.to_uppercase()) {
            return Some(format!("'{seg}' is a reserved device name on Windows"));
        }
    }
    None
}

/// Portability problems of the exported files (`portabilityProblems` in the TypeScript module); `paths`
/// are below the export root, in UTF-8 order.
pub fn portability_problems(paths: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for p in paths {
        if let Some(why) = windows_problem(p) {
            out.push(format!("'{p}' cannot be extracted on Windows: {why}"));
        }
    }
    if !paths.iter().any(|p| p == "/.nojekyll") {
        let hidden: Vec<&String> = paths
            .iter()
            .filter(|p| {
                p.as_str() != "/_headers"
                    && p.as_str() != "/_redirects"
                    && p.split('/').any(|s| s.starts_with('_'))
            })
            .collect();
        if !hidden.is_empty() {
            let mut shown = hidden
                .iter()
                .take(3)
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            if hidden.len() > 3 {
                shown.push_str(&format!(", … ({} in all)", hidden.len()));
            }
            out.push(format!("GitHub Pages (with Jekyll) does not publish files or folders whose name starts with '_' ({shown}); if the site is hosted there, add an empty /.nojekyll"));
        }
    }
    out
}
