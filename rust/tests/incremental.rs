//! Incremental rebuilds (src/memo.rs), ported from test/incremental.test.ts: a
//! build with a memo must be byte-identical to a clean build (same files, modes,
//! contents and origins, same warnings, same errors) across random edits.

use sitefile::build_args::BuildArgs;
use sitefile::executor::{build, BuildOptions, BuildResult, Callback, LockfileSetting};
use sitefile::memo::BuildMemo;
use sitefile::profile::Profiler;
use sitefile::tree::Tree;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

struct Tmp(PathBuf);
impl Tmp {
    fn new() -> Tmp {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!("sitefile-incremental-rs-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        Tmp(fs::canonicalize(&p).unwrap())
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// xorshift32, as in bench/generate.mjs and the TypeScript test.
struct Rng(u32);
impl Rng {
    fn next(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x as f64 / 4294967296.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() * n as f64) as usize
    }
}

const WORDS: &[&str] = &["alpha", "beta", "gamma", "delta", "tree", "stage", "render", "import", "export", "digest", "page", "post", "feed"];

fn put(file: &Path, text: &str) {
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, text).unwrap();
}

struct Post {
    title: String,
    date: String,
    body: String,
    tags: Vec<String>,
}

/// A site using every memoized step: imports of each kind, stages, COPY, RENDER (FOR), FEED, SITEMAP, TRANSFORM.
struct Site {
    dir: PathBuf,
    r: Rng,
    posts: BTreeMap<String, Post>,
    public: BTreeMap<String, String>,
    post_template: usize,
    index_template: usize,
    site_title: String,
    rows: Vec<(String, u32)>,
    extra_line: bool,
    swapped: bool,
    bad_post: bool,
    arg: String,
    next: u32,
}

impl Site {
    fn new(dir: PathBuf, seed: u32) -> Site {
        let mut s = Site {
            dir,
            r: Rng(seed),
            posts: BTreeMap::new(),
            public: BTreeMap::new(),
            post_template: 0,
            index_template: 0,
            site_title: "Site".into(),
            rows: vec![("a".into(), 1), ("b".into(), 2)],
            extra_line: false,
            swapped: false,
            bad_post: false,
            arg: "one".into(),
            next: 0,
        };
        for _ in 0..4 {
            s.add_post();
        }
        for i in 0..3 {
            let w = s.words(5);
            s.public.insert(format!("d{}/f{i}.txt", i % 2), w);
        }
        fs::create_dir_all(s.dir.join("plugins")).unwrap();
        fs::copy(repo_root().join("examples/wasm/plugins/badge.wasm"), s.dir.join("plugins/badge.wasm")).unwrap();
        s.write();
        s
    }

    fn word(&mut self) -> String {
        WORDS[self.r.below(WORDS.len())].to_string()
    }

    fn words(&mut self, n: usize) -> String {
        (0..n).map(|_| self.word()).collect::<Vec<_>>().join(" ")
    }

    fn add_post(&mut self) {
        let i = self.next;
        self.next += 1;
        let day = format!("{:02}", 1 + i % 28);
        let title = format!("Post {i} {}", self.word());
        let body = self.words(8);
        let tags = vec![self.word()];
        self.posts.insert(format!("2026-01-{day}-p{i}.md"), Post { title, date: format!("2026-02-{day}"), body, tags });
    }

