//! Minimal deterministic CBOR (RFC 8949) for Web Bundles: definite lengths only;
//! unsigned/negative ints, byte and text strings, arrays, maps, false/true/null.

#[derive(Debug, Clone, PartialEq)]
pub enum Cbor {
    Uint(u64),
    Nint(u64), // value is -1 - n
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Cbor>),
    Map(Vec<(Cbor, Cbor)>),
    Bool(bool),
    Null,
}

fn head(out: &mut Vec<u8>, major: u8, n: u64) {
    let m = major << 5;
    if n < 24 {
        out.push(m | n as u8);
    } else if n < 0x100 {
        out.extend([m | 24, n as u8]);
    } else if n < 0x10000 {
        out.push(m | 25);
        out.extend((n as u16).to_be_bytes());
    } else if n < 0x1_0000_0000 {
        out.push(m | 26);
        out.extend((n as u32).to_be_bytes());
    } else {
        out.push(m | 27);
        out.extend(n.to_be_bytes());
    }
}

pub fn encode(v: &Cbor) -> Vec<u8> {
    let mut out = Vec::new();
    encode_into(&mut out, v);
    out
}

pub fn encode_into(out: &mut Vec<u8>, v: &Cbor) {
    match v {
        Cbor::Uint(n) => head(out, 0, *n),
        Cbor::Nint(n) => head(out, 1, *n),
        Cbor::Bytes(b) => {
            head(out, 2, b.len() as u64);
            out.extend_from_slice(b);
        }
        Cbor::Text(s) => {
            head(out, 3, s.len() as u64);
            out.extend_from_slice(s.as_bytes());
        }
        Cbor::Array(items) => {
            head(out, 4, items.len() as u64);
            for i in items {
                encode_into(out, i);
            }
        }
        Cbor::Map(pairs) => {
            // Deterministic: keys sorted by their encoded bytes.
            let mut enc: Vec<(Vec<u8>, &Cbor)> = pairs.iter().map(|(k, v)| (encode(k), v)).collect();
            enc.sort_by(|a, b| a.0.cmp(&b.0));
            head(out, 5, enc.len() as u64);
            for (k, v) in enc {
                out.extend(k);
                encode_into(out, v);
            }
        }
        Cbor::Bool(false) => out.push(0xf4),
        Cbor::Bool(true) => out.push(0xf5),
        Cbor::Null => out.push(0xf6),
    }
}

/// Encoded size of a head for `n` (1, 2, 3, 5 or 9 bytes).
pub fn head_size(n: u64) -> usize {
    if n < 24 { 1 } else if n < 0x100 { 2 } else if n < 0x10000 { 3 } else if n < 0x1_0000_0000 { 5 } else { 9 }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CborError {
    pub message: String,
    pub offset: usize,
}

pub struct Limits {
    pub max_depth: u32,
    pub max_items: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_depth: 32, max_items: 1_000_000 }
    }
}

/// Read a head at `pos`: (major type, argument, next position).
pub fn read_head(b: &[u8], pos: usize) -> Result<(u8, u64, usize), CborError> {
    let e = |m: &str| CborError { message: m.into(), offset: pos };
    let ib = *b.get(pos).ok_or_else(|| e("truncated"))?;
    let major = ib >> 5;
    let info = ib & 0x1f;
    let (arg, next) = match info {
        0..=23 => (info as u64, pos + 1),
        24 => (*b.get(pos + 1).ok_or_else(|| e("truncated"))? as u64, pos + 2),
        25 => (u16::from_be_bytes(b.get(pos + 1..pos + 3).ok_or_else(|| e("truncated"))?.try_into().unwrap()) as u64, pos + 3),
        26 => (u32::from_be_bytes(b.get(pos + 1..pos + 5).ok_or_else(|| e("truncated"))?.try_into().unwrap()) as u64, pos + 5),
        27 => (u64::from_be_bytes(b.get(pos + 1..pos + 9).ok_or_else(|| e("truncated"))?.try_into().unwrap()), pos + 9),
        _ => return Err(e("indefinite lengths and reserved values are not supported")),
    };
    Ok((major, arg, next))
}

