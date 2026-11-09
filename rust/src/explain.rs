//! `sitefile graph` and `sitefile why` (issue #94), port of `src/explain.ts`: the dependency graph of a
//! plan, and the history of one output file. Both outputs are part of the CLI contract and identical in
//! the two implementations.

use crate::error::{ErrorCode::E_NOT_FOUND, Result, SitefileError};
use crate::executor::{BuildResult, ImportedSitefile};
use crate::language::ast::imported_stage;
use crate::paths::{join_virtual, normalize_virtual};
use crate::planner::{imported_stage_refs, Operation, Plan, SourceRef, Stage, StageBase};
use crate::resources::types::Resource;
use crate::tree::{FileNode, Tree};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;

// ---------------------------------------------------------------- labels

/// A context-relative path as written in a Sitefile: `./src`, `./` for the context root.
fn local_label(p: &str) -> String {
    format!("./{p}")
}

/// A source as an operation label shows it.
fn source_label(r: &SourceRef) -> String {
    match r {
        SourceRef::Inline { delimiter, .. } => format!("<<{delimiter}"),
        SourceRef::Stage { name, path, .. } => format!("--from={name} {path}"),
        SourceRef::Local { path, .. } => local_label(path),
        SourceRef::Import { name, path, .. } => {
            if path == "/" {
                format!("{name}:")
            } else {
                format!("{name}:{path}")
            }
        }
    }
}

/// One line describing an operation: its keyword, source and destination (FOR destinations are not evaluated).
pub fn operation_label(op: &Operation) -> String {
    match op {
        Operation::Copy {
            source,
            dest,
            dest_is_dir,
            ..
        } => {
            format!(
                "COPY {} {dest}{}",
                source_label(source),
                if *dest_is_dir && dest != "/" { "/" } else { "" }
            )
        }
        Operation::Render {
            template,
            dest,
            each,
            ..
        } => match each {
            Some(e) => format!(
                "RENDER {} FOR {} IN {}{}{}",
                source_label(template),
                e.as_name,
                e.list,
                e.by.as_ref()
                    .map(|b| format!(" BY {b}"))
                    .unwrap_or_default(),
                e.page
                    .as_ref()
                    .map(|p| format!(" PAGE {}", p.size))
                    .unwrap_or_default()
            ),
            None => format!("RENDER {} TO {dest}", source_label(template)),
        },
        Operation::Transform {
            source,
            plugin,
            each,
            ..
        } => format!(
            "TRANSFORM {plugin}{}{}",
            source
                .as_ref()
                .map(|s| format!(" {}", source_label(s)))
                .unwrap_or_default(),
            each.as_ref()
                .map(|e| format!(" FOR {} IN {}", e.as_name, e.list))
                .unwrap_or_default()
        ),
        Operation::Sitemap { dest, .. } => format!("SITEMAP {dest}"),
        Operation::Feed { list, dest, .. } => format!("FEED {list} {dest}"),
        Operation::Fingerprint { patterns, .. } => format!("FINGERPRINT {}", patterns.join(" ")),
        Operation::Canonical { base, pretty, .. } => {
            format!("CANONICAL {base}{}", if *pretty { " PRETTY" } else { "" })
        }
        Operation::CheckLinks { warn, external, .. } => {
            format!(
                "CHECKLINKS{}{}",
                if *warn { " --warn" } else { "" },
                if *external { " --external" } else { "" }
            )
        }
    }
}

fn stage_label(stage: &Stage) -> String {
    let base = match &stage.base {
        StageBase::Scratch => "scratch".to_string(),
        StageBase::Local { path } => local_label(path),
        StageBase::Stage { name } => name.clone(),
        StageBase::Image {
            reference,
            at: Some(at),
            ..
        } => format!("image {reference} AT {at}"),
        StageBase::Image { reference, .. } => format!("image {reference}"),
    };
    match &stage.name {
        Some(n) => format!("FROM {base} AS {n}"),
        None => format!("FROM {base}"),
    }
}

/// `build`, or `#2` for the second stage when it has no name.
fn stage_display(stage: &Stage, index: usize) -> String {
    stage
        .name
        .clone()
        .unwrap_or_else(|| format!("#{}", index + 1))
}

fn stage_id(stage: &Stage, index: usize) -> String {
    format!("stage:{}", stage_display(stage, index))
}

// ---------------------------------------------------------------- graph

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Import,
    Image,
    Plugin,
    Tool,
    Local,
    Operation,
    Stage,
    Site,
    Export,
}

