//! `serve` in a terminal (port of `src/cli/serve-keys.ts`, issue #224):
//! single keys while serving in the foreground, `--open[=path]` and `--qr`.

use super::common::{err, out};
use crate::qr::{encode_qr, qr_terminal};
use crate::server::format_url;
use std::io::Read;
use std::net::UdpSocket;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Open,
    Urls,
    Restart,
    Clear,
    Quit,
    Help,
}

/// What a key does (letters in either case; Ctrl-C and Ctrl-D quit), or None.
pub fn key_action(byte: u8) -> Option<KeyAction> {
    match byte.to_ascii_lowercase() {
        3 | 4 => Some(KeyAction::Quit),
        b'o' => Some(KeyAction::Open),
        b'u' => Some(KeyAction::Urls),
        b'r' => Some(KeyAction::Restart),
        b'c' => Some(KeyAction::Clear),
        b'q' => Some(KeyAction::Quit),
        b'h' => Some(KeyAction::Help),
        _ => None,
    }
}

pub const KEYS_HINT: &str = "Press h for keys\n";
pub const KEYS_HELP: &str = "Keys: o open in the browser, u show the URLs, r rebuild (--watch) or re-read the directory, c clear the screen, q quit, h this help\n";
pub const CLEAR: &str = "\x1b[2J\x1b[3J\x1b[H";

fn is_wildcard(h: &str) -> bool {
    h == "0.0.0.0" || h == "::"
}

/// This machine's address on the network: the local address of a UDP socket
/// "connected" to a public address (nothing is sent), or None without a route.
pub fn network_address(bind_host: &str) -> Option<String> {
    let v6 = bind_host == "::";
    let s = UdpSocket::bind(if v6 { "[::]:0" } else { "0.0.0.0:0" }).ok()?;
    s.connect(if v6 { "[2001:db8::1]:9" } else { "192.0.2.1:9" })
        .ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then(|| ip.to_string())
}

pub struct Urls {
    pub local: String,
    pub network: Option<String>,
}

/// The URLs to show (below `serve --base`): the local one, and the network one
/// when bound to every interface.
pub fn serve_urls(host: &str, port: u16, base: Option<&str>) -> Urls {
    let path = &base.unwrap_or("/")[1..];
    let local = format!("{}{path}", format_url(host, port));
    let network = if is_wildcard(host) {
        network_address(host).map(|a| format!("{}{path}", format_url(&a, port)))
    } else {
        None
    };
    Urls { local, network }
}

/// `u`: the URLs, and a QR code of the network URL when there is one.
pub fn urls_text(urls: &Urls) -> String {
    let mut s = format!("  Local:   {}\n", urls.local);
    if let Some(n) = &urls.network {
        s.push_str(&format!("  Network: {n}\n{}", qr_text(n)));
    }
    s
}

/// `--qr`: the URL's QR code.
pub fn qr_text(url: &str) -> String {
    encode_qr(url).map(|c| qr_terminal(&c)).unwrap_or_default()
}

/// `--open=<path>` (or `--open`, `/`) joined to the server's URL.
pub fn open_url(base: &str, path: &str) -> String {
    format!("{base}{}", path.trim_start_matches('/'))
}

/// Open `url` in the browser: `$BROWSER` when set (run with the URL as its
/// only argument, no shell), else `open` (macOS) or `xdg-open`. A failure is a
/// warning: the server keeps running.
pub fn open_browser(url: &str) {
    let browser = std::env::var("BROWSER").ok().filter(|b| !b.is_empty());
    let (cmd, args): (String, Vec<String>) = match browser {
        Some(b) => (b, vec![url.to_string()]),
        None if cfg!(target_os = "macos") => ("open".into(), vec![url.to_string()]),
        None => ("xdg-open".into(), vec![url.to_string()]),
    };
    let spawned = Command::new(&cmd)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match spawned {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => err(&format!("warning: cannot open the browser ({cmd}): {e}\n")),
    }
}

