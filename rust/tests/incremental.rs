//! Incremental rebuilds (src/memo.rs), ported from test/incremental.test.ts: a
//! build with a memo must be byte-identical to a clean build (same files, modes,
//! contents and origins, same warnings, same errors) across random edits.

use sitefile::build_args::BuildArgs;
use sitefile::digest::sha256;
use sitefile::executor::{build, build_to_directory, BuildOptions, BuildResult, Callback, ExportTarget, LockfileSetting};
use sitefile::export::exporter::{export_directory, ExportOptions};
use sitefile::export::incremental::{Applied, ExportState, SharedExportState};
use sitefile::memo::BuildMemo;
use sitefile::profile::Profiler;
use sitefile::tree::{make_file, Tree};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
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
    draft: bool,
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
    /// --build-arg DRAFTS (IMPORT … DRAFTS ${DRAFTS}).
    drafts: bool,
    /// schema.yaml requires a key no post has.
    bad_schema: bool,
    /// --target: build only the `assets` stage.
    target: Option<String>,
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
            drafts: false,
            bad_schema: false,
            target: None,
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
        self.posts.insert(format!("2026-01-{day}-p{i}.md"), Post { title, date: format!("2026-02-{day}"), body, tags, draft: false });
    }

    fn write(&self) {
        let _ = fs::remove_dir_all(self.dir.join("posts"));
        let _ = fs::remove_dir_all(self.dir.join("public"));
        for (name, p) in &self.posts {
            let draft = if p.draft { "draft: true\n" } else { "" };
            put(
                &self.dir.join("posts").join(name),
                &format!("---\ntitle: \"{}\"\ndate: {}\ntags: [{}]\n{draft}---\n# {}\n\n{}\n", p.title, p.date, p.tags.join(", "), p.title, p.body),
            );
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
        put(
            &self.dir.join("schema.yaml"),
            if self.bad_schema { "required: [missing]\n" } else { "required: [title]\nproperties:\n  tags: {type: list, items: string}\n  draft: boolean\n" },
        );
        let rows: Vec<String> = self.rows.iter().map(|(a, n)| format!("{a},{n}")).collect();
        put(&self.dir.join("data/rows.csv"), &format!("name,n\n{}\n", rows.join("\n")));
        let a = "COPY ./public/ /static/";
        let b = "COPY --from=assets /x/ /assets/";
        let mut lines = vec![
            "ARG SITE = \"S\"",
            "ARG DRAFTS = \"false\"",
            "PLUGIN badge FROM ./plugins/badge.wasm",
            "IMPORT posts FROM ./posts AS markdown SORT BY date DESC SCHEMA ./schema.yaml DRAFTS ${DRAFTS}",
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
            // Tag pages (two posts a page) and an index paginated by three.
            "RENDER --escape=html <<T TO /tags/${t.key}/${t.number}.html FOR t IN posts BY tags PAGE 2 FIRST /tags/${t.key}/index.html",
            "${t.key} ${t.number}/${t.total}:${#each t.items as p}${p.title}${/each} ${t.prev} ${t.next}",
            "T",
            "RENDER --escape=html <<T TO /page/${page.number}.html FOR page IN posts PAGE 3",
            "${#each page.items as p}<a>${p.title}</a>${/each}${page.next}",
            "T",
            "REDIRECT /old /index.html",
            "EXPORT /",
        ]);
        put(&self.dir.join("Sitefile"), &(lines.join("\n") + "\n"));
    }

    /// One random edit; returns what it did.
    fn mutate(&mut self) -> &'static str {
        const KINDS: &[&str] = &[
            "post-body", "post-title", "post-date", "add-post", "remove-post", "post-template", "index-template", "public-edit", "public-add",
            "public-remove", "site-json", "rows", "arg", "sitefile-line", "sitefile-swap", "bad-post", "target", "post-draft", "drafts-arg", "schema",
            "post-tags", "nothing",
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
            "target" => self.target = if self.target.is_none() { Some("assets".into()) } else { None },
            "post-draft" => {
                let n = pick(&mut self.r, &names);
                let p = self.posts.get_mut(&n).unwrap();
                p.draft = !p.draft;
            }
            "drafts-arg" => self.drafts = !self.drafts,
            "schema" => self.bad_schema = !self.bad_schema,
            "post-tags" => {
                let n = pick(&mut self.r, &names);
                let count = self.r.below(3);
                let tags = (0..count).map(|_| self.word()).collect();
                self.posts.get_mut(&n).unwrap().tags = tags;
            }
            _ => {}
        }
        self.write();
        kind
    }

    fn options(&self) -> BuildOptions {
        let mut args = BuildArgs::new();
        args.set("SITE", self.arg.clone());
        args.set("DRAFTS", self.drafts.to_string());
        BuildOptions { context_dir: self.dir.to_string_lossy().into_owned(), lockfile: LockfileSetting::Disabled, build_args: args, target: self.target.clone(), ..Default::default() }
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

/// Build, or with `out` build and export there (`--force`); the export says whether it was incremental.
fn outcome_to(mut options: BuildOptions, out: Option<&Path>) -> (Outcome, Option<(BuildResult, Option<Applied>)>) {
    let warnings = Arc::new(Mutex::new(Vec::new()));
    let w = warnings.clone();
    let cb: Callback = Arc::new(move |m: &str| w.lock().unwrap().push(m.to_string()));
    options.on_warning = Some(cb);
    let built = match out {
        None => build(&options).map(|r| (r, None)),
        Some(out) => build_to_directory(&options, &out.to_string_lossy(), &ExportTarget { force: true, ..Default::default() }).map(|e| (e.result, e.incremental)),
    };
    match built {
        Ok((r, applied)) => {
            let o = Outcome { files: Some(listing(&r.output)), tree: Some(listing(&r.tree)), warnings: warnings.lock().unwrap().clone(), error: None };
            (o, Some((r, applied)))
        }
        Err(e) => (Outcome { files: None, tree: None, warnings: warnings.lock().unwrap().clone(), error: Some(format!("{}: {}", e.code.as_str(), e.message())) }, None),
    }
}

/// Everything under `dir`, for byte-for-byte comparison: type, path, permission bits, contents.
fn list_dir(dir: &Path) -> Option<Vec<String>> {
    fn walk(dir: &Path, rel: &str, out: &mut Vec<String>) {
        let mut names: Vec<String> = fs::read_dir(dir.join(rel.trim_start_matches('/'))).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        for name in names {
            let p = format!("{rel}/{name}");
            let full = dir.join(p.trim_start_matches('/'));
            let m = fs::symlink_metadata(&full).unwrap();
            let mode = m.permissions().mode() & 0o7777;
            if m.file_type().is_symlink() {
                out.push(format!("l {p} -> {}", fs::read_link(&full).unwrap().display()));
            } else if m.is_dir() {
                out.push(format!("d {p} {mode:o}"));
                walk(dir, &p, out);
            } else {
                out.push(format!("f {p} {mode:o} {}", sha256(&fs::read(&full).unwrap())));
            }
        }
    }
    if !dir.exists() {
        return None;
    }
    let mut out = Vec::new();
    walk(dir, "", &mut out);
    Some(out)
}

const TAMPERS: &[&str] = &["rewrite-same-size", "append", "add-file", "add-dir", "delete-file", "delete-dir", "chmod", "file-to-symlink", "dir-to-symlink"];

/// Modify an exported directory behind the exporter's back (`victim`: a directory outside it).
fn tamper(out: &Path, victim: &Path, kind: &str, pick: &mut dyn FnMut(usize) -> usize) {
    let all = list_dir(out).unwrap_or_default();
    let files: Vec<String> = all.iter().filter(|l| l.starts_with("f ")).map(|l| l.split(' ').nth(1).unwrap().to_string()).collect();
    let dirs: Vec<String> = all.iter().filter(|l| l.starts_with("d ")).map(|l| l.split(' ').nth(1).unwrap().to_string()).collect();
    let at = |p: &str| out.join(p.trim_start_matches('/'));
    if files.is_empty() {
        return;
    }
    let f = at(&files[pick(files.len())]);
    match kind {
        "rewrite-same-size" => {
            // Same size, mtime put back: only the ctime tells.
            let m = fs::metadata(&f).unwrap();
            let mut bytes = fs::read(&f).unwrap();
            if let Some(b) = bytes.first_mut() {
                *b = if *b == b'X' { b'Y' } else { b'X' };
            }
            fs::write(&f, &bytes).unwrap();
            let mtime = std::time::UNIX_EPOCH + std::time::Duration::new(m.mtime() as u64, m.mtime_nsec() as u32);
            fs::File::options().write(true).open(&f).unwrap().set_modified(mtime).unwrap();
        }
        "append" => {
            use std::io::Write;
            fs::OpenOptions::new().append(true).open(&f).unwrap().write_all(b"tampered").unwrap();
        }
        "add-file" => {
            let d = if dirs.is_empty() { out.to_path_buf() } else { at(&dirs[pick(dirs.len())]) };
            fs::write(d.join("stray.txt"), "stray").unwrap();
        }
        "add-dir" => fs::create_dir_all(out.join("stray-dir")).unwrap(),
        "delete-file" => fs::remove_file(&f).unwrap(),
        "delete-dir" => {
            if !dirs.is_empty() {
                fs::remove_dir_all(at(&dirs[pick(dirs.len())])).unwrap();
            }
        }
        "chmod" => fs::set_permissions(&f, fs::Permissions::from_mode(0o600)).unwrap(),
        "file-to-symlink" => {
            fs::remove_file(&f).unwrap();
            std::os::unix::fs::symlink(victim.join("secret.txt"), &f).unwrap();
        }
        "dir-to-symlink" => {
            if !dirs.is_empty() {
                let d = at(&dirs[pick(dirs.len())]);
                fs::remove_dir_all(&d).unwrap();
                std::os::unix::fs::symlink(victim, &d).unwrap();
            }
        }
        _ => unreachable!(),
    }
}

fn random_edits(seed: u32) {
    let work = Tmp::new();
    let mut site = Site::new(work.0.join("ctx"), seed);
    // The exports (watch mode writes only the changes; the clean one rewrites everything),
    // and a directory outside them that tampering points symlinks at.
    let inc_out = work.0.join("out-incremental");
    let clean_out = work.0.join("out-clean");
    let victim = work.0.join("victim");
    put(&victim.join("secret.txt"), "secret");
    let victim_before = list_dir(&victim);
    let mut incremental_exports = 0;
    let mut tampered = 0;
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
            // Sometimes the export is modified between rebuilds.
            if site.r.next() < 0.2 {
                let kind = TAMPERS[site.r.below(TAMPERS.len())];
                tamper(&inc_out, &victim, kind, &mut |n| site.r.below(n));
                history.push(format!("tamper:{kind}"));
                tampered += 1;
            }
        }
        let mut inc = site.options();
        inc.memo = Some(memo.clone());
        inc.cache_dir = cache.clone();
        let mut clean = site.options();
        clean.cache_dir = cache.clone();
        let before = list_dir(&inc_out);
        let (a, r) = outcome_to(inc, Some(&inc_out));
        let (b, _) = outcome_to(clean, Some(&clean_out));
        let at = format!("seed {seed}, step {step}: {}", history.join(" > "));
        assert_eq!(a, b, "{at}");
        if let Some((r, applied)) = r {
            reused += r.memo_stats.unwrap().hits;
            // The export is byte for byte (and mode for mode) the clean one.
            assert_eq!(list_dir(&inc_out), list_dir(&clean_out), "{at}");
            if applied.is_some() {
                incremental_exports += 1;
            }
        } else {
            // A failed build never touches the output.
            assert_eq!(list_dir(&inc_out), before, "{at}");
        }
        assert_eq!(list_dir(&victim), victim_before, "{at}");
    }
    // The memo was used, and exports wrote only the changes (the others: the first export,
    // large changes such as switching --target, and the exports after tampering).
    assert!(reused > 0);
    assert!(incremental_exports >= 8, "{incremental_exports} incremental exports");
    assert!(tampered > 0);
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
            "9 COPY hit",
            "11 COPY hit",
            "12 COPY hit",
            "13 COPY hit",
            "14 RENDER miss",
            "15 RENDER miss",
            "16 RENDER files",
            "19 TRANSFORM miss",
            "20 FEED miss",
            "21 SITEMAP miss",
            "22 RENDER miss",
            "25 RENDER miss"
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

