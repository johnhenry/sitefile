//! `pattern` in schemas, a port of `src/pattern.ts`: a small regular expression engine (a subset of
//! JavaScript regular expressions with the `u` flag) that both implementations run identically, so they
//! accept the same patterns, report the same errors and match the same strings. Shared test vectors:
//! `rust/tests/fixtures/pattern/vectors.json`. Syntax and semantics are documented in `src/pattern.ts`
//! and docs/LANGUAGE.md. Matching is a Pike VM over code points: linear in the string length.

/// A compile error: `position` is the 1-based code point index in the pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternError {
    pub reason: String,
    pub position: usize,
}

impl std::fmt::Display for PatternError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "at character {}: {}", self.position, self.reason)
    }
}

type Range = (u32, u32);

/// A set of code points: ranges, or their complement.
#[derive(Debug, Clone, PartialEq)]
struct CharSet {
    ranges: Vec<Range>,
    negate: bool,
}

impl CharSet {
    fn single(c: u32) -> CharSet {
        CharSet {
            ranges: vec![(c, c)],
            negate: false,
        }
    }
    fn contains(&self, c: u32) -> bool {
        self.ranges.iter().any(|&(lo, hi)| c >= lo && c <= hi) != self.negate
    }
    fn negated(mut self) -> CharSet {
        self.negate = true;
        self
    }
}

fn digit() -> CharSet {
    CharSet {
        ranges: vec![(0x30, 0x39)],
        negate: false,
    }
}
fn word() -> CharSet {
    CharSet {
        ranges: vec![(0x30, 0x39), (0x41, 0x5a), (0x5f, 0x5f), (0x61, 0x7a)],
        negate: false,
    }
}
/// JavaScript's `\s`: WhiteSpace and LineTerminator.
fn space() -> CharSet {
    CharSet {
        ranges: vec![
            (0x09, 0x0d),
            (0x20, 0x20),
            (0xa0, 0xa0),
            (0x1680, 0x1680),
            (0x2000, 0x200a),
            (0x2028, 0x2029),
            (0x202f, 0x202f),
            (0x205f, 0x205f),
            (0x3000, 0x3000),
            (0xfeff, 0xfeff),
        ],
        negate: false,
    }
}
/// `.`: everything but line terminators.
fn dot() -> CharSet {
    CharSet {
        ranges: vec![(0x0a, 0x0a), (0x0d, 0x0d), (0x2028, 0x2029)],
        negate: true,
    }
}

const SYNTAX: &str = "^$\\.*+?()[]{}|/";
const MAX_COUNT: usize = 1000;
/// Compiled programs larger than this are refused (a {n,m} of a {n,m} …).
const MAX_PROGRAM: usize = 20000;

#[derive(Debug, Clone)]
enum Node {
    Set {
        items: Vec<CharSet>,
        negate: bool,
    },
    Bol,
    Eol,
    Cat(Vec<Node>),
    Alt(Vec<Node>),
    Rep {
        x: Box<Node>,
        min: usize,
        max: Option<usize>,
    },
}

struct Parser<'a> {
    cps: &'a [u32],
    i: usize,
}

type PResult<T> = std::result::Result<T, PatternError>;

