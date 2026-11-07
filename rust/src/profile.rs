//! `--profile <file>`: a timing trace of one build in the Chrome trace event
//! format (port of `src/profile.ts`; same events, names, categories and args).
//!
//! Every event is a complete event (`ph: "X"`, microseconds) on process 1. The
//! build runs on thread 1; import N (0-based, in Sitefile order) on thread N+2.
//! Events appear in the order they started in the Sitefile (imports in
//! declaration order); only `ts` and `dur` differ between runs.

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

struct Slot {
    name: String,
    cat: &'static str,
    tid: u64,
    args: Map<String, Value>,
    ts: Option<u64>,
    dur: Option<u64>,
}

#[derive(Default)]
struct Inner {
    slots: Vec<Slot>,
    threads: BTreeMap<u64, String>,
}

pub struct Profiler {
    command: String,
    t0: Instant,
    inner: Mutex<Inner>,
}

/// Arguments of an event, in insertion order.
pub type Args = Vec<(&'static str, Value)>;

/// One event; ends when `end` is called (dropping it without `end` leaves it open,
/// which the trace reports as `incomplete`).
pub struct Span<'a> {
    profiler: Option<&'a Profiler>,
    index: usize,
}

impl Span<'_> {
    /// A span that records nothing (no profiler).
    pub fn none() -> Span<'static> {
        Span {
            profiler: None,
            index: 0,
        }
    }

    /// Start the clock of a reserved span.
    pub fn start(&self) {
        if let Some(p) = self.profiler {
            let now = p.now();
            if let Ok(mut i) = p.inner.lock() {
                i.slots[self.index].ts = Some(now);
            }
        }
    }

    pub fn end(&self, args: Args) {
        if let Some(p) = self.profiler {
            let now = p.now();
            if let Ok(mut i) = p.inner.lock() {
                let s = &mut i.slots[self.index];
                let Some(ts) = s.ts else { return };
                if s.dur.is_some() {
                    return;
                }
                s.dur = Some(now.saturating_sub(ts));
                for (k, v) in args {
                    s.args.insert(k.to_string(), v);
                }
            }
        }
    }
}

impl Profiler {
    pub fn new(command: &str) -> Arc<Profiler> {
        let mut inner = Inner::default();
        inner.threads.insert(1, "build".into());
        Arc::new(Profiler {
            command: command.to_string(),
            t0: Instant::now(),
            inner: Mutex::new(inner),
        })
    }

    fn now(&self) -> u64 {
        self.t0.elapsed().as_micros() as u64
    }

    /// Reserve an event's place in the file now; its clock starts with `start()`.
    pub fn reserve(
        &self,
        name: impl Into<String>,
        cat: &'static str,
        args: Args,
        tid: u64,
    ) -> Span<'_> {
        let mut m = Map::new();
        for (k, v) in args {
            m.insert(k.to_string(), v);
        }
        let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        i.slots.push(Slot {
            name: name.into(),
            cat,
            tid,
            args: m,
            ts: None,
            dur: None,
        });
        Span {
            profiler: Some(self),
            index: i.slots.len() - 1,
        }
    }

    /// Start an event now, on the build thread.
    pub fn begin(&self, name: impl Into<String>, cat: &'static str, args: Args) -> Span<'_> {
        let s = self.reserve(name, cat, args, 1);
        s.start();
        s
    }

    pub fn name_thread(&self, tid: u64, name: impl Into<String>) {
        if let Ok(mut i) = self.inner.lock() {
            i.threads.insert(tid, name.into());
        }
    }

    /// The trace as JSON text, one event per line (same layout as the TypeScript CLI).
    pub fn to_json(&self) -> String {
        let now = self.now();
        let i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut lines = vec![json!({ "name": "process_name", "ph": "M", "pid": 1, "tid": 1, "args": { "name": format!("sitefile {}", self.command) } }).to_string()];
        for (tid, name) in &i.threads {
            lines.push(json!({ "name": "thread_name", "ph": "M", "pid": 1, "tid": tid, "args": { "name": name } }).to_string());
        }
        for s in &i.slots {
            let Some(ts) = s.ts else { continue };
            let mut args = s.args.clone();
            if s.dur.is_none() {
                args.insert("incomplete".into(), Value::Bool(true));
            }
            let dur = s.dur.unwrap_or(now.saturating_sub(ts));
            lines.push(json!({ "name": s.name, "cat": s.cat, "ph": "X", "ts": ts, "dur": dur, "pid": 1, "tid": s.tid, "args": args }).to_string());
        }
        format!(
            "{{\"traceEvents\":[\n{}\n],\n\"displayTimeUnit\":\"ms\"}}\n",
            lines.join(",\n")
        )
    }
}

/// `begin` on an optional profiler.
pub fn begin<'a>(
    p: Option<&'a Profiler>,
    name: impl FnOnce() -> String,
    cat: &'static str,
    args: impl FnOnce() -> Args,
) -> Span<'a> {
    match p {
        Some(p) => p.begin(name(), cat, args()),
        None => Span::none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_in_reserved_order_and_open_ones_incomplete() {
        let p = Profiler::new("build");
        let a = p.begin("a", "build", vec![("x", json!(1))]);
        let b = p.reserve("b", "import", vec![], 2);
        p.name_thread(2, "import b");
        let c = p.reserve("never", "import", vec![], 3);
        b.start();
        b.end(vec![("kind", json!("file"))]);
        a.end(vec![("x", json!(2)), ("y", json!(true))]);
        let open = p.begin("open", "operation", vec![]);
        let _ = (&c, &open);
        let v: Value = serde_json::from_str(&p.to_json()).unwrap();
        let ev = v["traceEvents"].as_array().unwrap();
        let names: Vec<_> = ev.iter().map(|e| e["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "process_name",
                "thread_name",
                "thread_name",
                "a",
                "b",
                "open"
            ]
        );
        assert_eq!(ev[3]["args"], json!({ "x": 2, "y": true }));
        assert_eq!(ev[4]["tid"], json!(2));
        assert_eq!(ev[5]["args"], json!({ "incomplete": true }));
        assert_eq!(v["displayTimeUnit"], "ms");
    }
}