extern "C" {
    fn isatty(fd: i32) -> i32;
}

/// The terminal's settings before raw mode (`stty -g`), restored on drop.
struct Raw(Option<String>);

impl Drop for Raw {
    fn drop(&mut self) {
        if let Some(saved) = &self.0 {
            let _ = Command::new("stty")
                .arg(saved)
                .stdin(Stdio::inherit())
                .status();
        }
    }
}

fn raw_mode() -> Raw {
    let saved = Command::new("stty")
        .arg("-g")
        .stdin(Stdio::inherit())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    if saved.is_some() {
        let _ = Command::new("stty")
            .args(["-icanon", "-echo", "min", "1"])
            .stdin(Stdio::inherit())
            .status();
    }
    Raw(saved)
}

/// Keys being read; dropping it restores the terminal.
pub struct Keys {
    _raw: Raw,
    /// Set by `q` (and Ctrl-C/Ctrl-D in raw mode).
    pub quit: Arc<AtomicBool>,
}

/// Read single keys from stdin while serving, when stdin is a terminal or
/// `SITEFILE_SERVE_KEYS=1` (tests drive it through a pipe), and run `on_action`
/// for each on a background thread. None when keys are off.
pub fn read_keys(on_action: Box<dyn Fn(KeyAction) + Send>) -> Option<Keys> {
    // SAFETY: isatty only inspects a file descriptor.
    let tty = unsafe { isatty(0) } == 1;
    if !tty && std::env::var("SITEFILE_SERVE_KEYS").ok().as_deref() != Some("1") {
        return None;
    }
    let raw = if tty { raw_mode() } else { Raw(None) };
    let quit = Arc::new(AtomicBool::new(false));
    let q = quit.clone();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 64];
        loop {
            let n = match stdin.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            for &b in &buf[..n] {
                if let Some(a) = key_action(b) {
                    if a == KeyAction::Quit {
                        q.store(true, Ordering::SeqCst);
                    } else {
                        on_action(a);
                    }
                }
            }
        }
    });
    out(KEYS_HINT);
    Some(Keys { _raw: raw, quit })
}

/// After the server listens: `--qr`, `--open[=path]`, then single keys.
/// `restart` runs for `r` (on the key thread).
pub fn terminal(
    qr: bool,
    open: Option<&str>,
    host: &str,
    port: u16,
    base: Option<&str>,
    restart: Box<dyn Fn() + Send>,
) -> Option<Keys> {
    let urls = serve_urls(host, port, base);
    if qr {
        out(&qr_text(urls.network.as_deref().unwrap_or(&urls.local)));
    }
    if let Some(p) = open {
        open_browser(&open_url(&urls.local, p));
    }
    let open_path = open.unwrap_or("/").to_string();
    read_keys(Box::new(move |a| match a {
        KeyAction::Help => out(KEYS_HELP),
        KeyAction::Clear => out(CLEAR),
        KeyAction::Urls => out(&urls_text(&urls)),
        KeyAction::Open => open_browser(&open_url(&urls.local, &open_path)),
        KeyAction::Restart => restart(),
        KeyAction::Quit => {}
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert_eq!(key_action(b'o'), Some(KeyAction::Open));
        assert_eq!(key_action(b'U'), Some(KeyAction::Urls));
        assert_eq!(key_action(3), Some(KeyAction::Quit));
        assert_eq!(key_action(4), Some(KeyAction::Quit));
        assert_eq!(key_action(b'x'), None);
        assert_eq!(
            open_url("http://127.0.0.1:8/", "/a/b"),
            "http://127.0.0.1:8/a/b"
        );
        assert_eq!(open_url("http://127.0.0.1:8/", ""), "http://127.0.0.1:8/");
        let u = Urls {
            local: "http://127.0.0.1:8/".into(),
            network: None,
        };
        assert_eq!(urls_text(&u), "  Local:   http://127.0.0.1:8/\n");
    }
}
