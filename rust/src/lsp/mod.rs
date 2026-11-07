//! `sitefile lsp`: a Language Server Protocol 3.17 server over stdio
//! (JSON-RPC 2.0 with `Content-Length` framing). See docs/LSP.md.
//!
//! Single-threaded: one loop reads a message, handles it, and writes any
//! reply. Documents use full text sync. Diagnostics run the real pipeline
//! (scan → parse → validate → plan, no IO) and are published when the input
//! queue is drained, so a burst of edits is analyzed once.
//!
//! This is a Rust-only tooling exception (docs/RUST.md): the language is the
//! same in both implementations; the server is an editor tool.

pub mod actions;
pub mod diagnostics;
pub mod docs;
pub mod features;
pub mod framing;
pub mod index;
pub mod text;

use crate::resources::lockfile::Lockfile;
use diagnostics::{analyze, Analysis};
use index::Index;
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::io::{self, BufReader, Read, Write};
use text::Lines;

// JSON-RPC / LSP error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;
const SERVER_NOT_INITIALIZED: i64 = -32002;
const REQUEST_FAILED: i64 = -32803;

struct Document {
    text: String,
    version: Option<i64>,
    /// Label used in spans and warnings (the file name).
    file: String,
    lines: Lines,
    index: Index,
    /// Out of date after an edit until the next publish (or a hover needs it).
    analysis: Option<Analysis>,
}

impl Document {
    fn new(uri: &str, text: String, version: Option<i64>) -> Document {
        let lines = Lines::new(&text);
        let index = index::build(&lines);
        Document {
            text,
            version,
            file: file_label(uri),
            lines,
            index,
            analysis: None,
        }
    }
    fn set_text(&mut self, text: String) {
        self.lines = Lines::new(&text);
        self.index = index::build(&self.lines);
        self.text = text;
        self.analysis = None;
    }
    fn analysis(&mut self) -> &Analysis {
        if self.analysis.is_none() {
            self.analysis = Some(analyze(&self.text, &self.file));
        }
        self.analysis.as_ref().unwrap_or_else(|| unreachable!())
    }
}

/// The last path segment of a `file://` URI, percent-decoded (`Sitefile` otherwise).
/// The local path of a `file://` URI (percent-decoded), if it is one.
pub(crate) fn uri_path(uri: &str) -> Option<std::path::PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    Some(std::path::PathBuf::from(percent_decode(rest)))
}