impl Parser<'_> {
    fn peek(&self) -> Option<char> {
        self.cps.get(self.i).and_then(|&c| char::from_u32(c))
    }
    fn fail<T>(&self, reason: impl Into<String>, at: usize) -> PResult<T> {
        Err(PatternError {
            reason: reason.into(),
            position: at + 1,
        })
    }

    fn alt(&mut self) -> PResult<Node> {
        let mut xs = vec![self.cat()?];
        while self.peek() == Some('|') {
            self.i += 1;
            xs.push(self.cat()?);
        }
        Ok(if xs.len() == 1 {
            xs.pop().unwrap_or(Node::Cat(Vec::new()))
        } else {
            Node::Alt(xs)
        })
    }

    fn cat(&mut self) -> PResult<Node> {
        let mut xs = Vec::new();
        loop {
            match self.peek() {
                None | Some('|') | Some(')') => break,
                _ => {}
            }
            let start = self.i;
            let atom = self.atom()?;
            xs.push(self.quantified(atom, start)?);
        }
        Ok(if xs.len() == 1 {
            xs.pop().unwrap_or(Node::Cat(Vec::new()))
        } else {
            Node::Cat(xs)
        })
    }

    /// A quantifier after `atom` (which began at `start`), if any.
    fn quantified(&mut self, atom: Node, start: usize) -> PResult<Node> {
        let (min, max) = match self.peek() {
            Some('*') => {
                self.i += 1;
                (0, None)
            }
            Some('+') => {
                self.i += 1;
                (1, None)
            }
            Some('?') => {
                self.i += 1;
                (0, Some(1))
            }
            Some('{') => self.counted()?,
            _ => return Ok(atom),
        };
        if matches!(atom, Node::Bol | Node::Eol) {
            return self.fail("nothing to repeat", start);
        }
        if self.peek() == Some('?') {
            self.i += 1; // lazy: the same answer for a yes/no match
        }
        if matches!(self.peek(), Some('*' | '+' | '?' | '{')) {
            return self.fail("nothing to repeat", self.i);
        }
        Ok(Node::Rep {
            x: Box::new(atom),
            min,
            max,
        })
    }

    /// `{n}`, `{n,}` or `{n,m}` at `{`.
    fn counted(&mut self) -> PResult<(usize, Option<usize>)> {
        let open = self.i;
        self.i += 1;
        let num = |p: &mut Self| -> Option<usize> {
            let from = p.i;
            while matches!(p.peek(), Some(c) if c.is_ascii_digit()) {
                p.i += 1;
            }
            if p.i == from {
                return None;
            }
            if p.i - from > 4 {
                return Some(MAX_COUNT + 1);
            }
            Some(
                p.cps[from..p.i]
                    .iter()
                    .fold(0, |n, &d| n * 10 + (d - 0x30) as usize),
            )
        };
        let min = num(self);
        let mut max = min;
        if min.is_some() && self.peek() == Some(',') {
            self.i += 1;
            max = num(self);
        }
        let Some(min) = min.filter(|_| self.peek() == Some('}')) else {
            return self.fail(
                "'{' must be written \\{ unless it starts a repetition {n}, {n,} or {n,m}",
                open,
            );
        };
        self.i += 1;
        if min > MAX_COUNT || max.is_some_and(|m| m > MAX_COUNT) {
            return self.fail(format!("repetition count above {MAX_COUNT}"), open);
        }
        if max.is_some_and(|m| m < min) {
            return self.fail("numbers out of order in {} repetition", open);
        }
        Ok((min, max))
    }

    fn atom(&mut self) -> PResult<Node> {
        let start = self.i;
        let c = self.peek().unwrap_or('\0');
        self.i += 1;
        match c {
            '(' => {
                if self.peek() == Some('?') {
                    if self.cps.get(self.i + 1) != Some(&0x3a) {
                        return self.fail(
                            "unsupported group '(?' (only ( … ) and (?: … ) are supported)",
                            start,
                        );
                    }
                    self.i += 2;
                }
                let inner = self.alt()?;
                if self.peek() != Some(')') {
                    return self.fail("missing ')'", start);
                }
                self.i += 1;
                Ok(inner)
            }
            ')' => self.fail("unmatched ')'", start),
            '[' => self.cls(start),
            '.' => Ok(Node::Set {
                items: vec![dot()],
                negate: false,
            }),
            '^' => Ok(Node::Bol),
            '$' => Ok(Node::Eol),
            '\\' => Ok(Node::Set {
                items: vec![self.escape(false, start)?],
                negate: false,
            }),
            '*' | '+' | '?' => self.fail("nothing to repeat", start),
            '{' => self.fail(
                "'{' must be written \\{ unless it starts a repetition {n}, {n,} or {n,m}",
                start,
            ),
            '}' | ']' => self.fail(format!("'{c}' must be written \\{c}"), start),
            _ => Ok(Node::Set {
                items: vec![CharSet::single(self.cps[start])],
                negate: false,
            }),
        }
    }

    /// After `\` (at `start`): a class escape or a single character.
    fn escape(&mut self, in_class: bool, start: usize) -> PResult<CharSet> {
        let Some(c) = self.peek() else {
            return self.fail("pattern ends with '\\'", start);
        };
        self.i += 1;
        Ok(match c {
            'd' => digit(),
            'D' => digit().negated(),
            'w' => word(),
            'W' => word().negated(),
            's' => space(),
            'S' => space().negated(),
            'n' => CharSet::single(0x0a),
            'r' => CharSet::single(0x0d),
            't' => CharSet::single(0x09),
            _ if SYNTAX.contains(c) || (in_class && c == '-') => CharSet::single(c as u32),
            _ => return self.fail(format!("unsupported escape '\\{c}'"), start),
        })
    }

    /// A class after `[` (at `start`).
    fn cls(&mut self, start: usize) -> PResult<Node> {
        let mut negate = false;
        if self.peek() == Some('^') {
            negate = true;
            self.i += 1;
        }
        let mut items = Vec::new();
        loop {
            match self.peek() {
                None => return self.fail("unterminated character class", start),
                Some(']') => {
                    self.i += 1;
                    break;
                }
                _ => {}
            }
            let (a_set, a_cp, a_at) = self.class_atom()?;
            if self.peek() == Some('-') && self.cps.get(self.i + 1).is_some_and(|&c| c != 0x5d) {
                self.i += 1;
                let (_, b_cp, _) = self.class_atom()?;
                let (Some(a), Some(b)) = (a_cp, b_cp) else {
                    return self.fail("a class escape cannot start or end a range", a_at);
                };
                if b < a {
                    return self.fail("range out of order in character class", a_at);
                }
                items.push(CharSet {
                    ranges: vec![(a, b)],
                    negate: false,
                });
            } else {
                items.push(a_set);
            }
        }
        Ok(Node::Set { items, negate })
    }

    /// One class atom: a set (a class escape) or a single code point.
    fn class_atom(&mut self) -> PResult<(CharSet, Option<u32>, usize)> {
        let at = self.i;
        let c = self.cps[at];
        self.i += 1;
        if c == 0x5c {
            let set = self.escape(true, at)?;
            let single = !set.negate && set.ranges.len() == 1 && set.ranges[0].0 == set.ranges[0].1;
            let cp = single.then_some(set.ranges[0].0);
            return Ok((set, cp, at));
        }
        Ok((CharSet::single(c), Some(c), at))
    }
}

