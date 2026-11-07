//! `sitefile bake` (port of `src/cli/bake.ts`): run the targets of a bake
//! file as `sitefile build` processes, up to `--jobs` at once, and print their
//! output in plan order.

use super::args::{flag, string, string_s, strings, OptSpec, Parsed};
use super::common::{err, is_safe_integer, js_to_number, out, pretty_json, usage, Command};
use crate::bake::{
    bake_plan, bake_plan_json, bake_plan_text, build_argv, find_bake_file, BakeTarget,
};
use crate::cache_limit::CACHE_MAX_ENV;
use crate::error::Result;
use crate::executor::join_path;
use std::collections::HashSet;
use std::process::Stdio;
use std::sync::{Condvar, Mutex};

const DEFAULT_JOBS: u64 = 4;

/// Flags passed unchanged to every target's build.
const PASS_BOOLEANS: &[&str] = &[
    "allow-plugins",
    "no-cache",
    "strict-pins",
    "frozen-lockfile",
    "update-lock",
    "no-lockfile",
    "allow-http",
    "allow-private-network",
    "allow-hardlinks",
    "force",
];
const PASS_STRINGS: &[&str] = &[
    "vendor-dir",
    "max-download-bytes",
    "timeout",
    "plugin-timeout",
];
const PASS_LISTS: &[&str] = &[
    "allow-exec",
    "allow-plugin",
    "cache-from",
    "insecure-registry",
    "build-arg-file",
    "build-arg",
];

const FLAGS: &[&str] = &[
    "-f, --file <path>             bake file (default: sitefile.bake.yaml, .yml or .json)",
    "--print                       print the resolved targets instead of building (text, or --json)",
    "--json                        with --print: machine-readable plan",
    "-j, --jobs <n>                builds to run at once (default 4)",
    "--profile <dir>               write each target's timing trace to <dir>/<target>.json",
    "--build-arg <NAME=value>      set an ARG in every target (wins over the bake file; repeatable)",
    "--build-arg-file <path>       read ARGs for every target from a file (after the targets' own; repeatable)",
    "Passed to every build: --allow-exec, --allow-plugin, --allow-plugins, --no-cache, --cache-from, --strict-pins,",
    "  --frozen-lockfile, --update-lock, --no-lockfile, --vendor-dir, --allow-http, --allow-private-network,",
    "  --allow-hardlinks, --max-download-bytes, --timeout, --plugin-timeout, --insecure-registry, --force",
];

const OPTS: &[OptSpec] = &[
    string_s("file", 'f'),
    flag("print"),
    flag("json"),
    string_s("jobs", 'j'),
    string("profile"),
    strings("cache-to"),
    flag("allow-plugins"),
    flag("no-cache"),
    flag("strict-pins"),
    flag("frozen-lockfile"),
    flag("update-lock"),
    flag("no-lockfile"),
    flag("allow-http"),
    flag("allow-private-network"),
    flag("allow-hardlinks"),
    flag("force"),
    string("vendor-dir"),
    string("max-download-bytes"),
    string("timeout"),
    string("plugin-timeout"),
    strings("allow-exec"),
    strings("allow-plugin"),
    strings("cache-from"),
    strings("insecure-registry"),
    strings("build-arg-file"),
    strings("build-arg"),
];

pub const BAKE: Command = Command {
    name: "bake",
    summary: "build several variants (targets, matrix, groups) from a bake file",
    usage: "bake [targets...] [-f sitefile.bake.yaml] [options]",
    description: "Build the targets of a bake file (default: sitefile.bake.yaml, .yml or .json in the current directory), like\n'docker buildx bake'. Each target is a 'sitefile build' with its own context, Sitefile, build args, --target\nstage, output and tags; a matrix expands one target into one per combination of values. Name targets or\ngroups to build only those (default: the 'default' group, else every target). Paths in the bake file are\nrelative to it. Up to --jobs builds run at once; each target's output is printed, prefixed with its\nname, in plan order. Every target runs even when one fails; then the exit code is 1.",
    flags: FLAGS,
    options: OPTS,
    run: run_bake,
};

fn positive(raw: Option<&str>, flag: &str) -> Result<Option<u64>> {
    let Some(raw) = raw else { return Ok(None) };
    let n = js_to_number(raw);
    if !is_safe_integer(n) || n <= 0.0 {
        return usage(format!("{flag} expects a positive integer"));
    }
    Ok(Some(n as u64))
}

/// The flags every target's build gets, in a fixed order.
fn pass_through(v: &Parsed) -> Vec<String> {
    let mut a = Vec::new();
    for f in PASS_BOOLEANS {
        if v.flag(f) {
            a.push(format!("--{f}"));
        }
    }
    for f in PASS_STRINGS {
        if let Some(s) = v.str(f) {
            a.extend([format!("--{f}"), s.to_string()]);
        }
    }
    for f in PASS_LISTS {
        for s in v.strs(f) {
            a.extend([format!("--{f}"), s]);
        }
    }
    a
}

struct Outcome {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run_build(argv: &[String]) -> Outcome {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            return Outcome {
                code: 1,
                stdout: String::new(),
                stderr: format!("error: cannot run the build: {e}\n"),
            }
        }
    };
    // Each target evicts nothing; bake applies the cache size limit once, at the end.
    match std::process::Command::new(exe)
        .args(argv)
        .env(CACHE_MAX_ENV, "0")
        .stdin(Stdio::null())
        .output()
    {
        Ok(o) => Outcome {
            code: o.status.code().unwrap_or(1),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        },
        Err(e) => Outcome {
            code: 1,
            stdout: String::new(),
            stderr: format!("error: cannot run the build: {e}\n"),
        },
    }
}