impl NodeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeKind::Import => "import",
            NodeKind::Image => "image",
            NodeKind::Plugin => "plugin",
            NodeKind::Tool => "tool",
            NodeKind::Local => "local",
            NodeKind::Operation => "operation",
            NodeKind::Stage => "stage",
            NodeKind::Site => "site",
            NodeKind::Export => "export",
        }
    }
    fn dot_shape(self) -> &'static str {
        match self {
            NodeKind::Import => "cylinder",
            NodeKind::Image => "tab",
            NodeKind::Plugin | NodeKind::Tool => "component",
            NodeKind::Local => "folder",
            NodeKind::Operation => "box",
            NodeKind::Stage => "box3d",
            NodeKind::Site => "note",
            NodeKind::Export => "doubleoctagon",
        }
    }
}

#[derive(Debug, Clone)]
pub struct GraphNode {
    pub id: String,
    pub kind: NodeKind,
    pub label: String,
    pub line: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct GraphEdge {
    pub from: String,
    pub to: String,
    /// `base`, `input`, `next` or `export`.
    pub kind: &'static str,
}

#[derive(Debug, Clone)]
pub struct Graph {
    pub file: String,
    pub target: Option<String>,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

#[derive(Default)]
struct Edges {
    list: Vec<GraphEdge>,
    seen: HashSet<(String, String, &'static str)>,
}

impl Edges {
    fn add(&mut self, from: &str, to: &str, kind: &'static str) {
        if self.seen.insert((from.to_string(), to.to_string(), kind)) {
            self.list.push(GraphEdge {
                from: from.to_string(),
                to: to.to_string(),
                kind,
            });
        }
    }
}

/// The plan as a graph (see `planGraph` in src/explain.ts for the node and edge kinds).
pub fn plan_graph(p: &Plan) -> Graph {
    let mut locals: Vec<GraphNode> = Vec::new();
    let mut edges = Edges::default();
    let local = |rel: &str, locals: &mut Vec<GraphNode>| -> String {
        let id = format!("local:{}", local_label(rel));
        if !locals.iter().any(|n| n.id == id) {
            locals.push(GraphNode {
                id: id.clone(),
                kind: NodeKind::Local,
                label: local_label(rel),
                line: None,
            });
        }
        id
    };
    let imports: HashSet<&str> = p.imports.iter().map(|i| i.name.as_str()).collect();
    let tools: HashSet<&str> = p.tools.iter().map(|t| t.name.as_str()).collect();
    let mut stage_ids: HashMap<String, String> = HashMap::new();
    let list =
        |name: &str| -> Option<String> { imports.contains(name).then(|| format!("import:{name}")) };
    // An IMPORT … SCHEMA reads its schema file.
    for i in &p.imports {
        if let Some(s) = &i.schema {
            let from = local(&s.path, &mut locals);
            edges.add(&from, &format!("import:{}", i.name), "input");
        }
    }

    // Stages of imported Sitefiles (IMPORT … AS sitefile) are read like stages: one node each, fed by the import.
    let mut imported_stages: Vec<GraphNode> = Vec::new();
    for r in imported_stage_refs(&p.stages) {
        if stage_ids.contains_key(&r.name) {
            continue;
        }
        let id = format!("stage:{}", r.name);
        let line = p
            .imports
            .iter()
            .find(|i| i.name == r.import)
            .map(|i| i.span.line);
        imported_stages.push(GraphNode {
            id: id.clone(),
            kind: NodeKind::Stage,
            label: format!("stage {} of IMPORT {}", r.stage, r.import),
            line,
        });
        edges.add(&format!("import:{}", r.import), &id, "input");
        stage_ids.insert(r.name.clone(), id);
    }
    let mut staged: Vec<GraphNode> = Vec::new();
    let mut images: Vec<GraphNode> = Vec::new();
    for (si, stage) in p.stages.iter().enumerate() {
        let id = stage_id(stage, si);
        let ids: Vec<String> = stage
            .operations
            .iter()
            .map(|op| format!("op:{}", op.span().line))
            .collect();
        let first = ids.first().cloned().unwrap_or_else(|| id.clone());
        match &stage.base {
            StageBase::Local { path } => {
                let from = local(path, &mut locals);
                edges.add(&from, &first, "base");
            }
            StageBase::Stage { name } => {
                if let Some(from) = stage_ids.get(name) {
                    edges.add(from, &first, "base");
                }
            }
            StageBase::Image { reference, .. } => {
                let image_id = format!("image:{reference}");
                if !images.iter().any(|n| n.id == image_id) {
                    images.push(GraphNode {
                        id: image_id.clone(),
                        kind: NodeKind::Image,
                        label: format!("image {reference}"),
                        line: Some(stage.span.line),
                    });
                }
                edges.add(&image_id, &first, "base");
            }
            StageBase::Scratch => {}
        }
        for (i, op) in stage.operations.iter().enumerate() {
            let op_id = &ids[i];
            staged.push(GraphNode {
                id: op_id.clone(),
                kind: NodeKind::Operation,
                label: operation_label(op),
                line: Some(op.span().line),
            });
            let source_node = |r: &SourceRef, locals: &mut Vec<GraphNode>| -> Option<String> {
                match r {
                    SourceRef::Inline { .. } => None,
                    SourceRef::Stage { name, .. } => stage_ids.get(name).cloned(),
                    SourceRef::Local { path, .. } => Some(local(path, locals)),
                    SourceRef::Import { name, .. } => Some(format!("import:{name}")),
                }
            };
            let mut inputs: Vec<Option<String>> = Vec::new();
            match op {
                Operation::Copy { source, .. } => inputs.push(source_node(source, &mut locals)),
                Operation::Render { template, each, .. } => {
                    inputs.push(source_node(template, &mut locals));
                    inputs.push(each.as_ref().and_then(|e| list(&e.list)));
                }
                Operation::Transform {
                    source,
                    plugin,
                    each,
                    ..
                } => {
                    inputs.push(source.as_ref().and_then(|s| source_node(s, &mut locals)));
                    inputs.push(Some(format!(
                        "{}:{plugin}",
                        if tools.contains(plugin.as_str()) {
                            "tool"
                        } else {
                            "plugin"
                        }
                    )));
                    inputs.push(each.as_ref().and_then(|e| list(&e.list)));
                }
                Operation::Feed { list: l, .. } => inputs.push(list(l)),
                Operation::Sitemap { .. }
                | Operation::Fingerprint { .. }
                | Operation::Canonical { .. }
                | Operation::CheckLinks { .. } => {}
            }
            for from in inputs.into_iter().flatten() {
                edges.add(&from, op_id, "input");
            }
            edges.add(op_id, ids.get(i + 1).unwrap_or(&id), "next");
        }
        staged.push(GraphNode {
            id: id.clone(),
            kind: NodeKind::Stage,
            label: stage_label(stage),
            line: Some(stage.span.line),
        });
        if let Some(n) = &stage.name {
            stage_ids.insert(n.clone(), id);
        }
    }

    let last = p.stages.len() - 1;
    edges.add(&stage_id(&p.stages[last], last), "export", "export");
    let mut site: Vec<GraphNode> = Vec::new();
    let rules: [(&str, Vec<u32>, &str); 2] = [
        (
            "_redirects",
            p.redirects.iter().map(|r| r.span.line).collect(),
            "REDIRECT",
        ),
        (
            "_headers",
            p.headers.iter().map(|h| h.span.line).collect(),
            "HEADER",
        ),
    ];
    for (name, lines, kind) in rules {
        let Some(first) = lines.first() else { continue };
        let id = format!("site:{name}");
        let n = lines.len();
        site.push(GraphNode {
            id: id.clone(),
            kind: NodeKind::Site,
            label: format!(
                "{} ({n} {kind} line{})",
                join_virtual(&p.export_path, name),
                if n == 1 { "" } else { "s" }
            ),
            line: Some(*first),
        });
        edges.add(&id, "export", "export");
    }

    let mut nodes: Vec<GraphNode> = Vec::new();
    for i in &p.imports {
        let as_kind = i
            .as_kind
            .map(|k| format!(" AS {}", k.as_str()))
            .unwrap_or_default();
        nodes.push(GraphNode {
            id: format!("import:{}", i.name),
            kind: NodeKind::Import,
            label: format!("IMPORT {} {}{as_kind}", i.name, i.uri),
            line: Some(i.span.line),
        });
    }
    for pl in &p.plugins {
        nodes.push(GraphNode {
            id: format!("plugin:{}", pl.name),
            kind: NodeKind::Plugin,
            label: format!("PLUGIN {} {}", pl.name, local_label(&pl.path)),
            line: Some(pl.span.line),
        });
    }
    for t in &p.tools {
        nodes.push(GraphNode {
            id: format!("tool:{}", t.name),
            kind: NodeKind::Tool,
            label: format!("TOOL {}", t.name),
            line: Some(t.span.line),
        });
    }
    nodes.extend(images);
    nodes.extend(imported_stages);
    nodes.extend(locals);
    nodes.extend(staged);
    nodes.extend(site);
    let target = p
        .target
        .as_ref()
        .map(|t| format!(" (--target {t})"))
        .unwrap_or_default();
    nodes.push(GraphNode {
        id: "export".into(),
        kind: NodeKind::Export,
        label: format!(
            "EXPORT {} AS {}{target}",
            p.export_path,
            p.export_format.as_str()
        ),
        line: None,
    });
    Graph {
        file: p.file.clone(),
        target: p.target.clone(),
        nodes,
        edges: edges.list,
    }
}

/// The object `graph --json` prints: `{file, target?, nodes: [{id, kind, label, line?}], edges: [{from, to, kind}]}`.
pub fn graph_json(g: &Graph) -> Value {
    let mut m = Map::new();
    m.insert("file".into(), json!(g.file));
    if let Some(t) = &g.target {
        m.insert("target".into(), json!(t));
    }
    let nodes: Vec<Value> = g
        .nodes
        .iter()
        .map(|n| {
            let mut o = Map::new();
            o.insert("id".into(), json!(n.id));
            o.insert("kind".into(), json!(n.kind.as_str()));
            o.insert("label".into(), json!(n.label));
            if let Some(l) = n.line {
                o.insert("line".into(), json!(l));
            }
            Value::Object(o)
        })
        .collect();
    m.insert("nodes".into(), Value::Array(nodes));
    m.insert(
        "edges".into(),
        Value::Array(
            g.edges
                .iter()
                .map(|e| json!({ "from": e.from, "to": e.to, "kind": e.kind }))
                .collect(),
        ),
    );
    Value::Object(m)
}

/// `id  label` per node, then one `  <- from (kind)` line per incoming edge, then a count.
pub fn graph_text(g: &Graph) -> String {
    let mut out = String::new();
    for n in &g.nodes {
        out.push_str(&format!("{}  {}\n", n.id, n.label));
        for e in g.edges.iter().filter(|e| e.to == n.id) {
            out.push_str(&format!("  <- {} ({})\n", e.from, e.kind));
        }
    }
    out.push_str(&format!(
        "{} nodes, {} edges\n",
        g.nodes.len(),
        g.edges.len()
    ));
    out
}

fn dot_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Graphviz DOT (`sitefile graph --dot | dot -Tsvg > graph.svg`).
pub fn graph_dot(g: &Graph) -> String {
    let mut out = String::from("digraph sitefile {\n  rankdir=LR;\n  node [shape=box];\n");
    for n in &g.nodes {
        out.push_str(&format!(
            "  {} [label={}, shape={}];\n",
            dot_string(&n.id),
            dot_string(&n.label),
            n.kind.dot_shape()
        ));
    }
    for e in &g.edges {
        let style = match e.kind {
            "input" => " [style=dashed]",
            "next" => "",
            _ => " [style=bold]",
        };
        out.push_str(&format!(
            "  {} -> {}{style};\n",
            dot_string(&e.from),
            dot_string(&e.to)
        ));
    }
    out.push_str("}\n");
    out
}

// ---------------------------------------------------------------- why

#[derive(Debug, Clone)]
pub struct WhyEvent {
    /// `Sitefile:12`: the instruction's line.
    pub location: String,
    /// The instruction, as `graph` labels it.
    pub instruction: String,
    /// Stage name, or `#n` for an unnamed stage.
    pub stage: String,
    /// The path in that stage's tree.
    pub path: String,
    /// `write` or `overwrite`.
    pub action: &'static str,
    pub digest: String,
    pub inputs: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct WhyReport {
    pub path: String,
    pub mode: String,
    pub size: usize,
    pub digest: String,
    pub origin: String,
    /// Oldest first; the last event wrote the file as exported.
    pub history: Vec<WhyEvent>,
}

fn same(a: Option<&FileNode>, b: Option<&FileNode>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.origin == b.origin && a.digest == b.digest && a.mode == b.mode,
        _ => false,
    }
}

/// `rel` of `full` below directory `dir` (`/x` for `/d/x` under `/d`).
fn below<'a>(dir: &str, full: &'a str) -> &'a str {
    if dir == "/" {
        full
    } else {
        &full[dir.len()..]
    }
}

/// `base` + `rel` where `base` is a virtual path (`/` for the root) and `rel` starts with `/`.
fn under(base: &str, rel: &str) -> String {
    if base == "/" {
        rel.to_string()
    } else {
        format!("{base}{rel}")
    }
}

fn read_label(r: &SourceRef) -> String {
    match r {
        SourceRef::Inline { delimiter, .. } => format!("<<{delimiter}"),
        SourceRef::Local { path, .. } => local_label(path),
        SourceRef::Import { name, path, .. } => {
            if path == "/" {
                format!("import {name}")
            } else {
                format!("import {name}:{path}")
            }
        }
        SourceRef::Stage { name, path, .. } => format!("stage {name}:{path}"),
    }
}

/// One Sitefile's view for `why`: the build's own, or one imported AS sitefile (stages shown as `theme+<stage>`).
#[derive(Clone)]
struct WhyScope<'a> {
    plan: &'a Plan,
    steps: &'a [Vec<Tree>],
    /// `theme+` for a Sitefile imported as `theme` (empty for the build's own).
    prefix: String,
    resources: &'a [(String, Resource)],
    /// The build's own context directory, or the imported Sitefile.
    context: WhyContext<'a>,
    imported: &'a [(String, ImportedSitefile)],
    /// What each FINGERPRINT of this Sitefile renamed (old path, new path), by its line.
    renames: &'a [(u32, Vec<(String, String)>)],
}