#[derive(Debug, Clone)]
enum Inst {
    Set { items: Vec<CharSet>, negate: bool },
    Bol,
    Eol,
    Match,
    Split(usize, usize),
    Jmp(usize),
}

/// A compiled pattern.
#[derive(Debug, Clone)]
pub struct Pattern {
    pub source: String,
    prog: Vec<Inst>,
}

fn too_large() -> PatternError {
    PatternError {
        reason: "pattern is too large".into(),
        position: 1,
    }
}

fn emit(n: &Node, prog: &mut Vec<Inst>) -> PResult<()> {
    if prog.len() > MAX_PROGRAM {
        return Err(too_large());
    }
    match n {
        Node::Set { items, negate } => prog.push(Inst::Set {
            items: items.clone(),
            negate: *negate,
        }),
        Node::Bol => prog.push(Inst::Bol),
        Node::Eol => prog.push(Inst::Eol),
        Node::Cat(xs) => {
            for x in xs {
                emit(x, prog)?;
            }
        }
        Node::Alt(xs) => {
            let mut jumps = Vec::new();
            for (i, x) in xs.iter().enumerate() {
                if i + 1 < xs.len() {
                    let split = prog.len();
                    prog.push(Inst::Split(split + 1, 0));
                    emit(x, prog)?;
                    jumps.push(prog.len());
                    prog.push(Inst::Jmp(0));
                    let next = prog.len();
                    prog[split] = Inst::Split(split + 1, next);
                } else {
                    emit(x, prog)?;
                }
            }
            let end = prog.len();
            for j in jumps {
                prog[j] = Inst::Jmp(end);
            }
        }
        Node::Rep { x, min, max } => {
            for _ in 0..*min {
                emit(x, prog)?;
            }
            match max {
                None => {
                    let at = prog.len();
                    prog.push(Inst::Split(at + 1, 0));
                    emit(x, prog)?;
                    prog.push(Inst::Jmp(at));
                    let end = prog.len();
                    prog[at] = Inst::Split(at + 1, end);
                }
                Some(max) => {
                    let mut splits = Vec::new();
                    for _ in *min..*max {
                        let at = prog.len();
                        prog.push(Inst::Split(at + 1, 0));
                        splits.push(at);
                        emit(x, prog)?;
                    }
                    let end = prog.len();
                    for s in splits {
                        prog[s] = Inst::Split(s + 1, end);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Compile a pattern.
pub fn compile_pattern(source: &str) -> PResult<Pattern> {
    let cps: Vec<u32> = source.chars().map(|c| c as u32).collect();
    let mut p = Parser { cps: &cps, i: 0 };
    let node = p.alt()?;
    if p.i < cps.len() {
        return p.fail("unmatched ')'", p.i);
    }
    let mut prog = Vec::new();
    emit(&node, &mut prog)?;
    if prog.len() > MAX_PROGRAM {
        return Err(too_large());
    }
    prog.push(Inst::Match);
    Ok(Pattern {
        source: source.to_string(),
        prog,
    })
}

impl Pattern {
    /// Whether the pattern matches somewhere in `text` (Pike VM over code points).
    pub fn matches(&self, text: &str) -> bool {
        let cps: Vec<u32> = text.chars().map(|c| c as u32).collect();
        let prog = &self.prog;
        let mut mark = vec![0u64; prog.len()];
        let mut gen = 0u64;
        // Follow jumps, splits and assertions from `pc` at `pos`; true when Match is reachable.
        let add =
            |list: &mut Vec<usize>, mark: &mut [u64], gen: u64, pc: usize, pos: usize| -> bool {
                let mut stack = vec![pc];
                while let Some(at) = stack.pop() {
                    if mark[at] == gen {
                        continue;
                    }
                    mark[at] = gen;
                    match &prog[at] {
                        Inst::Match => return true,
                        Inst::Jmp(x) => stack.push(*x),
                        Inst::Split(x, y) => {
                            stack.push(*y);
                            stack.push(*x);
                        }
                        Inst::Bol => {
                            if pos == 0 {
                                stack.push(at + 1);
                            }
                        }
                        Inst::Eol => {
                            if pos == cps.len() {
                                stack.push(at + 1);
                            }
                        }
                        Inst::Set { .. } => list.push(at),
                    }
                }
                false
            };
        let mut clist: Vec<usize> = Vec::new();
        let mut pos = 0;
        loop {
            gen += 1;
            // The start state at every position: an unanchored search.
            for &pc in &clist {
                mark[pc] = gen;
            }
            if add(&mut clist, &mut mark, gen, 0, pos) {
                return true;
            }
            if pos == cps.len() {
                return false;
            }
            let c = cps[pos];
            let mut next = Vec::new();
            gen += 1;
            for &pc in &clist {
                let Inst::Set { items, negate } = &prog[pc] else {
                    continue;
                };
                if items.iter().any(|s| s.contains(c)) != *negate
                    && add(&mut next, &mut mark, gen, pc + 1, pos + 1)
                {
                    return true;
                }
            }
            clist = next;
            pos += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// `rust/tests/fixtures/pattern/vectors.json`, shared with test/pattern.test.ts.
    #[test]
    fn shared_vectors() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/pattern/vectors.json"
        );
        let vectors: Vec<Value> =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert!(vectors.len() > 50);
        for v in &vectors {
            let src = v["pattern"].as_str().unwrap();
            match (compile_pattern(src), v.get("error")) {
                (Err(e), Some(want)) => assert_eq!(e.to_string(), want.as_str().unwrap(), "{src}"),
                (Err(e), None) => panic!("{src}: unexpected error {e}"),
                (Ok(_), Some(want)) => panic!("{src}: compiled, expected {want}"),
                (Ok(p), None) => {
                    for s in v["match"].as_array().into_iter().flatten() {
                        assert!(p.matches(s.as_str().unwrap()), "{src} should match {s}");
                    }
                    for s in v["reject"].as_array().into_iter().flatten() {
                        assert!(
                            !p.matches(s.as_str().unwrap()),
                            "{src} should not match {s}"
                        );
                    }
                }
            }
        }
    }

    /// `rust/tests/fixtures/pattern/fuzz.json`: random patterns and strings, as the TypeScript engine answers.
    #[test]
    fn differential_fuzz() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/pattern/fuzz.json"
        );
        let cases: Vec<Value> =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let mut bad = Vec::new();
        for c in &cases {
            let src = c["pattern"].as_str().unwrap();
            let got: Value = match compile_pattern(src) {
                Err(e) => Value::String(e.to_string()),
                Ok(p) => Value::Array(
                    c["inputs"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|s| Value::Bool(p.matches(s.as_str().unwrap())))
                        .collect(),
                ),
            };
            if got != c["result"] {
                bad.push(format!("{src:?}: want {} got {got}", c["result"]));
            }
        }
        assert!(
            bad.is_empty(),
            "{} of {} differ:\n{}",
            bad.len(),
            cases.len(),
            bad[..bad.len().min(10)].join("\n")
        );
    }

    #[test]
    fn linear_time_on_nested_quantifiers() {
        let p = compile_pattern("^(a+)+$").unwrap();
        let s = format!("{}b", "a".repeat(5000));
        assert!(!p.matches(&s));
    }
}
