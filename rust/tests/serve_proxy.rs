//! serve's proxy rules and `--proxy` routes (issue #172), through the
//! library's request handler against a small upstream (the same checks as
//! test/serve-proxy.test.ts).

use sitefile::resources::network::NetworkSettings;
use sitefile::resources::types::DEFAULT_FETCH_POLICY;
use sitefile::server::{handle_request, ServeProxy, SiteServerOptions};
use sitefile::tree::{make_file, Tree};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

/// An upstream that records each request line and its headers and answers 201 with the path
/// (302 for paths containing "moved").
fn upstream() -> (String, Arc<Mutex<Vec<(String, Vec<String>)>>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", l.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<(String, Vec<String>)>>> = Arc::default();
    let s = seen.clone();
    std::thread::spawn(move || {
        for stream in l.incoming() {
            let mut stream = stream.unwrap();
            let mut r = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            let mut headers = Vec::new();
            loop {
                let mut h = String::new();
                r.read_line(&mut h).unwrap();
                if h.trim().is_empty() {
                    break;
                }
                headers.push(h.trim().to_ascii_lowercase());
            }
            let path = line.split(' ').nth(1).unwrap_or("").to_string();
            s.lock().unwrap().push((line.trim().to_string(), headers));
            let reply = if path.contains("moved") {
                "HTTP/1.1 302 Found\r\nlocation: /elsewhere\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_string()
            } else {
                let body = format!("{{\"url\":\"{path}\"}}");
                format!("HTTP/1.1 201 Created\r\ncontent-type: application/json\r\nset-cookie: a=1\r\nset-cookie: b=2\r\nx-up: yes\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}", body.len())
            };
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    (origin, seen)
}

fn proxy(routes: Vec<(String, String)>, allow: &[&str]) -> ServeProxy {
    let mut policy = DEFAULT_FETCH_POLICY;
    policy.allow_http = true;
    policy.allow_private_network = true;
    policy.network = NetworkSettings {
        strict: true,
        allow_hosts: allow.iter().map(|s| s.to_string()).collect(),
        ..NetworkSettings::NONE
    };
    ServeProxy { routes, policy }
}

fn tree(files: &[(&str, &str)]) -> Tree {
    Tree::from_files(
        files
            .iter()
            .map(|(p, c)| (p.to_string(), make_file(c.as_bytes().to_vec(), "t", 0o644))),
    )
    .unwrap()
}

fn req(headers: &[(&str, &str)]) -> HashMap<String, String> {
    headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn routes_forward_the_path_and_query_without_credentials() {
    let (origin, seen) = upstream();
    let options = SiteServerOptions {
        proxy: Some(proxy(
            vec![("/api".into(), format!("{origin}/v1"))],
            &["127.0.0.1"],
        )),
        ..Default::default()
    };
    let t = tree(&[("/index.html", "home")]);
    let h = req(&[
        ("cookie", "secret=1"),
        ("authorization", "Bearer x"),
        ("accept", "text/plain"),
    ]);
    let r = handle_request(&t, &options, "GET", "/api/a%20b/c?q=1", &h);
    assert_eq!(r.status, 201);
    assert_eq!(
        String::from_utf8_lossy(&r.body),
        r#"{"url":"/v1/a%20b/c?q=1"}"#
    );
    let cookies: Vec<&str> = r
        .headers
        .iter()
        .filter(|(k, _)| k == "set-cookie")
        .map(|(_, v)| v.as_str())
        .collect();
    assert_eq!(cookies, ["a=1", "b=2"]);
    assert!(!r.headers.iter().any(|(k, _)| k == "connection"));
    let (_, sent) = seen.lock().unwrap().last().cloned().unwrap();
    assert!(sent.iter().any(|l| l == "accept: text/plain"));
    assert!(!sent
        .iter()
        .any(|l| l.starts_with("cookie:") || l.starts_with("authorization:")));
    let moved = handle_request(&t, &options, "GET", "/api/moved", &h);
    assert_eq!(
        (moved.status, moved.header("location")),
        (302, Some("/elsewhere"))
    );
    let head = handle_request(&t, &options, "HEAD", "/api/x", &h);
    assert_eq!((head.status, head.body.len()), (201, 0));
    assert!(seen
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .0
        .starts_with("HEAD /v1/x "));
}

#[test]
fn redirect_rules_need_an_allowed_host_and_yield_to_files() {
    let (origin, _) = upstream();
    let port = origin.rsplit(':').next().unwrap().to_string();
    let rules = format!("/api/* {origin}/r/:splat 200\n/f/* {origin}/f/:splat 200\n/other/* http://localhost:{port}/:splat 200\n");
    let t = tree(&[
        ("/index.html", "home"),
        ("/f/x", "file"),
        ("/_redirects", &rules),
    ]);
    let options = SiteServerOptions {
        proxy: Some(proxy(vec![], &["127.0.0.1"])),
        ..Default::default()
    };
    let h = req(&[]);
    let r = handle_request(&t, &options, "GET", "/api/z?k=v", &h);
    assert_eq!(String::from_utf8_lossy(&r.body), r#"{"url":"/r/z?k=v"}"#);
    assert_eq!(
        handle_request(&t, &options, "GET", "/f/x", &h).body,
        b"file"
    );
    let refused = handle_request(&t, &options, "GET", "/other/z", &h);
    assert_eq!(refused.status, 502);
    let text = String::from_utf8_lossy(&refused.body).to_string();
    assert!(
        text.starts_with(
            "502 Bad Gateway: error[E_HOST_BLOCKED]: refusing to connect to localhost:"
        ),
        "{text}"
    );
    assert!(text.ends_with("add --allow-hosts localhost\n"), "{text}");
    // No proxy settings (a detached server): refused.
    let bare = handle_request(&t, &SiteServerOptions::default(), "GET", "/api/z", &h);
    assert_eq!(bare.status, 502);
}
