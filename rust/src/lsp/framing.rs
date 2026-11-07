//! LSP base protocol framing: `Content-Length: N\r\n\r\n` + N bytes of JSON.
//! Other headers (`Content-Type`) are accepted and ignored.

use std::io::{self, BufRead, Write};

/// Read one message body. `Ok(None)` at a clean end of input (before any header).
pub fn read_message<R: BufRead>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut length: Option<usize> = None;
    let mut saw_header = false;
    loop {
        let mut line = String::new();
        let n = r.read_line(&mut line)?;
        if n == 0 {
            if saw_header {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "end of input inside message headers",
                ));
            }
            return Ok(None);
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            if !saw_header {
                // Stray blank line between messages: tolerate it.
                continue;
            }
            break;
        }
        saw_header = true;
        let Some((name, value)) = line.split_once(':') else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("malformed header line '{line}'"),
            ));
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            let v = value.trim().parse::<usize>().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid Content-Length '{}'", value.trim()),
                )
            })?;
            length = Some(v);
        }
    }
    let Some(len) = length else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message without Content-Length",
        ));
    };
    // Bodies are small (one Sitefile); refuse absurd sizes rather than allocate them.
    if len > 64 * 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message larger than 64 MiB",
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(Some(body))
}

/// Write one message and flush.
pub fn write_message<W: Write>(w: &mut W, body: &[u8]) -> io::Result<()> {
    write!(w, "Content-Length: {}\r\n\r\n", body.len())?;
    w.write_all(body)?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_headers() {
        let mut buf = Vec::new();
        write_message(&mut buf, br#"{"a":1}"#).unwrap();
        write_message(&mut buf, "{\"é\":2}".as_bytes()).unwrap();
        assert!(buf.starts_with(b"Content-Length: 7\r\n\r\n{\"a\":1}"));
        let mut r = io::Cursor::new(buf);
        assert_eq!(read_message(&mut r).unwrap().unwrap(), br#"{"a":1}"#);
        // Content-Length counts bytes, not characters.
        assert_eq!(
            read_message(&mut r).unwrap().unwrap(),
            "{\"é\":2}".as_bytes()
        );
        assert!(read_message(&mut r).unwrap().is_none());
    }

    #[test]
    fn extra_headers_and_case() {
        let raw = b"content-length: 2\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{}";
        assert_eq!(
            read_message(&mut io::Cursor::new(&raw[..]))
                .unwrap()
                .unwrap(),
            b"{}"
        );
    }

    #[test]
    fn malformed_input() {
        assert!(read_message(&mut io::Cursor::new(&b"Content-Type: x\r\n\r\n{}"[..])).is_err());
        assert!(read_message(&mut io::Cursor::new(&b"Content-Length: abc\r\n\r\n"[..])).is_err());
        assert!(read_message(&mut io::Cursor::new(&b"Content-Length: 10\r\n\r\n{}"[..])).is_err());
        assert!(read_message(&mut io::Cursor::new(&b"Content-Length: 2\r\n"[..])).is_err());
    }
}
