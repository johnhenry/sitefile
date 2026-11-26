//! Configurable recipe file names (johnhenry/sitefile#332): the list searched when no `-f` is given.
//! The process-wide list is shared by every test in this binary, so the one test that sets it also
//! checks the default, and the others pass their list in `BuildOptions`.

use sitefile::error::ErrorCode;
use sitefile::executor::{build, plan_build, BuildOptions, LockfileSetting};
use sitefile::recipe::{
    find_recipe_name, recipe_names, resolve_recipe, resolve_recipe_with, set_recipe_names,
    DEFAULT_RECIPE_NAMES,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

const BIN: &str = env!("CARGO_BIN_EXE_sitefile");

struct Tmp(PathBuf);
impl Tmp {
    fn new() -> Tmp {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "sitefile-recipe-rs-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        Tmp(sitefile::platform::canonicalize(&p).unwrap())
    }
    fn write(&self, rel: &str, data: &str) {
        let p = self.0.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap();
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A context holding `a.txt` and one recipe per `(file name, text it writes)`.
fn site(t: &Tmp, recipes: &[(&str, &str)]) -> String {
    t.write("a.txt", "A");
    for (name, text) in recipes {
        t.write(
            name,
            &format!("FROM scratch\nCOPY ./a.txt /{text}.txt\nEXPORT /\n"),
        );
    }
    t.0.display().to_string()
}

fn names(v: &[&str]) -> Option<Vec<String>> {
    Some(v.iter().map(|s| s.to_string()).collect())
}

fn opts(ctx: &str, recipe_names: Option<Vec<String>>) -> BuildOptions {
    BuildOptions {
        context_dir: ctx.to_string(),
        recipe_names,
        lockfile: LockfileSetting::Disabled,
        store_home: Some(Path::new(ctx).join(".home")),
        ..Default::default()
    }
}

/// The exported file names.
fn built(o: &BuildOptions) -> Vec<String> {
    let r = build(o).unwrap();
    let mut v: Vec<String> = r.output.entries().map(|(p, _)| p.clone()).collect();
    v.sort();
    v
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

#[test]
fn the_default_is_sitefile_and_a_process_wide_list_is_used_in_order() {
    // No other test in this binary sets the list or the environment variable.
    assert_eq!(DEFAULT_RECIPE_NAMES, ["Sitefile"]);
    assert_eq!(recipe_names(), s(&["Sitefile"]));
    let t = Tmp::new();
    let ctx = site(&t, &[("Sitefile", "plain")]);
    assert_eq!(resolve_recipe(&ctx).unwrap(), t.0.join("Sitefile"));
    assert_eq!(built(&opts(&ctx, None)), ["/plain.txt"]);

    set_recipe_names(&["Stockerfile", "Sitefile"]);
    assert_eq!(recipe_names(), s(&["Stockerfile", "Sitefile"]));
    // Only a Sitefile: the second name is used.
    assert_eq!(resolve_recipe(&ctx).unwrap(), t.0.join("Sitefile"));
    assert_eq!(built(&opts(&ctx, None)), ["/plain.txt"]);
    // Both: the first wins, and nothing else changes.
    t.write(
        "Stockerfile",
        "FROM scratch\nCOPY ./a.txt /stocker.txt\nEXPORT /\n",
    );
    assert_eq!(resolve_recipe(&ctx).unwrap(), t.0.join("Stockerfile"));
    assert_eq!(built(&opts(&ctx, None)), ["/stocker.txt"]);
    assert!(plan_build(&opts(&ctx, None))
        .unwrap()
        .file
        .ends_with("Stockerfile"));
    // An explicit list on the options beats the process-wide one.
    assert_eq!(
        built(&opts(&ctx, names(&["Sitefile"]))),
        ["/plain.txt"],
        "BuildOptions::recipe_names"
    );
    // Invalid entries are dropped; nothing valid left restores the default.
    set_recipe_names(&["", "a/b", "..", "Stockerfile"]);
    assert_eq!(recipe_names(), s(&["Stockerfile"]));
    set_recipe_names::<&str>(&[]);
    assert_eq!(recipe_names(), s(&["Sitefile"]));
    assert_eq!(built(&opts(&ctx, None)), ["/plain.txt"]);
}

#[test]
fn names_are_tried_in_order() {
    let t = Tmp::new();
    let ctx = site(&t, &[("Third", "third"), ("Second", "second")]);
    let order = ["First", "Second", "Third"];
    assert_eq!(
        resolve_recipe_with(&ctx, &order).unwrap(),
        t.0.join("Second")
    );
    assert_eq!(built(&opts(&ctx, names(&order))), ["/second.txt"]);
    assert_eq!(
        find_recipe_name(&ctx, names(&order).as_deref()).as_deref(),
        Some("Second")
    );
    // The same files, another order.
    let order = ["Third", "Second"];
    assert_eq!(built(&opts(&ctx, names(&order))), ["/third.txt"]);
}

#[test]
fn a_directory_of_that_name_is_not_a_recipe() {
    let t = Tmp::new();
    let ctx = site(&t, &[("Sitefile", "plain")]);
    fs::create_dir(t.0.join("Stockerfile")).unwrap();
    assert_eq!(
        resolve_recipe_with(&ctx, &["Stockerfile", "Sitefile"]).unwrap(),
        t.0.join("Sitefile")
    );
}

#[test]
fn dash_f_wins_over_the_names() {
    let t = Tmp::new();
    let ctx = site(&t, &[("Stockerfile", "stocker"), ("Other", "other")]);
    let mut o = opts(&ctx, names(&["Stockerfile"]));
    o.sitefile = Some(format!("{ctx}/Other"));
    assert_eq!(built(&o), ["/other.txt"]);
    // And a missing -f file is its own error, not a search.
    o.sitefile = Some(format!("{ctx}/Missing"));
    let e = build(&o).err().unwrap();
    assert_eq!(e.code, ErrorCode::E_NOT_FOUND);
    assert!(
        e.message().starts_with("cannot read Sitefile '"),
        "{}",
        e.message()
    );
}

#[test]
fn the_error_names_the_files_tried() {
    let t = Tmp::new();
    let ctx = site(&t, &[]);
    let e = build(&opts(&ctx, names(&["Stockerfile", "Sitefile"])))
        .err()
        .unwrap();
    assert_eq!(e.code, ErrorCode::E_NOT_FOUND);
    assert_eq!(
        e.message(),
        format!("no recipe found in '{ctx}' (tried: Stockerfile, Sitefile)")
    );
    let e = resolve_recipe_with(&ctx, &["Stockerfile", "Sitefile"]).unwrap_err();
    assert_eq!(e.code, ErrorCode::E_NOT_FOUND);
    assert!(e.message().ends_with("(tried: Stockerfile, Sitefile)"));
    // The default list keeps the message it always had.
    let e = build(&opts(&ctx, names(DEFAULT_RECIPE_NAMES)))
        .err()
        .unwrap();
    assert_eq!(
        e.message(),
        format!("cannot read Sitefile '{ctx}/Sitefile'")
    );
}

// ------------------------------------------------------------------ the CLI

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

fn cli(ctx: &str, env: &[(&str, &str)], args: &[&str]) -> Out {
    let mut c = Command::new(BIN);
    c.args(args)
        .current_dir(ctx)
        .env("SITEFILE_HOME", Path::new(ctx).join(".home"))
        .env_remove("SITEFILE_RECIPE_NAMES");
    for (k, v) in env {
        c.env(k, v);
    }
    let o = c.output().unwrap();
    Out {
        code: o.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&o.stdout).into(),
        stderr: String::from_utf8_lossy(&o.stderr).into(),
    }
}

#[test]
fn the_cli_default_is_unchanged() {
    let t = Tmp::new();
    let ctx = site(&t, &[("Stockerfile", "stocker")]);
    // A Stockerfile is not a Sitefile without the opt-in.
    let o = cli(&ctx, &[], &["build", ".", "-o", "dist"]);
    assert_eq!(o.code, 1, "{}", o.stderr);
    assert!(o.stderr.contains("cannot read Sitefile '"), "{}", o.stderr);
    t.write(
        "Sitefile",
        "FROM scratch\nCOPY ./a.txt /plain.txt\nEXPORT /\n",
    );
    let o = cli(&ctx, &[], &["build", ".", "-o", "dist"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(t.0.join("dist/plain.txt").is_file());
}

#[test]
fn the_environment_variable_sets_the_list() {
    let t = Tmp::new();
    let ctx = site(&t, &[("Stockerfile", "stocker")]);
    let env = [("SITEFILE_RECIPE_NAMES", "Stockerfile, Sitefile")];
    let o = cli(&ctx, &env, &["build", ".", "-o", "dist"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(t.0.join("dist/stocker.txt").is_file());
    // validate and graph use the same list.
    let o = cli(&ctx, &env, &["validate", "."]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    // -f still wins over the variable.
    t.write("Other", "FROM scratch\nCOPY ./a.txt /other.txt\nEXPORT /\n");
    let o = cli(&ctx, &env, &["build", ".", "-f", "Other", "-o", "dist2"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(t.0.join("dist2/other.txt").is_file());
    // None of the names exists: the error names them.
    let empty = Tmp::new();
    let o = cli(
        &empty.0.display().to_string(),
        &[("SITEFILE_RECIPE_NAMES", "Stockerfile,Sitefile")],
        &["build", "."],
    );
    assert_eq!(o.code, 1);
    assert!(
        o.stderr.contains("(tried: Stockerfile, Sitefile)"),
        "{}",
        o.stderr
    );
}
