//! `.sitefileignore`: a gitignore-style subset (port of `src/resources/ignore.ts`).
//!
//! - Blank lines and lines starting with `#` are skipped (`\#` escapes a literal `#`).
//! - `!` negates a rule (`\!` escapes a literal `!`). The last matching rule wins.
//! - A trailing `/` makes the rule match directories only.
//! - A leading `/` or a `/` in the middle anchors the rule to the context root;
//!   otherwise it matches at any depth.
//! - `*` and `?` match within one path segment; `**` as a whole segment matches
//!   any number of segments.
//! - Unescaped trailing spaces are ignored. Character classes are not supported.
//!
//! As in gitignore, a file cannot be re-included once a parent directory is
//! excluded; callers get this by not descending into excluded directories.
//!
//! The TypeScript version compiles each rule to a RegExp; this one compiles to
//! the same small token language and matches it with a memoized backtracker.

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    /// One literal character.
    Lit(char),
    /// `[^/]*`
    Star,
    /// `[^/]`
    Question,
    /// `(?:[^/]+/)*` — a `**` segment that is not last.
    DirStar,
    /// `.+` — a trailing `**`.
    RestPlus,
    /// `(?:.*/)?` — the implicit prefix of an unanchored rule.
    AnyPrefix,
}

#[derive(Debug, Clone)]
struct Rule {
    negate: bool,
    dir_only: bool,
    toks: Vec<Tok>,
}

#[derive(Debug, Clone, Default)]
pub struct IgnoreRules {
    rules: Vec<Rule>,
}

impl IgnoreRules {
    pub fn empty() -> IgnoreRules {
        IgnoreRules::default()
    }

    pub fn parse(text: &str) -> IgnoreRules {
        // split(/\r?\n/)
        let rules = text
            .split('\n')
            .map(|l| l.strip_suffix('\r').unwrap_or(l))
            .filter_map(parse_line)
            .collect();
        IgnoreRules { rules }
    }

    pub fn size(&self) -> usize {
        self.rules.len()
    }

    /// Whether the context-relative POSIX path (no leading `./`) is excluded by the last matching rule.
    pub fn ignores(&self, rel: &str, is_directory: bool) -> bool {
        let text: Vec<char> = rel.chars().collect();
        let mut ignored = false;
        for r in &self.rules {
            if r.dir_only && !is_directory {
                continue;
            }
            if matches(&r.toks, &text) {
                ignored = !r.negate;
            }
        }
        ignored
    }
}

/// `raw.replace(/(?<!\\)\s+$/, '')`
fn strip_trailing_space(raw: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let mut s = chars.len();
    while s > 0 && chars[s - 1].is_whitespace() {
        s -= 1;
    }
    if s == chars.len() {
        return raw.to_string();
    }
    let cut = if s > 0 && chars[s - 1] == '\\' {
        // The first whitespace is escaped; the rest (if any) is stripped.
        if s + 1 < chars.len() { s + 1 } else { chars.len() }
    } else {
        s
    };
    chars[..cut].iter().collect()
}

fn parse_line(raw: &str) -> Option<Rule> {
    let mut line = strip_trailing_space(raw);
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut negate = false;
    if let Some(rest) = line.strip_prefix('!') {
        negate = true;
        line = rest.to_string();
    }
    let dir_only = line.ends_with('/');
    if dir_only {
        line = line.trim_end_matches('/').to_string();
    }
    let anchored = line.contains('/');
    let line = line.trim_start_matches('/');
    if line.is_empty() {
        return None;
    }
    let mut toks = Vec::new();
    if !anchored {
        toks.push(Tok::AnyPrefix);
    }
    let segs: Vec<&str> = line.split('/').collect();
    for (i, seg) in segs.iter().enumerate() {
        let last = i == segs.len() - 1;
        if *seg == "**" {
            // A trailing `**` needs at least one more segment; elsewhere it may match none.
            toks.push(if last { Tok::RestPlus } else { Tok::DirStar });
        } else {
            glob_segment(seg, &mut toks);
            if !last {
                toks.push(Tok::Lit('/'));
            }
        }
    }
    Some(Rule { negate, dir_only, toks })
}

fn glob_segment(seg: &str, out: &mut Vec<Tok>) {
    let c: Vec<char> = seg.chars().collect();
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '\\' if i + 1 < c.len() => {
                i += 1;
                out.push(Tok::Lit(c[i]));
            }
            '*' => {
                while i + 1 < c.len() && c[i + 1] == '*' {
                    i += 1; // `**` inside a segment behaves like `*`
                }
                out.push(Tok::Star);
            }
            '?' => out.push(Tok::Question),
            ch => out.push(Tok::Lit(ch)),
        }
        i += 1;
    }
}