#[derive(Clone, Copy)]
enum WhyContext<'a> {
    Host(&'a str),
    Unit(&'a ImportedSitefile),
}

impl<'a> WhyScope<'a> {
    fn at(&self, line: u32) -> String {
        format!("{}:{line}", self.plan.file)
    }
    fn final_of(&self, i: usize) -> &'a Tree {
        self.steps[i].last().expect("a stage has a starting tree")
    }
    /// Whether a context-relative path is a directory (a COPY of a directory lists the file inside it).
    fn is_dir(&self, rel: &str) -> bool {
        match self.context {
            WhyContext::Host(dir) => std::fs::metadata(Path::new(dir).join(rel))
                .map(|m| m.is_dir())
                .unwrap_or(false),
            WhyContext::Unit(u) => u.is_directory(rel),
        }
    }
    /// The scope and index of the stage `name` (`build`, or `theme+assets` for a stage of an imported Sitefile).
    fn locate(&self, name: &str) -> (WhyScope<'a>, usize) {
        if let Some((import, stage)) = imported_stage(name) {
            if let Some((_, unit)) = self.imported.iter().find(|(n, _)| n == import) {
                let scope = WhyScope {
                    plan: &unit.plan,
                    steps: unit.steps.as_deref().unwrap_or(&[]),
                    prefix: format!("{}{import}+", self.prefix),
                    resources: &unit.resources,
                    context: WhyContext::Unit(unit),
                    imported: &unit.imported,
                    renames: &unit.renames,
                };
                let i = unit
                    .plan
                    .stages
                    .iter()
                    .position(|s| s.name.as_deref() == Some(stage))
                    .expect("an imported stage that was built");
                return (scope, i);
            }
        }
        let i = self
            .plan
            .stages
            .iter()
            .position(|s| s.name.as_deref() == Some(name))
            .expect("a stage that was built");
        (self.clone(), i)
    }

    /// Display and (for a stage) the file a COPY source contributes to `file` written at `dest`.
    fn copied(
        &self,
        r: &SourceRef,
        dest: &str,
        file: &str,
    ) -> (String, Option<(WhyScope<'a>, usize, String)>) {
        let rel = if file == dest {
            None
        } else {
            Some(below(dest, file))
        };
        match r {
            SourceRef::Inline { delimiter, .. } => (format!("<<{delimiter}"), None),
            SourceRef::Local { path, .. } => {
                let is_dir = self.is_dir(path);
                let rest = match rel {
                    Some(rel) if is_dir => {
                        if path.is_empty() {
                            rel[1..].to_string()
                        } else {
                            format!("{path}{rel}")
                        }
                    }
                    _ => path.clone(),
                };
                (local_label(&rest), None)
            }
            SourceRef::Import { name, path, .. } => {
                let res = self
                    .resources
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, r)| r);
                let Some(Resource::Tree { tree, .. }) = res else {
                    return (
                        if path == "/" {
                            format!("import {name}")
                        } else {
                            format!("import {name}:{path}")
                        },
                        None,
                    );
                };
                let inner = match rel {
                    Some(rel) if !tree.is_file(path) => under(path, rel),
                    _ => path.clone(),
                };
                (format!("import {name}:{inner}"), None)
            }
            SourceRef::Stage { name, path, .. } => {
                let (scope, i) = self.locate(name);
                let tree = scope.final_of(i);
                let inner = match rel {
                    Some(rel) if !tree.is_file(path) => under(path, rel),
                    _ => path.clone(),
                };
                let from = tree.is_file(&inner).then(|| (scope, i, inner.clone()));
                (format!("stage {name}:{inner}"), from)
            }
        }
    }

    /// The path `file` had before operation `up_to` of stage `si` when a FINGERPRINT there renamed it, and where.
    fn renamed_at(&self, si: usize, file: &str, up_to: usize) -> Option<(usize, String)> {
        let ops = &self.plan.stages[si].operations;
        (0..up_to).rev().find_map(|i| {
            let Operation::Fingerprint { span, .. } = &ops[i] else {
                return None;
            };
            let (_, list) = self.renames.iter().find(|(line, _)| *line == span.line)?;
            list.iter()
                .find(|(_, to)| to == file)
                .map(|(from, _)| (i, from.clone()))
        })
    }

    fn history(&self, si: usize, file: &str) -> Vec<WhyEvent> {
        self.history_to(si, file, self.plan.stages[si].operations.len())
    }

    fn history_to(&self, si: usize, file: &str, up_to: usize) -> Vec<WhyEvent> {
        let p = self.plan;
        let stage = &p.stages[si];
        let trees = &self.steps[si];
        let display = format!("{}{}", self.prefix, stage_display(stage, si));
        // A renamed file: the history of its old path up to the FINGERPRINT, then the rename.
        if let Some((index, from)) = self.renamed_at(si, file, up_to) {
            let op = &stage.operations[index];
            let node = trees[index + 1]
                .get(file)
                .expect("a renamed file is in the tree after its FINGERPRINT");
            let mut events = self.history_to(si, &from, index);
            events.push(WhyEvent {
                location: self.at(op.span().line),
                instruction: operation_label(op),
                stage: display.clone(),
                path: file.to_string(),
                action: "write",
                digest: node.digest.clone(),
                inputs: vec![format!("{from} (renamed)")],
            });
            events.extend(self.later(si, file, index + 1, up_to, Some(&**node)));
            return events;
        }
        let mut events = Vec::new();
        let prev = trees[0].get(file).map(|n| &**n);
        if let Some(start) = prev {
            match &stage.base {
                StageBase::Stage { name } => {
                    let (scope, i) = self.locate(name);
                    events.extend(scope.history(i, file));
                }
                StageBase::Local { path } => {
                    let rest = if path.is_empty() {
                        file[1..].to_string()
                    } else {
                        format!("{path}{file}")
                    };
                    events.push(WhyEvent {
                        location: self.at(stage.span.line),
                        instruction: stage_label(stage),
                        stage: display.clone(),
                        path: file.to_string(),
                        action: "write",
                        digest: start.digest.clone(),
                        inputs: vec![local_label(&rest)],
                    });
                }
                // The file's origin names the image, its digest and the path inside the layer.
                StageBase::Image { .. } => events.push(WhyEvent {
                    location: self.at(stage.span.line),
                    instruction: stage_label(stage),
                    stage: display.clone(),
                    path: file.to_string(),
                    action: "write",
                    digest: start.digest.clone(),
                    inputs: vec![start.origin.clone()],
                }),
                StageBase::Scratch => {}
            }
        }
        events.extend(self.later(si, file, 0, up_to, prev));
        events
    }

    /// The events of operations `from` to `up_to` (exclusive) of stage `si` that changed `file` (`prev`: before them).
    fn later(
        &self,
        si: usize,
        file: &str,
        from: usize,
        up_to: usize,
        mut prev: Option<&'a FileNode>,
    ) -> Vec<WhyEvent> {
        let tools: HashSet<&str> = self.plan.tools.iter().map(|t| t.name.as_str()).collect();
        let stage = &self.plan.stages[si];
        let trees = &self.steps[si];
        let display = format!("{}{}", self.prefix, stage_display(stage, si));
        let mut events = Vec::new();
        for (i, op) in stage.operations.iter().enumerate().take(up_to).skip(from) {
            let next = trees[i + 1].get(file).map(|n| &**n);
            let Some(written) = next.filter(|_| !same(prev, next)) else {
                prev = next;
                continue;
            };
            let mut inputs = Vec::new();
            match op {
                Operation::Copy { source, dest, .. } => {
                    let (input, from) = self.copied(source, dest, file);
                    if let Some((scope, s, path)) = from {
                        events.extend(scope.history(s, &path));
                    }
                    inputs.push(input);
                }
                Operation::Render { template, each, .. } => {
                    inputs.push(read_label(template));
                    if let Some(e) = each {
                        inputs.push(format!("list {}", e.list));
                    }
                }
                Operation::Transform {
                    source,
                    plugin,
                    each,
                    span,
                    ..
                } => {
                    // A directory TRANSFORM names the input file in each output's origin.
                    let per_file = format!("{} TRANSFORM ", self.at(span.line));
                    if let Some(s) = source {
                        inputs.push(match written.origin.strip_prefix(&per_file) {
                            Some(file) => file.to_string(),
                            None => read_label(s),
                        });
                    }
                    inputs.push(format!(
                        "{} {plugin}",
                        if tools.contains(plugin.as_str()) {
                            "tool"
                        } else {
                            "plugin"
                        }
                    ));
                    if let Some(e) = each {
                        inputs.push(format!("list {}", e.list));
                    }
                }
                Operation::Sitemap { .. } => inputs.push("the .html files in the tree".into()),
                Operation::Canonical { .. } => inputs.push("the page (canonical link)".into()),
                // Writes nothing, so never reached.
                Operation::CheckLinks { .. } => {}
                Operation::Fingerprint { manifest, .. } => inputs.push(
                    if manifest.as_deref() == Some(file) {
                        "the files it renamed"
                    } else {
                        "references to renamed files"
                    }
                    .into(),
                ),
                Operation::Feed { list, .. } => inputs.push(format!("list {list}")),
            }
            events.push(WhyEvent {
                location: self.at(op.span().line),
                instruction: operation_label(op),
                stage: display.clone(),
                path: file.to_string(),
                action: if prev.is_some() { "overwrite" } else { "write" },
                digest: written.digest.clone(),
                inputs,
            });
            prev = next;
        }
        events
    }
}