// --- incremental export (watch mode) -------------------------------------------------------

fn files(n: usize, extra: &[(&str, &str, u32)]) -> Tree {
    let mut list = Vec::new();
    for i in 0..n {
        let p = format!("/d{}/f{i}.txt", i % 3);
        if !extra.iter().any(|(q, ..)| *q == p) {
            list.push((p, make_file(format!("file {i}").into_bytes(), "test", 0o644)));
        }
    }
    for (p, text, mode) in extra {
        list.push((p.to_string(), make_file(text.as_bytes().to_vec(), "test", *mode)));
    }
    Tree::from_files(list).unwrap()
}

/// An incremental export (its state kept across calls), compared with a clean export after each.
struct Pair {
    work: Tmp,
    ctx: PathBuf,
    out: PathBuf,
    state: SharedExportState,
}

impl Pair {
    fn new() -> Pair {
        let work = Tmp::new();
        let ctx = work.0.join("ctx");
        fs::create_dir_all(&ctx).unwrap();
        let out = ctx.join("dist");
        Pair { work, ctx, out, state: Arc::new(Mutex::new(ExportState::default())) }
    }
    fn run_with(&self, t: &Tree, before_apply: Option<Arc<dyn Fn() + Send + Sync>>) -> Option<Applied> {
        let mut o = ExportOptions::new(&self.ctx).incremental(Some(self.state.clone()));
        o.before_apply = before_apply;
        let r = export_directory(t, &self.out, &o).unwrap();
        let clean = self.ctx.join("clean");
        export_directory(t, &clean, &ExportOptions::new(&self.ctx)).unwrap();
        assert_eq!(list_dir(&self.out), list_dir(&clean));
        r.incremental
    }
    fn run(&self, t: &Tree) -> Option<Applied> {
        self.run_with(t, None)
    }
}