/// Decode one item at the start of `b`; returns the value and where it ended.
pub fn decode(b: &[u8], limits: &Limits) -> Result<(Cbor, usize), CborError> {
    let mut items = 0u64;
    decode_at(b, 0, 0, limits, &mut items)
}

fn decode_at(b: &[u8], pos: usize, depth: u32, l: &Limits, items: &mut u64) -> Result<(Cbor, usize), CborError> {
    if depth >= l.max_depth {
        return Err(CborError { message: "nested too deeply".into(), offset: pos });
    }
    *items += 1;
    if *items > l.max_items {
        return Err(CborError { message: "too many items".into(), offset: pos });
    }
    let (major, arg, next) = read_head(b, pos)?;
    let take = |n: u64| -> Result<(&[u8], usize), CborError> {
        let n = usize::try_from(n).map_err(|_| CborError { message: "length too large".into(), offset: pos })?;
        let end = next.checked_add(n).filter(|&e| e <= b.len()).ok_or(CborError { message: "length exceeds input".into(), offset: pos })?;
        Ok((&b[next..end], end))
    };
    Ok(match major {
        0 => (Cbor::Uint(arg), next),
        1 => (Cbor::Nint(arg), next),
        2 => {
            let (s, end) = take(arg)?;
            (Cbor::Bytes(s.to_vec()), end)
        }
        3 => {
            let (s, end) = take(arg)?;
            let t = std::str::from_utf8(s).map_err(|_| CborError { message: "invalid UTF-8 text".into(), offset: pos })?;
            (Cbor::Text(t.to_string()), end)
        }
        4 => {
            if arg > (b.len() - next) as u64 {
                return Err(CborError { message: "array length exceeds input".into(), offset: pos });
            }
            let mut v = Vec::with_capacity(arg as usize);
            let mut p = next;
            for _ in 0..arg {
                let (item, np) = decode_at(b, p, depth + 1, l, items)?;
                v.push(item);
                p = np;
            }
            (Cbor::Array(v), p)
        }
        5 => {
            if arg > ((b.len() - next) / 2) as u64 {
                return Err(CborError { message: "map length exceeds input".into(), offset: pos });
            }
            let mut v: Vec<(Cbor, Cbor)> = Vec::with_capacity(arg as usize);
            // Keys are compared by their encoded bytes, which is exact for CBOR.
            let mut seen: std::collections::HashSet<&[u8]> = std::collections::HashSet::with_capacity(arg as usize);
            let mut p = next;
            for _ in 0..arg {
                let (k, np) = decode_at(b, p, depth + 1, l, items)?;
                if !seen.insert(&b[p..np]) {
                    return Err(CborError { message: "duplicate map key".into(), offset: p });
                }
                let (val, np2) = decode_at(b, np, depth + 1, l, items)?;
                v.push((k, val));
                p = np2;
            }
            (Cbor::Map(v), p)
        }
        7 => match arg {
            20 => (Cbor::Bool(false), next),
            21 => (Cbor::Bool(true), next),
            22 => (Cbor::Null, next),
            _ => return Err(CborError { message: "unsupported simple value or float".into(), offset: pos }),
        },
        _ => return Err(CborError { message: "tags are not supported".into(), offset: pos }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn round_trip() {
        let v = Cbor::Array(vec![Cbor::Uint(500), Cbor::Text("hé".into()), Cbor::Bytes(vec![1, 2]), Cbor::Map(vec![(Cbor::Bytes(b"b".to_vec()), Cbor::Null), (Cbor::Bytes(b"a".to_vec()), Cbor::Bool(true))])]);
        let e = encode(&v);
        let (d, end) = decode(&e, &Limits::default()).unwrap();
        assert_eq!(end, e.len());
        // map keys come back sorted
        if let Cbor::Array(items) = d {
            assert!(matches!(&items[3], Cbor::Map(m) if m[0].0 == Cbor::Bytes(b"a".to_vec())));
        }
    }
    #[test]
    fn rejects_lying_lengths() {
        assert!(decode(&[0x5a, 0xff, 0xff, 0xff, 0xff], &Limits::default()).is_err());
        assert!(decode(&[0x9b, 0, 0, 0, 1, 0, 0, 0, 0], &Limits::default()).is_err());
    }
}