/// Whole-string match of `toks` against `text`.
fn matches(toks: &[Tok], text: &[char]) -> bool {
    let w = text.len() + 1;
    let mut memo: Vec<Option<bool>> = vec![None; (toks.len() + 1) * w];
    fn go(p: usize, t: usize, toks: &[Tok], text: &[char], w: usize, memo: &mut Vec<Option<bool>>) -> bool {
        if let Some(v) = memo[p * w + t] {
            return v;
        }
        let n = text.len();
        let r = if p == toks.len() {
            t == n
        } else {
            match &toks[p] {
                Tok::Lit(c) => t < n && text[t] == *c && go(p + 1, t + 1, toks, text, w, memo),
                Tok::Question => t < n && text[t] != '/' && go(p + 1, t + 1, toks, text, w, memo),
                Tok::Star => {
                    let mut k = t;
                    loop {
                        if go(p + 1, k, toks, text, w, memo) {
                            break true;
                        }
                        if k >= n || text[k] == '/' {
                            break false;
                        }
                        k += 1;
                    }
                }
                Tok::DirStar => {
                    if go(p + 1, t, toks, text, w, memo) {
                        true
                    } else {
                        // one more `[^/]+/` group
                        match text[t..].iter().position(|&c| c == '/') {
                            Some(j) if j > 0 => go(p, t + j + 1, toks, text, w, memo),
                            _ => false,
                        }
                    }
                }
                Tok::RestPlus => (t + 1..=n).any(|k| go(p + 1, k, toks, text, w, memo)),
                Tok::AnyPrefix => {
                    go(p + 1, t, toks, text, w, memo)
                        || (t..n).any(|k| text[k] == '/' && go(p + 1, k + 1, toks, text, w, memo))
                }
            }
        };
        memo[p * w + t] = Some(r);
        r
    }
    go(0, 0, toks, text, w, &mut memo)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ig(text: &str, rel: &str) -> bool {
        IgnoreRules::parse(text).ignores(rel, false)
    }
    fn igd(text: &str, rel: &str) -> bool {
        IgnoreRules::parse(text).ignores(rel, true)
    }

    #[test]
    fn blank_lines_and_comments_are_skipped_escapes_make_them_literal() {
        let r = IgnoreRules::parse("\n# comment\n   \n\\#hash\n");
        assert_eq!(r.size(), 1);
        assert!(r.ignores("#hash", false));
        assert!(IgnoreRules::parse("\\!bang").ignores("!bang", false));
    }

    #[test]
    fn unanchored_match_any_depth_anchored_only_root() {
        assert!(ig("*.log", "a.log"));
        assert!(ig("*.log", "x/y/a.log"));
        assert!(!ig("*.log", "a.logx"));
        assert!(igd("/build", "build"));
        assert!(!igd("/build", "src/build"));
        assert!(ig("docs/tmp", "docs/tmp"));
        assert!(!ig("docs/tmp", "x/docs/tmp"));
    }

    #[test]
    fn trailing_slash_matches_directories_only() {
        assert!(igd("cache/", "cache"));
        assert!(igd("cache/", "a/cache"));
        assert!(!ig("cache/", "cache"));
    }

    #[test]
    fn star_and_question_do_not_cross_slashes() {
        assert!(ig("a/*.txt", "a/b.txt"));
        assert!(!ig("a/*.txt", "a/b/c.txt"));
        assert!(ig("f?.md", "fo.md"));
        assert!(!ig("f?.md", "f/.md"));
        assert!(!ig("f?.md", "f.md"));
    }

    #[test]
    fn double_star_matches_across_directories() {
        assert!(ig("**/foo", "foo"));
        assert!(ig("**/foo", "a/b/foo"));
        assert!(ig("a/**", "a/b/c"));
        assert!(!igd("a/**", "a"));
        assert!(ig("a/**/z", "a/z"));
        assert!(ig("a/**/z", "a/b/c/z"));
        assert!(!ig("a/**/z", "b/a/z"));
    }

    #[test]
    fn regex_metacharacters_are_literal() {
        assert!(ig("a+b(1).txt", "a+b(1).txt"));
        assert!(!ig("a.txt", "axtxt"));
    }

    #[test]
    fn last_matching_rule_wins_bang_reincludes() {
        let rules = "*.md\n!keep.md\n";
        assert!(ig(rules, "a.md"));
        assert!(!ig(rules, "keep.md"));
        assert!(ig("*.md\n!keep.md\nkeep.md", "keep.md"));
    }

    #[test]
    fn crlf_files_and_trailing_spaces() {
        assert!(ig("a.txt  \r\nb.txt\r\n", "a.txt"));
        assert!(ig("a.txt  \r\nb.txt\r\n", "b.txt"));
    }

    #[test]
    fn escaped_trailing_space_is_kept() {
        assert!(ig("a\\ ", "a "));
        assert!(ig("a\\   ", "a "));
        assert!(!ig("a\\ ", "a"));
    }

    #[test]
    fn empty_rules_ignore_nothing() {
        assert!(!IgnoreRules::empty().ignores("anything", false));
    }
}
