//! `EXPORT … AS car`: the tree as a UnixFS DAG in a CARv1 file (port of `src/car.ts`,
//! issue #208). The DAG is the one `ipfs add -r --hidden --cid-version=1` (kubo) builds:
//! 262144-byte chunks as raw blocks, a balanced tree of dag-pb File nodes of at most 174
//! links, dag-pb Directory nodes with links sorted by name (no HAMT), CIDv1 + SHA-256.
//! The CAR holds every block once, root first, then depth first in link order.

use crate::error::{ErrorCode, Result, SitefileError};
use crate::tree::Tree;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};

pub const CHUNK_SIZE: usize = 262_144;
pub const MAX_LINKS: usize = 174;
/// kubo shards a directory (HAMT) when the names and CIDs of its links exceed this many bytes.
pub const HAMT_THRESHOLD: usize = 262_144;

const RAW: u8 = 0x55;
const DAG_PB: u8 = 0x70;

fn varint(mut n: u64, out: &mut Vec<u8>) {
    while n >= 0x80 {
        out.push((n as u8 & 0x7f) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

fn cid_of(codec: u8, block: &[u8]) -> Vec<u8> {
    let mut c = vec![1, codec, 0x12, 0x20];
    c.extend_from_slice(&Sha256::digest(block));
    c
}

/// A CID as text: multibase base32 (lower case, no padding) with the `b` prefix.
pub fn cid_string(cid: &[u8]) -> String {
    const B32: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::from("b");
    let (mut bits, mut value) = (0u32, 0u32);
    for &b in cid {
        value = (value << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            out.push(B32[((value >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
        value &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(B32[((value << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn pb_varint(num: u64, v: u64, out: &mut Vec<u8>) {
    varint(num << 3, out);
    varint(v, out);
}

fn pb_bytes(num: u64, b: &[u8], out: &mut Vec<u8>) {
    varint((num << 3) | 2, out);
    varint(b.len() as u64, out);
    out.extend_from_slice(b);
}

/// UnixFS `Data`: Type, filesize, blocksizes.
fn unixfs(kind: u64, filesize: Option<u64>, blocksizes: &[u64]) -> Vec<u8> {
    let mut out = Vec::new();
    pb_varint(1, kind, &mut out);
    if let Some(s) = filesize {
        pb_varint(3, s, &mut out);
    }
    for s in blocksizes {
        pb_varint(4, *s, &mut out);
    }
    out
}

/// A dag-pb node: links first (field 2), then data (field 1).
fn dag_pb(links: &[(&[u8], &str, u64)], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for (cid, name, tsize) in links {
        let mut l = Vec::new();
        pb_bytes(1, cid, &mut l);
        pb_bytes(2, name.as_bytes(), &mut l);
        pb_varint(3, *tsize, &mut l);
        pb_bytes(2, &l, &mut out);
    }
    pb_bytes(1, data, &mut out);
    out
}

#[derive(Clone)]
struct Node {
    cid: Vec<u8>,
    tsize: u64,
    file_size: u64,
}

#[derive(Default)]
struct Dag {
    blocks: HashMap<Vec<u8>, (Vec<u8>, Vec<Vec<u8>>)>,
}

impl Dag {
    fn put(&mut self, codec: u8, data: Vec<u8>, children: Vec<Vec<u8>>) -> Vec<u8> {
        let cid = cid_of(codec, &data);
        self.blocks.entry(cid.clone()).or_insert((data, children));
        cid
    }

    fn file(&mut self, bytes: &[u8]) -> Node {
        let mut leaves = Vec::new();
        if bytes.is_empty() {
            leaves.push(Node {
                cid: self.put(RAW, Vec::new(), Vec::new()),
                tsize: 0,
                file_size: 0,
            });
        }
        for chunk in bytes.chunks(CHUNK_SIZE) {
            let n = chunk.len() as u64;
            leaves.push(Node {
                cid: self.put(RAW, chunk.to_vec(), Vec::new()),
                tsize: n,
                file_size: n,
            });
        }
        if leaves.len() == 1 {
            return leaves.pop().expect("one leaf");
        }
        let (mut depth, mut span) = (1u32, MAX_LINKS);
        while span < leaves.len() {
            depth += 1;
            span = span.saturating_mul(MAX_LINKS);
        }
        self.file_node(&leaves, depth)
    }

    fn file_node(&mut self, leaves: &[Node], depth: u32) -> Node {
        let children: Vec<Node> = if depth == 1 {
            leaves.to_vec()
        } else {
            let per = MAX_LINKS.pow(depth - 1);
            leaves
                .chunks(per)
                .map(|g| self.file_node(g, depth - 1))
                .collect()
        };
        let file_size: u64 = children.iter().map(|c| c.file_size).sum();
        let sizes: Vec<u64> = children.iter().map(|c| c.file_size).collect();
        let links: Vec<(&[u8], &str, u64)> =
            children.iter().map(|c| (&c.cid[..], "", c.tsize)).collect();
        let block = dag_pb(&links, &unixfs(2, Some(file_size), &sizes));
        let tsize = block.len() as u64 + children.iter().map(|c| c.tsize).sum::<u64>();
        let cid = self.put(
            DAG_PB,
            block,
            children.iter().map(|c| c.cid.clone()).collect(),
        );
        Node {
            cid,
            tsize,
            file_size,
        }
    }

    fn dir(&mut self, path: &str, mut entries: Vec<(String, Node)>) -> Result<Node> {
        entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        let estimate: usize = entries.iter().map(|(n, c)| n.len() + c.cid.len()).sum();
        if estimate > HAMT_THRESHOLD {
            return Err(SitefileError::plain(
                ErrorCode::E_EXPORT,
                format!("car export: directory '{path}' has too many entries for a plain UnixFS directory (IPFS would shard it; HAMT directories are not supported)"),
            ));
        }
        let links: Vec<(&[u8], &str, u64)> = entries
            .iter()
            .map(|(n, c)| (&c.cid[..], n.as_str(), c.tsize))
            .collect();
        let block = dag_pb(&links, &unixfs(1, None, &[]));
        let tsize = block.len() as u64 + entries.iter().map(|(_, c)| c.tsize).sum::<u64>();
        let cid = self.put(
            DAG_PB,
            block,
            entries.iter().map(|(_, c)| c.cid.clone()).collect(),
        );
        Ok(Node {
            cid,
            tsize,
            file_size: 0,
        })
    }
}

#[derive(Default)]
struct DirEntry<'a> {
    files: BTreeMap<String, &'a [u8]>,
    dirs: BTreeMap<String, DirEntry<'a>>,
}

fn build_dag(tree: &Tree) -> Result<(Dag, Vec<u8>)> {
    let mut top = DirEntry::default();
    for (p, n) in tree.entries() {
        let segs: Vec<&str> = p[1..].split('/').collect();
        let mut d = &mut top;
        for s in &segs[..segs.len() - 1] {
            d = d.dirs.entry((*s).to_string()).or_default();
        }
        d.files
            .insert(segs[segs.len() - 1].to_string(), &n.bytes[..]);
    }
    fn walk(dag: &mut Dag, d: &DirEntry, path: &str) -> Result<Node> {
        let mut entries = Vec::new();
        for (name, sub) in &d.dirs {
            let child = if path == "/" {
                format!("/{name}")
            } else {
                format!("{path}/{name}")
            };
            entries.push((name.clone(), walk(dag, sub, &child)?));
        }
        for (name, bytes) in &d.files {
            entries.push((name.clone(), dag.file(bytes)));
        }
        dag.dir(path, entries)
    }
    let mut dag = Dag::default();
    let root = walk(&mut dag, &top, "/")?.cid;
    Ok((dag, root))
}

/// The CID of one file's UnixFS DAG (a raw block for a file of one chunk).
pub fn file_cid(bytes: &[u8]) -> String {
    cid_string(&Dag::default().file(bytes).cid)
}

/// CARv1 bytes and the root CID.
pub fn encode_car(tree: &Tree) -> Result<(Vec<u8>, String)> {
    let (dag, root) = build_dag(tree)?;
    let mut header = vec![0xa2, 0x65];
    header.extend_from_slice(b"roots");
    header.extend_from_slice(&[0x81, 0xd8, 0x2a, 0x58, (root.len() + 1) as u8, 0x00]);
    header.extend_from_slice(&root);
    header.push(0x67);
    header.extend_from_slice(b"version");
    header.push(0x01);
    let mut out = Vec::new();
    varint(header.len() as u64, &mut out);
    out.extend_from_slice(&header);
    let mut seen = HashSet::new();
    let mut stack = vec![root.clone()];
    while let Some(cid) = stack.pop() {
        if !seen.insert(cid.clone()) {
            continue;
        }
        let (data, children) = &dag.blocks[&cid];
        varint((cid.len() + data.len()) as u64, &mut out);
        out.extend_from_slice(&cid);
        out.extend_from_slice(data);
        for c in children.iter().rev() {
            stack.push(c.clone());
        }
    }
    Ok((out, cid_string(&root)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_cids() {
        // kubo 0.43: `ipfs add --cid-version=1`.
        assert_eq!(
            file_cid(b"hello"),
            "bafkreibm6jg3ux5qumhcn2b3flc3tyu6dmlb4xa7u5bf44yegnrjhc4yeq"
        );
        assert_eq!(
            file_cid(b""),
            "bafkreihdwdcefgh4dqkjv67uzcmw7ojee6xedzdetojuzjevtenxquvyku"
        );
        let x = vec![b'x'; 50_000_000];
        assert_eq!(
            file_cid(&x),
            "bafybeib7wixxgtraw4anto5fi2eadaxjihpukuqbuuonv6w4njmny5lk2i"
        );
    }

    #[test]
    fn tree_matches_kubo() {
        use crate::tree::make_file;
        let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let tree = Tree::from_files(vec![
            (
                "/a.txt".to_string(),
                make_file(b"hello".to_vec(), "t", 0o644),
            ),
            ("/empty".to_string(), make_file(Vec::new(), "t", 0o644)),
            (
                "/.hidden".to_string(),
                make_file(b".h".to_vec(), "t", 0o644),
            ),
            ("/sub/big.bin".to_string(), make_file(big, "t", 0o644)),
            (
                "/sub/deeper/huge.txt".to_string(),
                make_file(vec![b'x'; 50_000_000], "t", 0o755),
            ),
            (
                "/sub/\u{fc} name.txt".to_string(),
                make_file(b"z".to_vec(), "t", 0o644),
            ),
        ])
        .unwrap();
        let (bytes, root) = encode_car(&tree).unwrap();
        assert_eq!(
            root,
            "bafybeiavtrkscgzjs4rysgzmsgbgoaq646dyz75fiee5jkoe67dvgminy4"
        );
        assert_eq!(
            crate::digest::sha256(&bytes),
            "sha256:71cc6a2157a41e5599ead9624df691744e72b8c6da0691100495c715a134920c"
        );
    }
}
