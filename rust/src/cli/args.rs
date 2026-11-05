//! A small `node:util` `parseArgs` work-alike (strict mode, positionals
//! allowed): the same option syntax and the same error messages, so usage
//! errors read identically in both implementations.

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Bool,
    Str,
}

#[derive(Debug, Clone, Copy)]
pub struct OptSpec {
    pub name: &'static str,
    pub short: Option<char>,
    pub kind: Kind,
    pub multiple: bool,
}

pub const fn flag(name: &'static str) -> OptSpec {
    OptSpec { name, short: None, kind: Kind::Bool, multiple: false }
}
pub const fn flag_s(name: &'static str, short: char) -> OptSpec {
    OptSpec { name, short: Some(short), kind: Kind::Bool, multiple: false }
}
pub const fn string(name: &'static str) -> OptSpec {
    OptSpec { name, short: None, kind: Kind::Str, multiple: false }
}
pub const fn string_s(name: &'static str, short: char) -> OptSpec {
    OptSpec { name, short: Some(short), kind: Kind::Str, multiple: false }
}
pub const fn strings(name: &'static str) -> OptSpec {
    OptSpec { name, short: None, kind: Kind::Str, multiple: true }
}
pub const fn strings_s(name: &'static str, short: char) -> OptSpec {
    OptSpec { name, short: Some(short), kind: Kind::Str, multiple: true }
}

#[derive(Debug, Clone, Default)]
pub struct Parsed {
    values: HashMap<String, Vec<Option<String>>>,
    pub positionals: Vec<String>,
}

impl Parsed {
    /// A boolean option was given.
    pub fn flag(&self, name: &str) -> bool {
        self.values.contains_key(name)
    }
    /// The (last) value of a string option.
    pub fn str(&self, name: &str) -> Option<&str> {
        self.values.get(name).and_then(|v| v.last()).and_then(|v| v.as_deref())
    }
    /// Every value of a (multiple) string option.
    pub fn strs(&self, name: &str) -> Vec<String> {
        self.values.get(name).map(|v| v.iter().flatten().cloned().collect()).unwrap_or_default()
    }
    pub fn has(&self, name: &str) -> bool {
        self.values.contains_key(name)
    }
}

fn unknown(raw: &str) -> String {
    format!("Unknown option '{raw}'. To specify a positional argument starting with a '-', place it at the end of the command after '--', as in '-- \"{raw}\"")
}

fn short_and_long(o: &OptSpec) -> String {
    match o.short {
        Some(s) => format!("-{s}, --{}", o.name),
        None => format!("--{}", o.name),
    }
}

fn option_like(v: &str) -> bool {
    v.len() > 1 && v.starts_with('-')
}

fn store(p: &mut Parsed, o: &OptSpec, v: Option<String>) {
    let slot = p.values.entry(o.name.to_string()).or_default();
    if o.multiple {
        slot.push(v);
    } else {
        *slot = vec![v];
    }
}

/// Parse `args` against `options`. Errors carry Node's message text.
pub fn parse(args: &[String], options: &[OptSpec]) -> Result<Parsed, String> {
    let mut p = Parsed::default();
    let by_long = |n: &str| options.iter().find(|o| o.name == n);
    let by_short = |c: char| options.iter().find(|o| o.short == Some(c));
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        i += 1;
        if a == "--" {
            p.positionals.extend(args[i..].iter().cloned());
            break;
        }
        if let Some(body) = a.strip_prefix("--") {
            let (name, inline) = match body.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (body, None),
            };
            let Some(o) = by_long(name) else { return Err(unknown(&format!("--{name}"))) };
            match o.kind {
                Kind::Bool => {
                    if inline.is_some() {
                        return Err(format!("Option '--{name}' does not take an argument"));
                    }
                    store(&mut p, o, None);
                }
                Kind::Str => {
                    let v = match inline {
                        Some(v) => v,
                        None => {
                            let Some(next) = args.get(i) else {
                                return Err(format!("Option '{} <value>' argument missing", short_and_long(o)));
                            };
                            if option_like(next) {
                                return Err(format!(
                                    "Option '--{name}' argument is ambiguous.\nDid you forget to specify the option argument for '--{name}'?\nTo specify an option argument starting with a dash use '--{name}=-XYZ'."
                                ));
                            }
                            i += 1;
                            next.clone()
                        }
                    };
                    store(&mut p, o, Some(v));
                }
            }
            continue;
        }
        if a.len() > 1 && a.starts_with('-') {
            let chars: Vec<char> = a[1..].chars().collect();
            let mut k = 0;
            while k < chars.len() {
                let c = chars[k];
                k += 1;
                let Some(o) = by_short(c) else { return Err(unknown(&format!("-{c}"))) };
                match o.kind {
                    Kind::Bool => store(&mut p, o, None),
                    Kind::Str => {
                        if k < chars.len() {
                            let rest: String = chars[k..].iter().collect();
                            store(&mut p, o, Some(rest));
                            break;
                        }
                        let Some(next) = args.get(i) else {
                            return Err(format!("Option '{} <value>' argument missing", short_and_long(o)));
                        };
                        if option_like(next) {
                            return Err(format!(
                                "Option '-{c}' argument is ambiguous.\nDid you forget to specify the option argument for '-{c}'?\nTo specify an option argument starting with a dash use '--{}=-XYZ' or '-{c}-XYZ'.",
                                o.name
                            ));
                        }
                        i += 1;
                        store(&mut p, o, Some(next.clone()));
                    }
                }
            }
            continue;
        }
        p.positionals.push(a.clone());
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPTS: &[OptSpec] = &[string_s("out", 'o'), flag("force"), strings_s("tag", 't'), flag_s("help", 'h'), flag_s("detach", 'd'), flag("no-cache")];

    fn run(a: &[&str]) -> Result<Parsed, String> {
        parse(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>(), OPTS)
    }

    #[test]
    fn node_compatible() {
        assert!(run(&["--bogus"]).unwrap_err().starts_with("Unknown option '--bogus'. To specify"));
        assert_eq!(run(&["--out"]).unwrap_err(), "Option '-o, --out <value>' argument missing");
        assert!(run(&["-o", "--force"]).unwrap_err().ends_with("use '--out=-XYZ' or '-o-XYZ'."));
        assert_eq!(run(&["--force=1"]).unwrap_err(), "Option '--force' does not take an argument");
        let p = run(&["-dofoo"]).unwrap();
        assert!(p.flag("detach") && p.str("out") == Some("foo"));
        assert_eq!(run(&["-od"]).unwrap().str("out"), Some("d"));
        assert_eq!(run(&["a", "--", "--force"]).unwrap().positionals, ["a", "--force"]);
        assert_eq!(run(&["-o", "-"]).unwrap().str("out"), Some("-"));
        assert_eq!(run(&["--tag=a", "-tb"]).unwrap().strs("tag"), ["a", "b"]);
        assert_eq!(run(&["-o=x"]).unwrap().str("out"), Some("=x"));
        assert!(run(&["-xd"]).unwrap_err().starts_with("Unknown option '-x'"));
    }
}