/// Explain one exported file (see `why` in src/explain.ts). `result` must come from a build with `record_steps`.
pub fn why(result: &BuildResult, raw: &str, context_dir: &str) -> Result<WhyReport> {
    let p = &result.plan;
    let steps = result
        .steps
        .as_deref()
        .expect("why() needs a build with record_steps");
    let wanted = if raw.starts_with('/') {
        raw.to_string()
    } else {
        format!("/{raw}")
    };
    let want = normalize_virtual(&wanted, None, "path")?.path;
    let Some(node) = result.output.get(&want) else {
        if result.output.is_directory(&want) {
            return Err(SitefileError::plain(
                E_NOT_FOUND,
                format!("'{want}' is a directory in the build output; name a file"),
            ));
        }
        return Err(SitefileError::plain(
            E_NOT_FOUND,
            format!("'{want}' is not in the build output"),
        ));
    };
    let w = WhyScope {
        plan: p,
        steps,
        prefix: String::new(),
        resources: &result.resources,
        context: WhyContext::Host(context_dir),
        imported: &result.imported,
        renames: &result.renames,
    };
    let last = p.stages.len() - 1;
    let full = under(&p.export_path, &want);
    let mut history = w.history(last, &full);
    // The EXPORT site files are added after the last stage.
    let before = w.final_of(last).get(&full).map(|n| &**n);
    let after = result.tree.get(&full).map(|n| &**n);
    if let Some(a) = after.filter(|_| !same(before, after)) {
        let (lines, kind): (Vec<u32>, &str) = if full == join_virtual(&p.export_path, "_redirects")
        {
            (
                p.redirects.iter().map(|r| r.span.line).collect(),
                "REDIRECT",
            )
        } else {
            (p.headers.iter().map(|h| h.span.line).collect(), "HEADER")
        };
        let mut inputs: Vec<String> = lines
            .iter()
            .map(|l| format!("{} {kind}", w.at(*l)))
            .collect();
        // `/_headers` may also hold FINGERPRINT's Cache-Control lines (or only them): with cache_headers set,
        // no HEADER sets Cache-Control, so the name in the file means they were added.
        let fp = p.cache_headers.as_ref().filter(|_| {
            kind == "HEADER" && String::from_utf8_lossy(&a.bytes).contains("  Cache-Control: ")
        });
        if let Some(fp) = fp {
            inputs.push(format!("{} FINGERPRINT", w.at(fp.line)));
        }
        let (location, instruction) = match (lines.first(), fp) {
            (Some(l), _) => (*l, kind),
            (None, Some(fp)) => (fp.line, "FINGERPRINT"),
            (None, None) => (0, kind),
        };
        history.push(WhyEvent {
            location: w.at(location),
            instruction: instruction.to_string(),
            stage: stage_display(&p.stages[last], last),
            path: full.clone(),
            action: if before.is_some() {
                "overwrite"
            } else {
                "write"
            },
            digest: a.digest.clone(),
            inputs,
        });
    }
    Ok(WhyReport {
        path: want,
        mode: format!("{:o}", node.mode),
        size: node.bytes.len(),
        digest: node.digest.clone(),
        origin: node.origin.clone(),
        history,
    })
}