/// Each line of `text` with `[name] ` in front.
fn prefixed(name: &str, text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines.iter().map(|l| format!("[{name}] {l}\n")).collect()
}

/// `path.resolve(path.dirname(file), 'Sitefile.lock')`.
fn lock_key(t: &BakeTarget, v: &Parsed) -> Option<String> {
    if v.flag("no-lockfile") || v.flag("frozen-lockfile") {
        return None;
    }
    let file = if t.file.starts_with('/') {
        t.file.clone()
    } else {
        join_path(
            &std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy(),
            &t.file,
        )
    };
    Some(join_path(&file, "../Sitefile.lock"))
}

struct State {
    pending: Vec<usize>,
    busy: HashSet<String>,
    results: Vec<Option<Outcome>>,
}

fn run_bake(v: &Parsed) -> Result<i32> {
    if v.flag("json") && !v.flag("print") {
        return usage("bake: --json is only for --print");
    }
    if !v.strs("cache-to").is_empty() {
        return usage("bake: --cache-to is not supported (each target would replace the others' results); push from one 'sitefile build --cache-to'");
    }
    if v.flag("frozen-lockfile") && v.flag("update-lock") {
        return usage("--frozen-lockfile and --update-lock conflict");
    }
    if v.flag("strict-pins") && v.flag("update-lock") {
        return usage("--strict-pins and --update-lock conflict");
    }
    if v.flag("no-cache") && !v.strs("cache-from").is_empty() {
        return usage("--no-cache cannot be combined with --cache-from or --cache-to");
    }
    let jobs = positive(v.str("jobs"), "--jobs")?.unwrap_or(DEFAULT_JOBS) as usize;
    let profile = v.str("profile").map(str::to_string);
    if profile.as_deref() == Some("") {
        return usage("--profile expects a directory path");
    }
    let plan = bake_plan(&find_bake_file(v.str("file"))?, &v.positionals)?;
    if v.flag("print") {
        out(&if v.flag("json") {
            pretty_json(&bake_plan_json(&plan)) + "\n"
        } else {
            bake_plan_text(&plan)
        });
        return Ok(0);
    }
    if let Some(dir) = &profile {
        let _ = std::fs::create_dir_all(dir);
    }
    let extra = pass_through(v);
    let targets = &plan.targets;
    let keys: Vec<Option<String>> = targets.iter().map(|t| lock_key(t, v)).collect();
    let state = Mutex::new(State {
        pending: (0..targets.len()).collect(),
        busy: HashSet::new(),
        results: targets.iter().map(|_| None).collect(),
    });
    let changed = Condvar::new();
    std::thread::scope(|scope| {
        for _ in 0..jobs.min(targets.len()) {
            scope.spawn(|| loop {
                let mut st = state.lock().unwrap();
                let i = loop {
                    if st.pending.is_empty() {
                        return;
                    }
                    // The first pending target whose Sitefile.lock no running build may write.
                    let ready = st
                        .pending
                        .iter()
                        .position(|&i| keys[i].as_ref().is_none_or(|k| !st.busy.contains(k)));
                    match ready {
                        Some(k) => break st.pending.remove(k),
                        None => st = changed.wait(st).unwrap(),
                    }
                };
                if let Some(k) = &keys[i] {
                    st.busy.insert(k.clone());
                }
                drop(st);
                let t = &targets[i];
                let mut argv = build_argv(t);
                argv.extend(extra.iter().cloned());
                if let Some(dir) = &profile {
                    argv.extend([
                        "--profile".to_string(),
                        join_path(dir, &format!("{}.json", t.name)),
                    ]);
                }
                let r = run_build(&argv);
                let mut st = state.lock().unwrap();
                st.results[i] = Some(r);
                if let Some(k) = &keys[i] {
                    st.busy.remove(k);
                }
                changed.notify_all();
            });
        }
        // Print each target's output in plan order, as soon as it and every earlier target finished.
        let mut printed = 0;
        let mut st = state.lock().unwrap();
        while printed < targets.len() {
            while printed < targets.len() {
                let Some(r) = &st.results[printed] else { break };
                let name = &targets[printed].name;
                err(&prefixed(name, &r.stderr));
                out(&prefixed(name, &r.stdout));
                printed += 1;
            }
            if printed < targets.len() {
                st = changed.wait(st).unwrap();
            }
        }
    });
    super::cache::evict_after_build();
    let st = state.into_inner().unwrap();
    let failed: Vec<&str> = targets
        .iter()
        .zip(&st.results)
        .filter(|(_, r)| r.as_ref().is_none_or(|r| r.code != 0))
        .map(|(t, _)| t.name.as_str())
        .collect();
    let n = format!(
        "{} target{}",
        targets.len(),
        if targets.len() == 1 { "" } else { "s" }
    );
    if !failed.is_empty() {
        err(&format!(
            "error[E_BAKE]: {} of {n} failed: {}\n",
            failed.len(),
            failed.join(", ")
        ));
        return Ok(1);
    }
    out(&format!("Baked {n}\n"));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_lines() {
        assert_eq!(prefixed("a", ""), "");
        assert_eq!(prefixed("a", "x\ny\n"), "[a] x\n[a] y\n");
        assert_eq!(prefixed("a", "x"), "[a] x\n");
    }
}
