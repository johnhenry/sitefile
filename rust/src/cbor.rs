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

/// `Number.MAX_SAFE_INTEGER`: TypeScript decodes larger integers as `bigint`.
pub const MAX_SAFE: u64 = (1 << 53) - 1;

impl Cbor {
    /// An unsigned integer TypeScript decodes as a `number` (not a `bigint`).
    pub fn as_number(&self) -> Option<u64> {
        match self {
            Cbor::Uint(n) if *n <= MAX_SAFE => Some(*n),
            _ => None,
        }
    }

    /// Identity of a map key as a JavaScript `Map` sees it (SameValueZero):
    /// equal primitives are one key; byte strings, arrays and maps are objects
    /// and never equal anything else.
    fn js_key(&self) -> Option<(u8, std::borrow::Cow<'_, str>)> {
        match self {
            Cbor::Uint(n) => Some((0, n.to_string().into())),
            Cbor::Nint(n) => Some((1, n.to_string().into())),
            Cbor::Text(t) => Some((3, t.as_str().into())),
            Cbor::Bool(b) => Some((7, if *b { "t" } else { "f" }.into())),
            Cbor::Null => Some((7, "n".into())),
            _ => None,
        }
    }
}

impl std::fmt::Display for CborError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (at byte {})", self.message, self.offset)
    }
}

/// Read a head at `pos`: (major type, argument, next position). Definite
/// lengths only: additional info 28-31 (reserved and indefinite) is an error.
pub fn read_head(b: &[u8], pos: usize) -> Result<(u8, u64, usize), CborError> {
    let e = |m: &str| CborError { message: m.into(), offset: pos };
    let ib = *b.get(pos).ok_or_else(|| e("unexpected end of input"))?;
    let major = ib >> 5;
    let info = ib & 0x1f;
    let need = |n: usize| if pos + 1 + n > b.len() { Err(e("unexpected end of input")) } else { Ok(()) };
    let (arg, next) = match info {
        0..=23 => (info as u64, pos + 1),
        24 => {
            need(1)?;
            (b[pos + 1] as u64, pos + 2)
        }
        25 => {
            need(2)?;
            (u16::from_be_bytes([b[pos + 1], b[pos + 2]]) as u64, pos + 3)
        }
        26 => {
            need(4)?;
            (u32::from_be_bytes(b[pos + 1..pos + 5].try_into().unwrap()) as u64, pos + 5)
        }
        27 => {
            need(8)?;
            (u64::from_be_bytes(b[pos + 1..pos + 9].try_into().unwrap()), pos + 9)
        }
        31 => return Err(e("indefinite-length items are not supported")),
        _ => return Err(e("reserved additional information")),
    };
    Ok((major, arg, next))
}

/// Decode one item at the start of `b`; returns the value and where it ended.
/// A port of `decodeCbor` (src/cbor.ts): the same checks, in the same order,
/// with the same messages and offsets.
pub fn decode(b: &[u8], limits: &Limits) -> Result<(Cbor, usize), CborError> {
    let mut items = 0u64;
    decode_at(b, 0, 0, limits, &mut items)
}

fn decode_at(b: &[u8], pos: usize, depth: u32, l: &Limits, items: &mut u64) -> Result<(Cbor, usize), CborError> {
    let err = |message: String, offset: usize| CborError { message, offset };
    *items += 1;
    if *items > l.max_items {
        return Err(err(format!("more than {} items", l.max_items), pos));
    }
    let (major, arg, next) = read_head(b, pos)?;
    let remaining = (b.len() - next) as u64;
    // A declared length can never exceed what is left of the input (every
    // element or byte takes at least one byte), so nothing is sized from it.
    let length = |min: u64| -> Result<usize, CborError> {
        if arg > MAX_SAFE || arg.saturating_mul(min) > remaining {
            return Err(err("declared length exceeds remaining input".into(), pos));
        }
        Ok(arg as usize)
    };
    let nest = || if depth >= l.max_depth { Err(err(format!("nesting deeper than {}", l.max_depth), pos)) } else { Ok(()) };
    Ok(match major {
        0 => (Cbor::Uint(arg), next),
        1 => (Cbor::Nint(arg), next),
        2 => {
            let n = length(1)?;
            (Cbor::Bytes(b[next..next + n].to_vec()), next + n)
        }
        3 => {
            let n = length(1)?;
            let t = std::str::from_utf8(&b[next..next + n]).map_err(|_| err("invalid UTF-8 in text string".into(), next))?;
            // TextDecoder (without ignoreBOM) drops one leading byte order mark.
            (Cbor::Text(t.strip_prefix('\u{feff}').unwrap_or(t).to_string()), next + n)
        }
        4 => {
            let n = length(1)?;
            nest()?;
            let mut v = Vec::with_capacity(n);
            let mut p = next;
            for _ in 0..n {
                let (item, np) = decode_at(b, p, depth + 1, l, items)?;
                v.push(item);
                p = np;
            }
            (Cbor::Array(v), p)
        }
        5 => {
            let n = length(2)?;
            nest()?;
            let mut v: Vec<(Cbor, Cbor)> = Vec::with_capacity(n);
            // Duplicates are found by their encoded bytes, which is exact for CBOR.
            let mut seen: std::collections::HashSet<&[u8]> = std::collections::HashSet::with_capacity(n);
            let mut slots: std::collections::HashMap<(u8, String), usize> = std::collections::HashMap::new();
            let mut p = next;
            for _ in 0..n {
                let (k, np) = decode_at(b, p, depth + 1, l, items)?;
                if !seen.insert(&b[p..np]) {
                    return Err(err("duplicate map key".into(), p));
                }
                let (val, np2) = decode_at(b, np, depth + 1, l, items)?;
                // Like a JavaScript Map: a key equal to an earlier one (the same
                // text or number with a longer head) keeps its place and takes the new value.
                let slot = k.js_key().map(|(t, s)| (t, s.into_owned()));
                match slot.as_ref().and_then(|s| slots.get(s)) {
                    Some(&i) => v[i].1 = val,
                    None => {
                        if let Some(s) = slot {
                            slots.insert(s, v.len());
                        }
                        v.push((k, val));
                    }
                }
                p = np2;
            }
            (Cbor::Map(v), p)
        }
        6 => return Err(err("tags are not supported".into(), pos)),
        _ => match b[pos] {
            0xf4 => (Cbor::Bool(false), next),
            0xf5 => (Cbor::Bool(true), next),
            0xf6 => (Cbor::Null, next),
            _ => return Err(err("unsupported simple value or float".into(), pos)),
        },
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