fn percent_decode(seg: &str) -> String {
    let bytes = seg.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(b) = hex {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn file_label(uri: &str) -> String {
    let seg = uri.rsplit('/').next().unwrap_or("");
    let bytes = seg.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(b) = hex {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    let s = String::from_utf8_lossy(&out).into_owned();
    if s.is_empty() {
        "Sitefile".into()
    } else {
        s
    }
}

#[derive(Default)]
pub struct Server {
    docs: HashMap<String, Document>,
    /// Documents whose diagnostics must be (re)published.
    dirty: BTreeSet<String>,
    initialized: bool,
    shutdown: bool,
}

fn response(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

fn capabilities() -> Value {
    json!({
        "positionEncoding": "utf-16",
        "textDocumentSync": { "openClose": true, "change": 1, "save": { "includeText": true } },
        "completionProvider": { "triggerCharacters": ["$", "{", "-", "=", " ", ":"] },
        "hoverProvider": true,
        "definitionProvider": true,
        "referencesProvider": true,
        "documentSymbolProvider": true,
        "renameProvider": { "prepareProvider": true },
        "codeActionProvider": { "codeActionKinds": ["quickfix"] },
    })
}

/// `textDocument.uri` and `position` of a request.
fn doc_position(params: &Value) -> Option<(String, u32, u32)> {
    let uri = params["textDocument"]["uri"].as_str()?.to_string();
    let line = params["position"]["line"].as_u64()? as u32;
    let character = params["position"]["character"].as_u64()? as u32;
    Some((uri, line, character))
}

impl Server {
    pub fn new() -> Server {
        Server::default()
    }

    /// Publish diagnostics for edited documents.
    pub fn flush(&mut self, out: &mut Vec<Value>) {
        for uri in std::mem::take(&mut self.dirty) {
            let Some(doc) = self.docs.get_mut(&uri) else {
                continue;
            };
            let version = doc.version;
            let diags: Vec<Value> = doc
                .analysis()
                .diagnostics
                .iter()
                .map(|d| d.to_json())
                .collect();
            let mut params = json!({ "uri": uri, "diagnostics": diags });
            if let Some(v) = version {
                params["version"] = json!(v);
            }
            out.push(notification("textDocument/publishDiagnostics", params));
        }
    }

    /// Handle one message. Replies are pushed to `out`; `Some(code)` means exit.
    pub fn handle(&mut self, msg: &Value, out: &mut Vec<Value>) -> Option<i32> {
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            if msg.get("id").is_none() {
                out.push(error(
                    &Value::Null,
                    INVALID_REQUEST,
                    "not a JSON-RPC request or notification",
                ));
            }
            return None; // a response to a server request (we send none): ignore
        };
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        match msg.get("id") {
            Some(id) => {
                let reply = self.request(method, id, &params);
                out.push(reply);
                None
            }
            None => self.notify(method, &params, out),
        }
    }

    fn request(&mut self, method: &str, id: &Value, params: &Value) -> Value {
        if method == "initialize" {
            self.initialized = true;
            return response(
                id,
                json!({
                    "capabilities": capabilities(),
                    "serverInfo": { "name": "sitefile", "version": crate::cli::VERSION },
                }),
            );
        }
        if !self.initialized {
            return error(id, SERVER_NOT_INITIALIZED, "the server is not initialized");
        }
        if self.shutdown {
            return error(id, INVALID_REQUEST, "the server is shutting down");
        }
        if method == "shutdown" {
            self.shutdown = true;
            return response(id, Value::Null);
        }
        let known = matches!(
            method,
            "textDocument/completion"
                | "textDocument/hover"
                | "textDocument/definition"
                | "textDocument/references"
                | "textDocument/documentSymbol"
                | "textDocument/prepareRename"
                | "textDocument/rename"
                | "textDocument/codeAction"
        );
        if !known {
            return error(id, METHOD_NOT_FOUND, &format!("unhandled method {method}"));
        }
        let uri = params["textDocument"]["uri"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let Some(doc) = self.docs.get_mut(&uri) else {
            // Not open: nothing to say about it.
            return response(
                id,
                if method == "textDocument/completion"
                    || method == "textDocument/references"
                    || method == "textDocument/documentSymbol"
                {
                    json!([])
                } else {
                    Value::Null
                },
            );
        };
        if method == "textDocument/documentSymbol" {
            return response(id, features::document_symbols(&doc.lines, &doc.index));
        }
        if method == "textDocument/codeAction" {
            let (Some(first), Some(last)) = (
                params["range"]["start"]["line"].as_u64(),
                params["range"]["end"]["line"].as_u64(),
            ) else {
                return error(id, INVALID_PARAMS, "expected textDocument and range");
            };
            // The only file the server reads: the Sitefile.lock next to the document, for pins.
            let lock = uri_path(&uri)
                .and_then(|p| p.parent().map(|d| d.join("Sitefile.lock")))
                .and_then(|p| Lockfile::load(p).ok());
            doc.analysis();
            let Some(analysis) = doc.analysis.as_ref() else {
                return response(id, json!([]));
            };
            return response(
                id,
                actions::code_actions(
                    &doc.lines,
                    &doc.index,
                    analysis,
                    &uri,
                    first as u32,
                    last as u32,
                    lock.as_ref(),
                ),
            );
        }
        let Some((_, line, col)) = doc_position(params) else {
            return error(id, INVALID_PARAMS, "expected textDocument and position");
        };
        let result = match method {
            "textDocument/completion" => {
                let c = features::completion(&doc.lines, &doc.index, line, col);
                features::completion_json(&c, line, col)
            }
            "textDocument/hover" => {
                doc.analysis();
                features::comment_hover(&doc.lines, &doc.index, line, col)
                    .or_else(|| features::hover(&doc.index, doc.analysis.as_ref(), line, col))
                    .unwrap_or(Value::Null)
            }
            "textDocument/definition" => {
                features::definition(&doc.index, &uri, line, col).unwrap_or(Value::Null)
            }
            "textDocument/references" => {
                let decl = params["context"]["includeDeclaration"]
                    .as_bool()
                    .unwrap_or(true);
                features::references(&doc.index, &uri, line, col, decl)
            }
            "textDocument/prepareRename" => {
                features::prepare_rename(&doc.index, line, col).unwrap_or(Value::Null)
            }
            _ => {
                let new_name = params["newName"].as_str().unwrap_or("");
                match features::rename(&doc.index, &uri, line, col, new_name) {
                    Ok(v) => v.unwrap_or(Value::Null),
                    Err(m) => return error(id, REQUEST_FAILED, &m),
                }
            }
        };
        response(id, result)
    }

    fn notify(&mut self, method: &str, params: &Value, out: &mut Vec<Value>) -> Option<i32> {
        if method == "exit" {
            return Some(if self.shutdown { 0 } else { 1 });
        }
        if !self.initialized || self.shutdown {
            return None;
        }
        let uri = params["textDocument"]["uri"]
            .as_str()
            .unwrap_or("")
            .to_string();
        match method {
            "textDocument/didOpen" => {
                let text = params["textDocument"]["text"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                let version = params["textDocument"]["version"].as_i64();
                self.docs
                    .insert(uri.clone(), Document::new(&uri, text, version));
                self.dirty.insert(uri);
            }
            "textDocument/didChange" => {
                let doc = self.docs.get_mut(&uri)?;
                let mut text = std::mem::take(&mut doc.text);
                for change in params["contentChanges"].as_array().into_iter().flatten() {
                    let new_text = change["text"].as_str().unwrap_or("");
                    let r = &change["range"];
                    let pos = |p: &Value| {
                        (
                            p["line"].as_u64().unwrap_or(0) as u32,
                            p["character"].as_u64().unwrap_or(0) as u32,
                        )
                    };
                    if r.is_object() {
                        text::apply_edit(&mut text, pos(&r["start"]), pos(&r["end"]), new_text);
                    } else {
                        text = new_text.to_string();
                    }
                }
                doc.version = params["textDocument"]["version"].as_i64().or(doc.version);
                doc.set_text(text);
                self.dirty.insert(uri);
            }
            "textDocument/didSave" => {
                if let Some(doc) = self.docs.get_mut(&uri) {
                    if let Some(t) = params["text"].as_str() {
                        if t != doc.text {
                            doc.set_text(t.to_string());
                        }
                    }
                    self.dirty.insert(uri);
                }
            }
            "textDocument/didClose" => {
                self.docs.remove(&uri);
                self.dirty.remove(&uri);
                out.push(notification(
                    "textDocument/publishDiagnostics",
                    json!({ "uri": uri, "diagnostics": [] }),
                ));
            }
            // initialized, $/cancelRequest (requests finish synchronously), $/setTrace,
            // workspace/didChangeConfiguration and anything else: nothing to do.
            _ => {}
        }
        None
    }
}

fn write_all<W: Write>(w: &mut W, msgs: &mut Vec<Value>) -> io::Result<()> {
    for m in msgs.drain(..) {
        framing::write_message(w, m.to_string().as_bytes())?;
    }
    Ok(())
}

/// Serve until `exit` or end of input. Returns the process exit code: 0 after
/// `shutdown` + `exit`, 1 when input ends or `exit` comes without `shutdown`.
pub fn serve<R: Read, W: Write>(input: R, mut output: W) -> i32 {
    let mut reader = BufReader::new(input);
    let mut server = Server::new();
    let mut out: Vec<Value> = Vec::new();
    loop {
        // Nothing more queued: publish now (edits that arrived together are analyzed once).
        if reader.buffer().is_empty() {
            server.flush(&mut out);
            if write_all(&mut output, &mut out).is_err() {
                return 1;
            }
        }
        let body = match framing::read_message(&mut reader) {
            Ok(Some(b)) => b,
            Ok(None) => return 1,
            Err(e) => {
                eprintln!("sitefile lsp: {e}");
                return 1;
            }
        };
        let code = match serde_json::from_slice::<Value>(&body) {
            Ok(msg) if msg.is_object() => {
                // A bug in one handler should not take the editor's server down.
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    server.handle(&msg, &mut out)
                }));
                r.unwrap_or_else(|_| {
                    if let Some(id) = msg.get("id") {
                        out.push(error(
                            id,
                            INTERNAL_ERROR,
                            "internal error (see the server's stderr)",
                        ));
                    }
                    None
                })
            }
            Ok(_) | Err(_) => {
                out.push(error(&Value::Null, PARSE_ERROR, "invalid JSON-RPC message"));
                None
            }
        };
        if write_all(&mut output, &mut out).is_err() {
            return 1;
        }
        if let Some(code) = code {
            return code;
        }
    }
}

/// `sitefile lsp [--stdio]`.
pub fn run(args: &[String]) -> i32 {
    for a in args {
        match a.as_str() {
            // Editors pass the transport; stdio is the only one.
            "--stdio" => {}
            "-h" | "--help" => {
                print!(
                    "Usage: sitefile lsp [--stdio]\n\nRun the Sitefile language server (LSP 3.17, JSON-RPC over stdin/stdout).\nEditors start it; see docs/LSP.md.\n"
                );
                return 0;
            }
            other => {
                return crate::cli::common::usage_error(&format!(
                    "lsp: unknown option '{other}' (only --stdio is supported)"
                ))
            }
        }
    }
    serve(io::stdin().lock(), io::stdout().lock())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(v: Value) -> Vec<u8> {
        let mut b = Vec::new();
        framing::write_message(&mut b, v.to_string().as_bytes()).unwrap();
        b
    }

    fn read_all(mut out: &[u8]) -> Vec<Value> {
        let mut v = Vec::new();
        while let Some(m) = framing::read_message(&mut out).unwrap() {
            v.push(serde_json::from_slice(&m).unwrap());
        }
        v
    }

    fn session(msgs: Vec<Value>) -> (i32, Vec<Value>) {
        let input: Vec<u8> = msgs.into_iter().flat_map(frame).collect();
        let mut output = Vec::new();
        let code = serve(&input[..], &mut output);
        (code, read_all(&output))
    }

    fn init() -> Value {
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "capabilities": {} } })
    }

    #[test]
    fn lifecycle_and_unknown_methods() {
        let (code, out) = session(vec![
            json!({ "jsonrpc": "2.0", "id": 0, "method": "textDocument/hover", "params": {} }),
            init(),
            json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
            json!({ "jsonrpc": "2.0", "method": "$/cancelRequest", "params": { "id": 1 } }),
            json!({ "jsonrpc": "2.0", "method": "custom/notification" }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "custom/request" }),
            json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
            json!({ "jsonrpc": "2.0", "id": 4, "method": "textDocument/hover" }),
            json!({ "jsonrpc": "2.0", "method": "exit" }),
        ]);
        assert_eq!(code, 0);
        assert_eq!(out[0]["error"]["code"], SERVER_NOT_INITIALIZED);
        assert_eq!(
            out[1]["result"]["capabilities"]["positionEncoding"],
            "utf-16"
        );
        assert_eq!(out[1]["result"]["serverInfo"]["name"], "sitefile");
        assert_eq!(out[2]["id"], 2);
        assert_eq!(out[2]["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(
            (out[3]["id"].clone(), out[3]["result"].clone()),
            (json!(3), Value::Null)
        );
        assert_eq!(out[4]["error"]["code"], INVALID_REQUEST);
        assert_eq!(out.len(), 5);
    }

    #[test]
    fn exit_without_shutdown_and_eof() {
        assert_eq!(
            session(vec![init(), json!({ "jsonrpc": "2.0", "method": "exit" })]).0,
            1
        );
        assert_eq!(session(vec![init()]).0, 1);
    }

    #[test]
    fn bad_json_is_a_parse_error() {
        let mut input = frame(init());
        framing::write_message(&mut input, b"{not json").unwrap();
        let mut output = Vec::new();
        serve(&input[..], &mut output);
        let out = read_all(&output);
        assert_eq!(out[1]["error"]["code"], PARSE_ERROR);
    }

    const URI: &str = "file:///w/Sitefile";

    fn open(text: &str) -> Value {
        json!({ "jsonrpc": "2.0", "method": "textDocument/didOpen", "params": { "textDocument": { "uri": URI, "languageId": "sitefile", "version": 1, "text": text } } })
    }

    fn edits() -> Vec<Value> {
        vec![
            json!({ "jsonrpc": "2.0", "method": "textDocument/didChange", "params": { "textDocument": { "uri": URI, "version": 2 }, "contentChanges": [{ "range": { "start": { "line": 1, "character": 8 }, "end": { "line": 1, "character": 8 } }, "text": " /a" }] } }),
            json!({ "jsonrpc": "2.0", "method": "textDocument/didChange", "params": { "textDocument": { "uri": URI, "version": 3 }, "contentChanges": [{ "range": { "start": { "line": 2, "character": 0 }, "end": { "line": 2, "character": 0 } }, "text": "FROM scratch\n" }] } }),
        ]
    }

    #[test]
    fn diagnostics_follow_edits_and_close() {
        let mut s = Server::new();
        let mut out = Vec::new();
        s.handle(&init(), &mut out);
        s.handle(&open("FROM scratch\nCOPY ./a\nEXPORT /"), &mut out);
        out.clear();
        s.flush(&mut out);
        assert_eq!(out[0]["params"]["diagnostics"][0]["code"], "E_SYNTAX");
        assert_eq!(out[0]["params"]["version"], 1);
        // Incremental edits: the syntax error is fixed, then an unnamed stage is added.
        out.clear();
        for e in edits() {
            s.handle(&e, &mut out);
        }
        s.flush(&mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["params"]["version"], 3);
        let d = &out[0]["params"]["diagnostics"];
        assert_eq!(d.as_array().unwrap().len(), 1, "{d}");
        assert_eq!(
            (
                d[0]["severity"].clone(),
                d[0]["range"]["start"]["line"].clone()
            ),
            (json!(2), json!(0))
        );
        out.clear();
        s.flush(&mut out);
        assert!(out.is_empty()); // nothing changed
        s.handle(&json!({ "jsonrpc": "2.0", "method": "textDocument/didClose", "params": { "textDocument": { "uri": URI } } }), &mut out);
        assert_eq!(out[0]["params"]["diagnostics"], json!([]));
    }

    #[test]
    fn queued_edits_are_analyzed_once() {
        // Everything arrives in one read: one publish (for the close), none for each edit.
        let mut msgs = vec![init(), open("FROM scratch\nCOPY ./a\nEXPORT /")];
        msgs.extend(edits());
        msgs.push(json!({ "jsonrpc": "2.0", "id": 9, "method": "textDocument/documentSymbol", "params": { "textDocument": { "uri": URI } } }));
        let (_, out) = session(msgs);
        let pubs = out
            .iter()
            .filter(|m| m["method"] == "textDocument/publishDiagnostics")
            .count();
        assert_eq!(pubs, 1);
        let symbols = out.iter().find(|m| m["id"] == 9).unwrap();
        assert_eq!(symbols["result"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn file_labels() {
        assert_eq!(file_label("file:///a/b/Sitefile"), "Sitefile");
        assert_eq!(
            file_label("file:///a/My%20Site.sitefile"),
            "My Site.sitefile"
        );
        assert_eq!(file_label("untitled:"), "untitled:");
        assert_eq!(file_label("file:///a/"), "Sitefile");
    }
}