    fn write(&self) {
        let _ = fs::remove_dir_all(self.dir.join("posts"));
        let _ = fs::remove_dir_all(self.dir.join("public"));
        for (name, p) in &self.posts {
            put(&self.dir.join("posts").join(name), &format!("---\ntitle: \"{}\"\ndate: {}\ntags: [{}]\n---\n# {}\n\n{}\n", p.title, p.date, p.tags.join(", "), p.title, p.body));
        }
        if self.bad_post {
            put(&self.dir.join("posts/zz-bad.md"), "---\ntitle: [unclosed\n---\nbody\n");
        }
        for (name, text) in &self.public {
            put(&self.dir.join("public").join(name), text);
        }
        let post_templates = [
            "<h1>${post.title}</h1>${post.content|raw}<p>${post.date} ${SITE}</p>",
            "<article><h2>${post.title}</h2>${#each post.tags as t}<i>${t}</i>${/each}</article>",
            "<h1>${post.title}</h1><nav>${#each posts as o}<a href=\"/${o.slug}\">${o.title}</a>${/each}</nav>",
            "<h1>${post.title}</h1><footer>${site.title}</footer>",
        ];
        let index_templates = ["<ul>${#each posts as p}<li>${p.title}</li>${/each}</ul>", "<h1>${site.title}</h1>${#if posts}<p>posts</p>${/if}"];
        put(&self.dir.join("templates/post.html"), post_templates[self.post_template]);
        put(&self.dir.join("templates/index.html"), index_templates[self.index_template]);
        put(&self.dir.join("data/site.json"), &serde_json::json!({ "title": self.site_title }).to_string());
        let rows: Vec<String> = self.rows.iter().map(|(a, n)| format!("{a},{n}")).collect();
        put(&self.dir.join("data/rows.csv"), &format!("name,n\n{}\n", rows.join("\n")));
        let a = "COPY ./public/ /static/";
        let b = "COPY --from=assets /x/ /assets/";
        let mut lines = vec![
            "ARG SITE = \"S\"",
            "PLUGIN badge FROM ./plugins/badge.wasm",
            "IMPORT posts FROM ./posts AS markdown SORT BY date DESC",
            "IMPORT site FROM ./data/site.json AS json",
            "IMPORT rows FROM ./data/rows.csv AS csv",
            "IMPORT pub FROM ./public",
            "STAGE assets FROM scratch",
            "COPY pub:/ /x/",
            "FROM scratch",
        ];
        if self.swapped {
            lines.extend([b, a]);
        } else {
            lines.extend([a, b]);
        }
        if self.extra_line {
            lines.push("COPY ./data/site.json /site.json");
        }
        lines.extend([
            // A file, or a file in a directory once ./public has a d2/ (the input tree decides).
            "COPY ./data/rows.csv /static/d2",
            "RENDER --escape=html ./templates/index.html TO /index.html",
            "RENDER --escape=html ./templates/post.html TO /posts/${post.slug}.html FOR post IN posts",
            "RENDER <<T TO /rows/${row.name}.txt FOR row IN rows",
            "${row.name}=${row.n} ${SITE}",
            "T",
            "TRANSFORM WITH badge TO /badges/${post.slug}.svg FOR post IN posts",
            "FEED posts BASE https://example.com LINK /posts/${item.slug}.html TITLE \"Feed\"",
            "SITEMAP BASE https://example.com",
            "REDIRECT /old /index.html",
            "EXPORT /",
        ]);
        put(&self.dir.join("Sitefile"), &(lines.join("\n") + "\n"));
    }

