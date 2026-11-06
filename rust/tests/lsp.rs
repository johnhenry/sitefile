//! `sitefile lsp` over real stdio: spawn the binary and speak LSP to it.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{ChildStdin, ChildStdout, Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_sitefile");

fn send(w: &mut ChildStdin, v: Value) {
    let body = v.to_string();
    write!(w, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    w.flush().unwrap();
}

fn recv(r: &mut BufReader<ChildStdout>) -> Value {
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        assert!(r.read_line(&mut line).unwrap() > 0, "server closed stdout");
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            len = v.trim().parse().unwrap();
        }
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

/// The next message that is not a notification of another method.
fn recv_where(r: &mut BufReader<ChildStdout>, pred: impl Fn(&Value) -> bool) -> Value {
    loop {
        let m = recv(r);
        if pred(&m) {
            return m;
        }
    }
}

#[test]
fn stdio_session() {
    let mut child = Command::new(BIN)
        .args(["lsp", "--stdio"])
        .env("SITEFILE_HOME", std::env::temp_dir().join("sitefile-lsp-test-home-unused"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let uri = "file:///tmp/project/Sitefile";

    send(&mut stdin, json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "processId": null, "rootUri": null, "capabilities": {} } }));
    let init = recv(&mut stdout);
    assert_eq!(init["id"], 1);
    let caps = &init["result"]["capabilities"];
    assert_eq!(caps["positionEncoding"], "utf-16");
    assert_eq!(caps["hoverProvider"], true);
    send(&mut stdin, json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }));

    // A syntax error on line 2 (0-based 1), with a non-ASCII path before it.
    let text = "FROM scratch\nCOPY ./é😀 /b extra\nIMPORT posts FROM ./posts AS markdown\nEXPORT /\n";
    send(&mut stdin, json!({ "jsonrpc": "2.0", "method": "textDocument/didOpen", "params": { "textDocument": { "uri": uri, "languageId": "sitefile", "version": 1, "text": text } } }));
    let diags = recv_where(&mut stdout, |m| m["method"] == "textDocument/publishDiagnostics");
    assert_eq!(diags["params"]["uri"], uri);
    let d = &diags["params"]["diagnostics"][0];
    assert_eq!(d["code"], "E_SYNTAX");
    assert_eq!(d["severity"], 1);
    assert_eq!(d["range"]["start"], json!({ "line": 1, "character": 14 }));
    assert_eq!(d["range"]["end"], json!({ "line": 1, "character": 19 }));

    // Completion after `AS ` on the IMPORT line.
    send(&mut stdin, json!({ "jsonrpc": "2.0", "id": 2, "method": "textDocument/completion", "params": { "textDocument": { "uri": uri }, "position": { "line": 2, "character": 29 } } }));
    let c = recv_where(&mut stdout, |m| m["id"] == 2);
    let labels: Vec<&str> = c["result"]["items"].as_array().unwrap().iter().map(|i| i["label"].as_str().unwrap()).collect();
    assert!(labels.contains(&"markdown") && labels.contains(&"tree"), "{labels:?}");

    // Fix the error: the next publish is clean.
    send(&mut stdin, json!({ "jsonrpc": "2.0", "method": "textDocument/didChange", "params": { "textDocument": { "uri": uri, "version": 2 }, "contentChanges": [{ "text": "FROM scratch\nCOPY ./a /b\nEXPORT /\n" }] } }));
    let diags = recv_where(&mut stdout, |m| m["method"] == "textDocument/publishDiagnostics");
    assert_eq!(diags["params"]["version"], 2);
    assert_eq!(diags["params"]["diagnostics"], json!([]));

    // A lint warning carries its rule id, and a code action fixes it.
    assert_eq!(caps["codeActionProvider"]["codeActionKinds"], json!(["quickfix"]));
    send(&mut stdin, json!({ "jsonrpc": "2.0", "method": "textDocument/didChange", "params": { "textDocument": { "uri": uri, "version": 3 }, "contentChanges": [{ "text": "RENDER <<H TO /a.html\nx\nH\nEXPORT /\n" }] } }));
    let diags = recv_where(&mut stdout, |m| m["method"] == "textDocument/publishDiagnostics");
    let d = &diags["params"]["diagnostics"][0];
    assert_eq!((d["code"].as_str(), d["severity"].as_u64()), (Some("render-escape"), Some(2)));
    let range = json!({ "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } });
    send(&mut stdin, json!({ "jsonrpc": "2.0", "id": 4, "method": "textDocument/codeAction", "params": { "textDocument": { "uri": uri }, "range": range, "context": { "diagnostics": [d] } } }));
    let a = recv_where(&mut stdout, |m| m["id"] == 4);
    assert_eq!(a["result"][0]["title"], "Add --escape=html");
    assert_eq!(a["result"][0]["edit"]["changes"][uri][0]["newText"], " --escape=html");

    send(&mut stdin, json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }));
    let s = recv_where(&mut stdout, |m| m["id"] == 3);
    assert_eq!(s["result"], Value::Null);
    send(&mut stdin, json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);
    assert_eq!(child.wait().unwrap().code(), Some(0));
}

#[test]
fn lsp_help_and_bad_option() {
    let out = Command::new(BIN).args(["lsp", "--help"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("Usage: sitefile lsp [--stdio]"));
    let out = Command::new(BIN).args(["lsp", "--tcp"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("lsp: unknown option '--tcp'"));
    let out = Command::new(BIN).arg("--help").output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("sitefile lsp [--stdio]"));
}

#[test]
fn examples_have_no_error_diagnostics() {
    let examples = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples");
    let mut n = 0;
    for e in std::fs::read_dir(&examples).unwrap() {
        let p = e.unwrap().path().join("Sitefile");
        let Ok(text) = std::fs::read_to_string(&p) else { continue };
        let a = sitefile::lsp::diagnostics::analyze(&text, "Sitefile");
        let errors: Vec<_> = a.diagnostics.iter().filter(|d| d.severity == 1).collect();
        assert!(errors.is_empty(), "{}: {errors:?}", p.display());
        n += 1;
    }
    assert!(n > 5);
}
