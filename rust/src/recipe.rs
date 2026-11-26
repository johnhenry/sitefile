//! The default recipe file name (johnhenry/sitefile#332).
//!
//! Without `-f`, a build reads the recipe from the context directory. Which file that is comes from an
//! ordered list of names, `["Sitefile"]` by default; the first one that exists in the directory is used.
//! Code that embeds the crate can change the list (an app with its own recipe name, `["Stockerfile",
//! "Sitefile"]`), for one build through `BuildOptions::recipe_names` or for the whole process with
//! [`set_recipe_names`]. A CLI user can opt in with `$SITEFILE_RECIPE_NAMES` (comma-separated).
//! An explicit `-f` always wins and never reaches this module.
//!
//! The list in force is, in order: the one given for this call, the one set with [`set_recipe_names`],
//! `$SITEFILE_RECIPE_NAMES`, then [`DEFAULT_RECIPE_NAMES`].
//!
//! Only the recipe is renamed: `Sitefile.lock` and the `IMPORT … AS sitefile` lookup keep their names.

use crate::error::{ErrorCode::E_NOT_FOUND, Result, SitefileError};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// The names searched when nothing else is configured.
pub const DEFAULT_RECIPE_NAMES: &[&str] = &["Sitefile"];

/// `$SITEFILE_RECIPE_NAMES`: comma-separated recipe names, searched in order.
pub const RECIPE_NAMES_ENV: &str = "SITEFILE_RECIPE_NAMES";

static PROCESS_NAMES: RwLock<Option<Vec<String>>> = RwLock::new(None);

/// A recipe name is a bare file name: not empty, no path separator, not `.` or `..`.
fn is_valid_name(n: &str) -> bool {
    !n.is_empty() && n != "." && n != ".." && !n.contains(['/', '\\', '\0'])
}

/// The valid names of `names`, trimmed, without repeats, in order.
fn clean<S: AsRef<str>>(names: &[S]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in names {
        let n = n.as_ref().trim();
        if is_valid_name(n) && !out.iter().any(|x| x == n) {
            out.push(n.to_string());
        }
    }
    out
}

/// Parse a comma-separated list (`$SITEFILE_RECIPE_NAMES`), dropping empty and invalid entries.
pub fn parse_recipe_names(list: &str) -> Vec<String> {
    clean(&list.split(',').collect::<Vec<_>>())
}

/// Set the recipe names for the whole process, replacing an earlier call. Entries that are not bare
/// file names (empty, with a `/` or `\`, `.`, `..`) are dropped; a list with none left restores the
/// default (the environment, then [`DEFAULT_RECIPE_NAMES`]). Call it once at startup, before
/// `sitefile::cli::main` or a build, and again in a re-executed worker (`__serve-worker`) if that
/// does not go through the same startup.
pub fn set_recipe_names<S: AsRef<str>>(names: &[S]) {
    let v = clean(names);
    if let Ok(mut g) = PROCESS_NAMES.write() {
        *g = if v.is_empty() { None } else { Some(v) };
    }
}

/// The names in force for a call that gave none: [`set_recipe_names`], else `$SITEFILE_RECIPE_NAMES`,
/// else [`DEFAULT_RECIPE_NAMES`]. Never empty.
pub fn recipe_names() -> Vec<String> {
    if let Some(v) = PROCESS_NAMES.read().ok().and_then(|g| g.clone()) {
        return v;
    }
    if let Some(raw) = std::env::var_os(RECIPE_NAMES_ENV) {
        let v = parse_recipe_names(&raw.to_string_lossy());
        if !v.is_empty() {
            return v;
        }
    }
    DEFAULT_RECIPE_NAMES.iter().map(|s| s.to_string()).collect()
}

/// `names` when given and non-empty after cleaning, else [`recipe_names`].
pub(crate) fn effective(names: Option<&[String]>) -> Vec<String> {
    match names.map(clean).filter(|v| !v.is_empty()) {
        Some(v) => v,
        None => recipe_names(),
    }
}

/// Whether `names` is exactly the default list (the one whose "not found" message is the original one).
pub(crate) fn is_default(names: &[String]) -> bool {
    names.len() == DEFAULT_RECIPE_NAMES.len()
        && names.iter().zip(DEFAULT_RECIPE_NAMES).all(|(a, b)| a == b)
}

/// The first of `names` (this call's list, else the one in force) that is a file in `dir`.
pub fn find_recipe_name(dir: impl AsRef<Path>, names: Option<&[String]>) -> Option<String> {
    let dir = dir.as_ref();
    effective(names).into_iter().find(|n| dir.join(n).is_file())
}

/// `no recipe found in '<dir>' (tried: A, B)`; without a directory (a Git checkout has none worth
/// showing) `no recipe found (tried: A, B)`.
pub(crate) fn not_found(dir: Option<&str>, names: &[String]) -> SitefileError {
    let tried = names.join(", ");
    SitefileError::plain(
        E_NOT_FOUND,
        match dir {
            Some(d) => format!("no recipe found in '{d}' (tried: {tried})"),
            None => format!("no recipe found (tried: {tried})"),
        },
    )
}

/// The recipe of `context_dir`: the first of [`recipe_names`] that exists there. `E_NOT_FOUND` naming
/// every file tried when none does.
pub fn resolve_recipe(context_dir: impl AsRef<Path>) -> Result<PathBuf> {
    resolve_recipe_with(context_dir, &recipe_names())
}

/// [`resolve_recipe`] with an explicit list (an empty one means the list in force).
pub fn resolve_recipe_with<S: AsRef<str>>(
    context_dir: impl AsRef<Path>,
    names: &[S],
) -> Result<PathBuf> {
    let dir = context_dir.as_ref().to_string_lossy().into_owned();
    let names = effective(Some(
        &names
            .iter()
            .map(|s| s.as_ref().to_string())
            .collect::<Vec<_>>(),
    ));
    match names.iter().find(|n| Path::new(&dir).join(n).is_file()) {
        Some(n) => Ok(PathBuf::from(crate::executor::join_path(&dir, n))),
        None => Err(not_found(Some(&dir), &names)),
    }
}

/// The recipe path a build of `context_dir` reads: the first existing name, else the first name (so
/// the read that follows fails naming it). A string, as the executor's paths are.
pub(crate) fn recipe_path(context_dir: &str, names: Option<&[String]>) -> String {
    let names = effective(names);
    let name = names
        .iter()
        .find(|n| Path::new(context_dir).join(n).is_file())
        .unwrap_or(&names[0]);
    crate::executor::join_path(context_dir, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_cleans_lists() {
        assert_eq!(
            parse_recipe_names(" Stockerfile, ,Sitefile,Stockerfile,a/b,..,x\\y"),
            ["Stockerfile", "Sitefile"]
        );
        assert!(parse_recipe_names("").is_empty());
        assert!(is_default(&["Sitefile".to_string()]));
        assert!(!is_default(&[
            "Stockerfile".to_string(),
            "Sitefile".to_string()
        ]));
    }

    #[test]
    fn explicit_names_beat_the_default() {
        let n = vec!["Mine".to_string()];
        assert_eq!(effective(Some(&n)), ["Mine"]);
        assert_eq!(effective(Some(&[])), recipe_names());
    }
}