    /// One random edit; returns what it did.
    fn mutate(&mut self) -> &'static str {
        const KINDS: &[&str] = &[
            "post-body", "post-title", "post-date", "add-post", "remove-post", "post-template", "index-template", "public-edit", "public-add",
            "public-remove", "site-json", "rows", "arg", "sitefile-line", "sitefile-swap", "bad-post", "nothing",
        ];
        let kind = KINDS[self.r.below(KINDS.len())];
        let names: Vec<String> = self.posts.keys().cloned().collect();
        let pick = |r: &mut Rng, xs: &[String]| xs[r.below(xs.len())].clone();
        match kind {
            "post-body" => {
                let n = pick(&mut self.r, &names);
                let w = self.words(6);
                self.posts.get_mut(&n).unwrap().body = w;
            }
            "post-title" => {
                let n = pick(&mut self.r, &names);
                let w = format!("T {}", self.words(2));
                self.posts.get_mut(&n).unwrap().title = w;
            }
            "post-date" => {
                let n = pick(&mut self.r, &names);
                let d = format!("2026-03-{:02}", 1 + self.r.below(28));
                self.posts.get_mut(&n).unwrap().date = d;
            }
            "add-post" => self.add_post(),
            "remove-post" => {
                if names.len() > 1 {
                    let n = pick(&mut self.r, &names);
                    self.posts.remove(&n);
                }
            }
            "post-template" => self.post_template = self.r.below(4),
            "index-template" => self.index_template = 1 - self.index_template,
            "public-edit" => {
                let k: Vec<String> = self.public.keys().cloned().collect();
                if !k.is_empty() {
                    let n = pick(&mut self.r, &k);
                    let w = self.words(4);
                    self.public.insert(n, w);
                }
            }
            "public-add" => {
                let d = self.r.below(3);
                let i = self.next;
                self.next += 1;
                let w = self.words(3);
                self.public.insert(format!("d{d}/n{i}.txt"), w);
            }
            "public-remove" => {
                let k: Vec<String> = self.public.keys().cloned().collect();
                if k.len() > 1 {
                    let n = pick(&mut self.r, &k);
                    self.public.remove(&n);
                }
            }
            "site-json" => self.site_title = self.words(2),
            "rows" => {
                for row in self.rows.iter_mut() {
                    if self.r.next() < 0.5 {
                        row.1 += 1;
                    }
                }
                if self.r.next() < 0.3 {
                    let i = self.next;
                    self.next += 1;
                    self.rows.push((format!("r{i}"), 0));
                }
            }
            "arg" => self.arg = self.word(),
            "sitefile-line" => self.extra_line = !self.extra_line,
            "sitefile-swap" => self.swapped = !self.swapped,
            "bad-post" => self.bad_post = !self.bad_post,
            _ => {}
        }
        self.write();
        kind
    }

    fn options(&self) -> BuildOptions {
        let mut args = BuildArgs::new();
        args.set("SITE", self.arg.clone());
        BuildOptions { context_dir: self.dir.to_string_lossy().into_owned(), lockfile: LockfileSetting::Disabled, build_args: args, ..Default::default() }
    }
}

#[derive(Debug, PartialEq)]
struct Outcome {
    files: Option<Vec<String>>,
    tree: Option<Vec<String>>,
    warnings: Vec<String>,
    error: Option<String>,
}

fn listing(t: &Tree) -> Vec<String> {
    t.entries().map(|(p, n)| format!("{p} {:o} {} {}", n.mode, n.digest, n.origin)).collect()
}

fn outcome(mut options: BuildOptions) -> (Outcome, Option<BuildResult>) {
    let warnings = Arc::new(Mutex::new(Vec::new()));
    let w = warnings.clone();
    let cb: Callback = Arc::new(move |m: &str| w.lock().unwrap().push(m.to_string()));
    options.on_warning = Some(cb);
    match build(&options) {
        Ok(r) => {
            let o = Outcome { files: Some(listing(&r.output)), tree: Some(listing(&r.tree)), warnings: warnings.lock().unwrap().clone(), error: None };
            (o, Some(r))
        }
        Err(e) => (Outcome { files: None, tree: None, warnings: warnings.lock().unwrap().clone(), error: Some(format!("{}: {}", e.code.as_str(), e.message())) }, None),
    }
}