/// The object `why --json` prints.
pub fn why_json(r: &WhyReport) -> Value {
    json!({
        "path": r.path,
        "mode": r.mode,
        "size": r.size,
        "digest": r.digest,
        "origin": r.origin,
        "history": r.history.iter().map(|e| json!({
            "location": e.location,
            "instruction": e.instruction,
            "stage": e.stage,
            "path": e.path,
            "action": e.action,
            "digest": e.digest,
            "inputs": e.inputs,
        })).collect::<Vec<_>>(),
    })
}

pub fn why_text(r: &WhyReport) -> String {
    let mut out = format!("{}  {}  {} bytes  {}\n", r.path, r.mode, r.size, r.digest);
    for (i, e) in r.history.iter().enumerate() {
        out.push_str(&format!(
            "  {}. {} {}  [stage {}]  {} {}\n",
            i + 1,
            e.location,
            e.instruction,
            e.stage,
            if e.action == "write" {
                "wrote"
            } else {
                "overwrote"
            },
            e.path
        ));
        for input in &e.inputs {
            out.push_str(&format!("       from {input}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::validate::parse_program;
    use crate::planner::{plan, target_plan};

    const SITEFILE: &str = "STAGE build\nIMPORT theme FROM ./theme AS tree\nCOPY ./src/ /out/\nCOPY --overwrite ./override.html /out/index.html\nSTAGE more FROM build\nCOPY <<X /out/extra.txt\nextra\nX\nSTAGE site\nCOPY --from=more /out/ /\nCOPY --overwrite theme: /\nREDIRECT /old /new\nEXPORT /\n";

    fn graph_of(src: &str) -> (Plan, Graph) {
        let p = plan(
            &parse_program(src, "Sitefile").unwrap(),
            &Default::default(),
        )
        .unwrap();
        let g = plan_graph(&p);
        (p, g)
    }

    #[test]
    fn graph_nodes_and_edges() {
        let (p, g) = graph_of(SITEFILE);
        let ids: Vec<&str> = g.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "import:theme",
                "local:./src",
                "local:./override.html",
                "op:3",
                "op:4",
                "stage:build",
                "op:6",
                "stage:more",
                "op:10",
                "op:11",
                "stage:site",
                "site:_redirects",
                "export"
            ]
        );
        let op4 = g.nodes.iter().find(|n| n.id == "op:4").unwrap();
        assert_eq!(
            (op4.kind, op4.label.as_str(), op4.line),
            (
                NodeKind::Operation,
                "COPY ./override.html /out/index.html",
                Some(4)
            )
        );
        let edges: Vec<String> = g
            .edges
            .iter()
            .map(|e| format!("{} -{}-> {}", e.from, e.kind, e.to))
            .collect();
        for e in [
            "stage:build -base-> op:6",
            "stage:more -input-> op:10",
            "import:theme -input-> op:11",
            "op:11 -next-> stage:site",
            "stage:site -export-> export",
            "site:_redirects -export-> export",
        ] {
            assert!(edges.contains(&e.to_string()), "{e}: {edges:?}");
        }
        let text = graph_text(&g);
        assert!(
            text.starts_with("import:theme  IMPORT theme ./theme AS tree\n"),
            "{text}"
        );
        assert!(text.ends_with("\n13 nodes, 12 edges\n"), "{text}");
        let dot = graph_dot(&g);
        assert!(dot.starts_with("digraph sitefile {\n  rankdir=LR;\n"));
        assert!(dot.contains("\n  \"stage:more\" -> \"op:10\" [style=dashed];\n"));
        assert!(dot.contains("\n  \"op:3\" -> \"op:4\";\n"));
        let odd = Graph {
            file: "x".into(),
            target: None,
            nodes: vec![GraphNode {
                id: "a\"b\\".into(),
                kind: NodeKind::Local,
                label: "q\"".into(),
                line: None,
            }],
            edges: vec![],
        };
        assert_eq!(
            graph_dot(&odd).split('\n').nth(3),
            Some("  \"a\\\"b\\\\\" [label=\"q\\\"\", shape=folder];")
        );
        let json = serde_json::to_string(&graph_json(&g)).unwrap();
        assert!(json.starts_with(r#"{"file":"Sitefile","nodes":[{"id":"import:theme","kind":"import","label":"IMPORT theme ./theme AS tree","line":2},"#), "{json}");

        let t = plan_graph(&target_plan(p, "build").unwrap());
        assert_eq!(t.target.as_deref(), Some("build"));
        assert_eq!(
            t.nodes.last().unwrap().label,
            "EXPORT / AS directory (--target build)"
        );
        assert!(!t.nodes.iter().any(|n| n.kind == NodeKind::Site));
    }

    #[test]
    fn operation_labels() {
        let (_, g) = graph_of("PLUGIN p FROM ./p/p.wasm\nTOOL t\nIMPORT posts FROM ./posts AS markdown\nTRANSFORM ./a.svg WITH p TO /a.svg\nTRANSFORM WITH t TO /b/${x.slug}.txt FOR x IN posts ARGS {output}\nRENDER ./t.html TO /p/${post.slug}.html FOR post IN posts\nFEED posts BASE https://x/ LINK /p/${item.slug}.html\nSITEMAP BASE https://x\nEXPORT /");
        let labels: Vec<&str> = g
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Operation)
            .map(|n| n.label.as_str())
            .collect();
        assert_eq!(
            labels,
            [
                "TRANSFORM p ./a.svg",
                "TRANSFORM t FOR x IN posts",
                "RENDER ./t.html FOR post IN posts",
                "FEED posts /feed.xml",
                "SITEMAP /sitemap.xml"
            ]
        );
        let edges: Vec<String> = g
            .edges
            .iter()
            .map(|e| format!("{} -{}-> {}", e.from, e.kind, e.to))
            .collect();
        for e in [
            "plugin:p -input-> op:4",
            "tool:t -input-> op:5",
            "import:posts -input-> op:5",
            "import:posts -input-> op:7",
        ] {
            assert!(edges.contains(&e.to_string()), "{e}: {edges:?}");
        }
    }
}