fn ino(p: &Path) -> u64 {
    fs::symlink_metadata(p).unwrap().ino()
}

const fn applied(written: usize, removed: usize) -> Option<Applied> {
    Some(Applied { written, removed })
}

#[test]
fn incremental_export_writes_only_what_changed() {
    let p = Pair::new();
    assert_eq!(p.run(&files(20, &[])), None); // the first export is a full one
    let one = ino(&p.out.join("d1/f1.txt"));
    let other = ino(&p.out.join("d0/f0.txt"));
    assert_eq!(p.run(&files(20, &[])), applied(0, 0));
    assert_eq!(ino(&p.out.join("d1/f1.txt")), one);
    assert_eq!(p.run(&files(20, &[("/d1/f1.txt", "edited", 0o644)])), applied(1, 0));
    assert_ne!(ino(&p.out.join("d1/f1.txt")), one);
    assert_eq!(ino(&p.out.join("d0/f0.txt")), other);
    // A mode change alone is a change.
    assert_eq!(p.run(&files(20, &[("/d1/f1.txt", "edited", 0o755)])), applied(1, 0));
    assert_eq!(fs::metadata(p.out.join("d1/f1.txt")).unwrap().permissions().mode() & 0o777, 0o755);
}

#[test]
fn incremental_export_removes_files_and_emptied_directories() {
    let p = Pair::new();
    p.run(&files(20, &[("/x/y/z.txt", "z", 0o644), ("/p", "file", 0o644)]));
    assert_eq!(p.run(&files(20, &[("/p/q.txt", "now a directory", 0o644)])), applied(1, 2));
    assert!(!p.out.join("x").exists());
    assert_eq!(p.run(&files(20, &[("/p", "a file again", 0o644)])), applied(1, 1));
}