fn random_edits(seed: u32) {
    let work = Tmp::new();
    let mut site = Site::new(work.0.join("ctx"), seed);
    let memo = BuildMemo::shared();
    // One seed also uses the TRANSFORM disk cache (shared by both builds, as in watch mode).
    let cache = (seed == 42).then(|| work.0.join("cache"));
    let mut reused = 0;
    let mut history: Vec<String> = Vec::new();
    for step in 0..60 {
        if step == 0 {
            history.push("initial".into());
        } else {
            // One to three edits between builds, like saving several files at once.
            let n = 1 + site.r.below(3);
            let edits: Vec<&str> = (0..n).map(|_| site.mutate()).collect();
            history.push(edits.join("+"));
        }
        let mut inc = site.options();
        inc.memo = Some(memo.clone());
        inc.cache_dir = cache.clone();
        let mut clean = site.options();
        clean.cache_dir = cache.clone();
        let (a, r) = outcome(inc);
        let (b, _) = outcome(clean);
        assert_eq!(a, b, "seed {seed}, step {step}: {}", history.join(" > "));
        if let Some(r) = r {
            reused += r.memo_stats.unwrap().hits;
        }
    }
    assert!(reused > 0);
}

#[test]
fn incremental_equals_clean_seed_1() {
    random_edits(1);
}

#[test]
fn incremental_equals_clean_seed_7() {
    random_edits(7);
}

#[test]
fn incremental_equals_clean_seed_42() {
    random_edits(42);
}

#[test]
fn incremental_equals_clean_seed_1234() {
    random_edits(1234);
}

#[test]
fn incremental_equals_clean_seed_99991() {
    random_edits(99991);
}

#[test]
fn an_edit_reruns_only_what_read_it() {
    let work = Tmp::new();
    let mut site = Site::new(work.0.join("ctx"), 3);
    let memo = BuildMemo::shared();
    let opts = |site: &Site| {
        let mut o = site.options();
        o.memo = Some(memo.clone());
        o
    };
    let first = build(&opts(&site)).unwrap();
    assert_eq!(first.memo_stats.unwrap().hits, 0);
    let again = build(&opts(&site)).unwrap();
    assert_eq!(again.memo_stats.unwrap().misses, 0);
    assert_eq!(again.transform_stats.plugin_calls, 0);
    assert_eq!(again.output.digest(), first.output.digest());

    site.post_template = 0;
    site.write();
    build(&opts(&site)).unwrap();
    let name = site.posts.keys().nth(1).unwrap().clone();
    site.posts.get_mut(&name).unwrap().body = "changed body".into();
    site.write();
    let profiler = Profiler::new("build");
    let mut o = opts(&site);
    o.profiler = Some(profiler.clone());
    let edited = build(&o).unwrap();
    let clean = build(&site.options()).unwrap();
    assert_eq!(edited.output.digest(), clean.output.digest());
    let trace: serde_json::Value = serde_json::from_str(&profiler.to_json()).unwrap();
    let states: Vec<String> = trace["traceEvents"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["cat"] == "operation")
        .map(|e| {
            let n = e["name"].as_str().unwrap();
            format!("{} {}", n.rsplit_once("Sitefile:").map(|(_, r)| r).unwrap_or(n), e["args"]["memo"].as_str().unwrap_or("-"))
        })
        .collect();
    assert_eq!(
        states,
        [
            "8 COPY hit",
            "10 COPY hit",
            "11 COPY hit",
            "12 COPY hit",
            "13 RENDER miss",
            "14 RENDER miss",
            "15 RENDER files",
            "18 TRANSFORM miss",
            "19 FEED miss",
            "20 SITEMAP miss"
        ]
    );
    assert_eq!(edited.transform_stats.plugin_calls, 1);
}

#[test]
fn watch_memo_respects_sitefile_no_incremental() {
    // Only this test touches the variable in this binary.
    std::env::remove_var("SITEFILE_NO_INCREMENTAL");
    assert!(sitefile::memo::watch_memo().is_some());
    std::env::set_var("SITEFILE_NO_INCREMENTAL", "0");
    assert!(sitefile::memo::watch_memo().is_some());
    std::env::set_var("SITEFILE_NO_INCREMENTAL", "1");
    assert!(sitefile::memo::watch_memo().is_none());
    std::env::remove_var("SITEFILE_NO_INCREMENTAL");
}
