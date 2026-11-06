//! Incremental rebuilds (port of `src/memo.ts`): results of build steps,
//! memoized in memory by content digests of everything each step reads.
//!
//! Keys are SHA-256 digests of a step's identity (the planned operation with
//! its Sitefile position, the ARG/LET values) and the digests of its inputs.
//! A step whose key is unchanged returns its previous result without running.
//! Failures are never memoized; entries keep the warnings a step printed so a
//! hit prints them again. The memo lives in memory only, for one watch session;
//! entries not used by the latest build (or the last successful one, while
//! builds fail) are dropped. Keys are internal: they need not match the
//! TypeScript implementation's, only the outputs must.

use crate::digest::sha256;
use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Steps reused and steps run in one build.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoStats {
    pub hits: u64,
    pub misses: u64,
}

struct Entry {
    gen: u64,
    value: Arc<dyn Any + Send + Sync>,
    /// Entries computed inside this one (FOR elements, documents): reusing it uses them too.
    children: Vec<String>,
}

#[derive(Default)]
pub struct BuildMemo {
    entries: HashMap<String, Entry>,
    gen: u64,
    good_gen: u64,
    pub stats: MemoStats,
}

/// A memo shared by a watch session's builds (imports resolve on threads).
pub type SharedMemo = Arc<Mutex<BuildMemo>>;

impl BuildMemo {
    pub fn new() -> BuildMemo {
        BuildMemo::default()
    }

    pub fn shared() -> SharedMemo {
        Arc::new(Mutex::new(BuildMemo::new()))
    }

    /// Start a build: new statistics, a new generation.
    pub fn begin(&mut self) {
        self.gen += 1;
        self.stats = MemoStats::default();
    }

    /// End a build: keep what it used (and, after a failure, what the last good build used).
    pub fn finish(&mut self, ok: bool) {
        if ok {
            self.good_gen = self.gen;
        }
        let keep = self.gen.min(self.good_gen);
        self.entries.retain(|_, e| e.gen >= keep);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn touch(&mut self, key: &str) {
        let gen = self.gen;
        let mut stack = vec![key.to_string()];
        while let Some(k) = stack.pop() {
            if let Some(e) = self.entries.get_mut(&k) {
                if e.gen == gen {
                    continue;
                }
                e.gen = gen;
                stack.extend(e.children.iter().cloned());
            }
        }
    }

    /// The value memoized under `key` (marking it, and what is inside it, as used).
    pub fn get<T: Clone + 'static>(&mut self, key: &str) -> Option<T> {
        let v = self.entries.get(key)?.value.downcast_ref::<T>()?.clone();
        self.touch(key);
        self.stats.hits += 1;
        Some(v)
    }

    pub fn set<T: Send + Sync + 'static>(&mut self, key: String, value: T, children: Vec<String>) {
        self.stats.misses += 1;
        self.entries.insert(key, Entry { gen: self.gen, value: Arc::new(value), children });
    }
}

/// `get`, then `compute(children)` and `set` on a miss, without holding the lock while computing.
pub fn memoize<T, E>(memo: &Mutex<BuildMemo>, key: &str, parent: Option<&Mutex<Vec<String>>>, compute: impl FnOnce(&Mutex<Vec<String>>) -> Result<T, E>) -> Result<T, E>
where
    T: Clone + Send + Sync + 'static,
{
    if let Some(p) = parent {
        if let Ok(mut p) = p.lock() {
            p.push(key.to_string());
        }
    }
    if let Some(hit) = lock(memo).get::<T>(key) {
        return Ok(hit);
    }
    let children = Mutex::new(Vec::new());
    let v = compute(&children)?;
    lock(memo).set(key.to_string(), v.clone(), children.into_inner().unwrap_or_default());
    Ok(v)
}

pub fn lock(memo: &Mutex<BuildMemo>) -> std::sync::MutexGuard<'_, BuildMemo> {
    memo.lock().unwrap_or_else(|e| e.into_inner())
}

/// A memo key: the digest of its parts (joined unambiguously).
pub fn memo_key(parts: &[&str]) -> String {
    let mut s = String::new();
    for p in parts {
        s.push_str(&p.len().to_string());
        s.push(':');
        s.push_str(p);
    }
    sha256(s.as_bytes())
}

/// Set to a non-empty value other than `0` to make watch mode rebuild everything every time.
pub const NO_INCREMENTAL_ENV: &str = "SITEFILE_NO_INCREMENTAL";

/// The memo for a watch session: None when $SITEFILE_NO_INCREMENTAL turns it off.
pub fn watch_memo() -> Option<SharedMemo> {
    match std::env::var(NO_INCREMENTAL_ENV) {
        Ok(v) if !v.is_empty() && v != "0" => None,
        _ => Some(BuildMemo::shared()),
    }
}

/// Per-document memo for Markdown and data collections: `(key parts, compute)`.
pub type DocMemo<'a> = &'a dyn Fn(&[&str], &mut dyn FnMut() -> crate::error::Result<serde_json::Map<String, serde_json::Value>>) -> crate::error::Result<serde_json::Map<String, serde_json::Value>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prunes_unused_entries_but_not_while_builds_fail() {
        let mut m = BuildMemo::new();
        m.begin();
        m.set("a".into(), 1u32, vec!["c".into()]);
        m.set("b".into(), 2u32, vec![]);
        m.set("c".into(), 3u32, vec![]);
        m.finish(true);
        m.begin();
        assert_eq!(m.get::<u32>("a"), Some(1)); // touches its child c too
        m.finish(true);
        assert_eq!(m.len(), 2);
        m.begin();
        m.set("d".into(), 4u32, vec![]);
        m.finish(false);
        assert_eq!(m.len(), 3);
        m.begin();
        m.finish(true);
        assert!(m.is_empty());
    }

    #[test]
    fn keys_separate_parts() {
        assert_ne!(memo_key(&["ab", "c"]), memo_key(&["a", "bc"]));
        assert_eq!(memo_key(&["x"]), memo_key(&["x"]));
    }
}