#[test]
fn incremental_export_large_or_case_changes_are_full() {
    let p = Pair::new();
    p.run(&files(20, &[("/About.html", "a", 0o644)]));
    assert_eq!(p.run(&files(5, &[])), None);
    p.run(&files(20, &[("/About.html", "a", 0o644)]));
    // /about.html may be /About.html on this file system: it is never renamed over in place.
    p.run(&files(20, &[("/about.html", "b", 0o644)]));
}

#[test]
fn incremental_export_detects_and_undoes_tampering() {
    for kind in TAMPERS {
        let p = Pair::new();
        let victim = p.work.0.join("victim");
        put(&victim.join("secret.txt"), "secret");
        p.run(&files(20, &[]));
        let mut n = 0;
        tamper(&p.out, &victim, kind, &mut |len| {
            n += 1;
            (n - 1) % len
        });
        assert_eq!(p.run(&files(20, &[("/d0/f0.txt", "edit", 0o644)])), None, "{kind}");
        assert_eq!(list_dir(&victim).unwrap(), [format!("f /secret.txt 644 {}", sha256(b"secret"))], "{kind}");
        assert_eq!(p.run(&files(20, &[("/d0/f0.txt", "edit 2", 0o644)])), applied(1, 0), "{kind}");
    }
}

#[test]
fn incremental_export_catches_changes_made_while_staging() {
    let p = Pair::new();
    p.run(&files(20, &[]));
    let f = p.out.join("d0/f0.txt");
    assert_eq!(p.run_with(&files(20, &[("/d0/f0.txt", "edit", 0o644)]), Some(Arc::new(move || fs::write(&f, "someone else").unwrap()))), None);
    let d = p.out.join("new");
    assert_eq!(p.run_with(&files(20, &[("/d0/f0.txt", "edit", 0o644), ("/new/a.txt", "a", 0o644)]), Some(Arc::new(move || fs::create_dir(&d).unwrap()))), None);
    let left: Vec<String> = fs::read_dir(&p.ctx).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).filter(|n| n.contains("sitefile-")).collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn incremental_export_other_or_replaced_destination_is_full() {
    let p = Pair::new();
    let state: SharedExportState = Arc::new(Mutex::new(ExportState::default()));
    let exp = |to: &str, s: Option<&SharedExportState>| export_directory(&files(20, &[]), &p.ctx.join(to), &ExportOptions::new(&p.ctx).incremental(s.cloned())).unwrap().incremental;
    exp("a", Some(&state));
    assert_eq!(exp("b", Some(&state)), None);
    assert!(exp("b", Some(&state)).is_some());
    fs::remove_dir_all(p.ctx.join("b")).unwrap();
    exp("b", None);
    assert_eq!(exp("b", Some(&state)), None);
    assert_eq!(exp("b", None), None);
}
