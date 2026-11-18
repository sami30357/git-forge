// Git object model, pkt-lines, packfiles, tree/commit/tag parsing, tree path
// operations, diff and merge helpers. Semantics mirror the Go forge exactly
// (plain objects, SHA-1 ids, OFS/REF delta resolution on read).

use crate::flate::{zlib_deflate, zlib_inflate, FlateError};
use crate::sha1::{hash_object, sha1};
use crate::util::is_oid_bytes;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObjType {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl ObjType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ObjType::Commit => "commit",
            ObjType::Tree => "tree",
            ObjType::Blob => "blob",
            ObjType::Tag => "tag",
        }
    }
    pub fn code(&self) -> u8 {
        match self {
            ObjType::Commit => 1,
            ObjType::Tree => 2,
            ObjType::Blob => 3,
            ObjType::Tag => 4,
        }
    }
    pub fn from_code(code: u8) -> Option<ObjType> {
        match code {
            1 => Some(ObjType::Commit),
            2 => Some(ObjType::Tree),
            3 => Some(ObjType::Blob),
            4 => Some(ObjType::Tag),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct GitObject {
    pub typ: ObjType,
    pub data: Vec<u8>,
}

pub const ZERO_OID: &str = "0000000000000000000000000000000000000000";
pub const EMPTY_TREE_OID: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

pub fn hash_obj(typ: ObjType, data: &[u8]) -> String {
    hash_object(typ.as_str(), data)
}

pub type Lookup = dyn Fn(&str) -> Option<Arc<GitObject>> + Send + Sync;

// ---------------------------------------------------------------------------
// pkt-lines
// ---------------------------------------------------------------------------

pub fn pkt_line(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 4);
    push_hex4(&mut out, payload.len() + 4);
    out.extend_from_slice(payload);
    out
}

pub fn push_hex4(out: &mut Vec<u8>, n: usize) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    out.push(DIGITS[(n >> 12) & 0xf]);
    out.push(DIGITS[(n >> 8) & 0xf]);
    out.push(DIGITS[(n >> 4) & 0xf]);
    out.push(DIGITS[n & 0xf]);
}

pub fn pkt_flush() -> &'static [u8] {
    b"0000"
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PktKind {
    Flush,
    Delim,
    Data,
}

#[derive(Clone, Copy, Debug)]
pub struct Pkt {
    pub kind: PktKind,
    pub start: usize, // payload start (after the 4-byte header)
    pub end: usize,   // payload end
}

fn is_hex4(b: &[u8]) -> bool {
    b.iter().all(|&c| c.is_ascii_hexdigit())
}

fn hex_val4(b: &[u8]) -> usize {
    let mut n = 0usize;
    for &c in b {
        n <<= 4;
        n += match c {
            b'0'..=b'9' => (c - b'0') as usize,
            b'a'..=b'f' => (c - b'a' + 10) as usize,
            b'A'..=b'F' => (c - b'A' + 10) as usize,
            _ => 0,
        };
    }
    n
}

/// Decode pkt-lines from the head of buf. Stops at the first flush (returning
/// the bytes after it as rest) or at the end of the buffer.
pub fn decode_pkt_lines(buf: &[u8]) -> (Vec<Pkt>, &[u8]) {
    let mut packets = Vec::new();
    let mut offset = 0usize;
    while offset + 4 <= buf.len() {
        let header = &buf[offset..offset + 4];
        if !is_hex4(header) {
            break;
        }
        let length = hex_val4(header);
        if length == 0 {
            packets.push(Pkt { kind: PktKind::Flush, start: offset + 4, end: offset + 4 });
            return (packets, &buf[offset + 4..]);
        }
        if length == 1 {
            packets.push(Pkt { kind: PktKind::Delim, start: offset + 4, end: offset + 4 });
            offset += 4;
            continue;
        }
        if length == 2 || length == 3 || length > 65520 || offset + length > buf.len() {
            break;
        }
        packets.push(Pkt { kind: PktKind::Data, start: offset + 4, end: offset + length });
        offset += length;
    }
    (packets, &buf[offset..])
}

// ---------------------------------------------------------------------------
// tree / commit / tag
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct TreeEntry {
    pub mode: String,
    pub name: String,
    pub oid: String,
}

pub fn parse_tree(data: &[u8]) -> Result<Vec<TreeEntry>, ()> {
    let mut entries = Vec::new();
    let mut offset = 0usize;
    while offset < data.len() {
        let rel = data[offset..].iter().position(|&c| c == b' ');
        let space = match rel {
            Some(r) => offset + r,
            None => return Err(()),
        };
        let nul_rel = data[space + 1..].iter().position(|&c| c == 0);
        let nul = match nul_rel {
            Some(r) => space + 1 + r,
            None => return Err(()),
        };
        if nul + 21 > data.len() {
            return Err(());
        }
        let mut hex = [0u8; 40];
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        for (i, &b) in data[nul + 1..nul + 21].iter().enumerate() {
            hex[i * 2] = DIGITS[(b >> 4) as usize];
            hex[i * 2 + 1] = DIGITS[(b & 0xf) as usize];
        }
        entries.push(TreeEntry {
            mode: String::from_utf8_lossy(&data[offset..space]).into_owned(),
            name: String::from_utf8_lossy(&data[space + 1..nul]).into_owned(),
            oid: String::from_utf8_lossy(&hex).into_owned(),
        });
        offset = nul + 21;
    }
    Ok(entries)
}

fn compare_tree_names(a: &TreeEntry, b: &TreeEntry) -> std::cmp::Ordering {
    let mut an = a.name.clone();
    if a.mode == "40000" {
        an.push('/');
    }
    let mut bn = b.name.clone();
    if b.mode == "40000" {
        bn.push('/');
    }
    an.as_bytes().cmp(bn.as_bytes())
}

pub fn build_tree(entries: &[TreeEntry]) -> Vec<u8> {
    let mut sorted: Vec<TreeEntry> = entries.to_vec();
    sorted.sort_by(compare_tree_names);
    let mut out = Vec::new();
    for e in &sorted {
        out.extend_from_slice(e.mode.as_bytes());
        out.push(b' ');
        out.extend_from_slice(e.name.as_bytes());
        out.push(0);
        // hex oid -> 20 raw bytes
        let mut raw = [0u8; 20];
        for i in 0..20 {
            let hi = hex_nibble(e.oid.as_bytes()[i * 2]);
            let lo = hex_nibble(e.oid.as_bytes()[i * 2 + 1]);
            raw[i] = (hi << 4) | lo;
        }
        out.extend_from_slice(&raw);
    }
    out
}

fn hex_nibble(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0,
    }
}

#[derive(Clone, Default)]
pub struct CommitView {
    pub tree: String,
    pub parents: Vec<String>,
    pub author: String,
    pub committer: String,
    pub message: String,
}

pub fn parse_commit(data: &[u8]) -> CommitView {
    let blank = find_subslice(data, b"\n\n");
    let header_end = blank.unwrap_or(data.len());
    let mut cv = CommitView::default();
    for line in data[..header_end].split(|&c| c == b'\n') {
        if line.is_empty() {
            continue;
        }
        let sp = line.iter().position(|&c| c == b' ');
        let (key, value) = match sp {
            Some(i) => (
                String::from_utf8_lossy(&line[..i]).into_owned(),
                String::from_utf8_lossy(&line[i + 1..]).into_owned(),
            ),
            None => (String::from_utf8_lossy(line).into_owned(), String::new()),
        };
        match key.as_str() {
            "tree" => cv.tree = value,
            "parent" => cv.parents.push(value),
            "author" => cv.author = value,
            "committer" => cv.committer = value,
            _ => {}
        }
    }
    if let Some(b) = blank {
        cv.message = String::from_utf8_lossy(&data[b + 2..]).into_owned();
    }
    cv
}

pub fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

pub const IDENT: &str = "ada iversen <adaiversen@example.invalid>";

pub fn build_commit(tree: &str, parents: &[String], message: &str, timestamp: i64) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(format!("tree {}\n", tree).as_bytes());
    for p in parents {
        out.extend_from_slice(format!("parent {}\n", p).as_bytes());
    }
    out.extend_from_slice(format!("author {} {} +0000\n", IDENT, timestamp).as_bytes());
    out.extend_from_slice(format!("committer {} {} +0000\n", IDENT, timestamp).as_bytes());
    out.push(b'\n');
    out.extend_from_slice(message.as_bytes());
    out
}

#[derive(Clone, Default)]
pub struct TagView {
    pub object: String,
    pub typ: String,
    pub tag: String,
    pub message: String,
}

pub fn parse_tag(data: &[u8]) -> TagView {
    let blank = find_subslice(data, b"\n\n");
    let header_end = blank.unwrap_or(data.len());
    let mut tv = TagView::default();
    for line in data[..header_end].split(|&c| c == b'\n') {
        if line.is_empty() {
            continue;
        }
        let sp = match line.iter().position(|&c| c == b' ') {
            Some(i) => i,
            None => continue,
        };
        let key = String::from_utf8_lossy(&line[..sp]).into_owned();
        let value = String::from_utf8_lossy(&line[sp + 1..]).into_owned();
        match key.as_str() {
            "object" => tv.object = value,
            "type" => tv.typ = value,
            "tag" => tv.tag = value,
            _ => {}
        }
    }
    if let Some(b) = blank {
        tv.message = String::from_utf8_lossy(&data[b + 2..]).into_owned();
    }
    tv
}

// ---------------------------------------------------------------------------
// packfiles
// ---------------------------------------------------------------------------

fn pack_object_header(typ: ObjType, size: usize) -> Vec<u8> {
    let code = typ.code();
    let mut out = Vec::new();
    let mut remaining = size;
    let mut first = (code << 4) | ((remaining & 0x0f) as u8);
    remaining >>= 4;
    while remaining > 0 {
        out.push(first | 0x80);
        first = (remaining & 0x7f) as u8;
        remaining >>= 7;
    }
    out.push(first);
    out
}

pub fn write_pack(objects: &[GitObject]) -> Vec<u8> {
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(b"PACK");
    body.extend_from_slice(&[0, 0, 0, 2]);
    body.extend_from_slice(&(objects.len() as u32).to_be_bytes());
    for obj in objects {
        body.extend_from_slice(&pack_object_header(obj.typ, obj.data.len()));
        body.extend_from_slice(&zlib_deflate(&obj.data));
    }
    let sum = sha1(&body);
    body.extend_from_slice(&sum);
    body
}

fn read_varint_size(buf: &[u8], start: usize) -> Result<(usize, usize, u8), String> {
    if start >= buf.len() {
        return Err("pack truncated".to_string());
    }
    let mut b = buf[start];
    let first = b;
    let mut size = (b & 0x0f) as usize;
    let mut shift = 4u32;
    let mut offset = start + 1;
    while b & 0x80 != 0 {
        if offset >= buf.len() {
            return Err("pack truncated".to_string());
        }
        b = buf[offset];
        offset += 1;
        size += ((b & 0x7f) as usize) << shift;
        shift += 7;
    }
    Ok((size, offset, first))
}

fn read_ofs_delta_base(buf: &[u8], start: usize) -> Result<(usize, usize), String> {
    if start >= buf.len() {
        return Err("pack truncated".to_string());
    }
    let mut b = buf[start];
    let mut offset = start + 1;
    let mut value = (b & 0x7f) as usize;
    while b & 0x80 != 0 {
        if offset >= buf.len() {
            return Err("pack truncated".to_string());
        }
        b = buf[offset];
        offset += 1;
        value = (value + 1) * 128 + (b & 0x7f) as usize;
    }
    Ok((value, offset))
}

fn apply_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>, String> {
    let mut offset = 0usize;
    let mut read_size = || -> Result<usize, String> {
        let mut value = 0usize;
        let mut shift = 0u32;
        loop {
            if offset >= delta.len() {
                return Err("delta truncated".to_string());
            }
            let b = delta[offset];
            offset += 1;
            value += ((b & 0x7f) as usize) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                break;
            }
        }
        Ok(value)
    };
    let source_size = read_size()?;
    let target_size = read_size()?;
    if source_size != base.len() {
        return Err("delta base size mismatch".to_string());
    }
    let mut out = vec![0u8; target_size];
    let mut written = 0usize;
    while offset < delta.len() {
        let op = delta[offset];
        offset += 1;
        if op & 0x80 != 0 {
            let mut copy_offset = 0usize;
            let mut copy_size = 0usize;
            let mut shift = 0u32;
            for bit in 0..4 {
                if op & (1 << bit) != 0 {
                    if offset >= delta.len() {
                        return Err("delta truncated".to_string());
                    }
                    copy_offset += (delta[offset] as usize) << shift;
                    offset += 1;
                }
                shift += 8;
            }
            shift = 0;
            for bit in 4..7 {
                if op & (1 << bit) != 0 {
                    if offset >= delta.len() {
                        return Err("delta truncated".to_string());
                    }
                    copy_size += (delta[offset] as usize) << shift;
                    offset += 1;
                }
                shift += 8;
            }
            if copy_size == 0 {
                copy_size = 0x10000;
            }
            if copy_offset + copy_size > base.len() || written + copy_size > out.len() {
                return Err("delta copy out of range".to_string());
            }
            out[written..written + copy_size]
                .copy_from_slice(&base[copy_offset..copy_offset + copy_size]);
            written += copy_size;
        } else if op != 0 {
            let literal = (op & 0x7f) as usize;
            if offset + literal > delta.len() || written + literal > out.len() {
                return Err("delta truncated".to_string());
            }
            out[written..written + literal].copy_from_slice(&delta[offset..offset + literal]);
            offset += literal;
            written += literal;
        } else {
            return Err("delta opcode 0".to_string());
        }
    }
    if written != target_size {
        return Err("delta size mismatch".to_string());
    }
    Ok(out)
}

/// Read a packfile, resolving OFS/REF deltas (REF bases may come from the
/// store lookup). Returns oid -> object.
pub fn read_pack(
    pack: &[u8],
    lookup: &Lookup,
) -> Result<HashMap<String, GitObject>, String> {
    match read_pack_flat(pack) {
        Ok(objs) => Ok(objs),
        Err(Flat::Delta) => read_pack_general(pack, lookup),
        Err(Flat::Err(e)) => Err(e),
    }
}

enum Flat {
    Delta,
    Err(String),
}

/// Fast path for delta-free packs. Returns Delta the moment a delta entry
/// (or unknown type code) appears, so the general path handles it.
fn read_pack_flat(pack: &[u8]) -> Result<HashMap<String, GitObject>, Flat> {
    if pack.len() < 32 {
        return Err(Flat::Err("pack too short".to_string()));
    }
    if &pack[0..4] != b"PACK" {
        return Err(Flat::Err("not a pack".to_string()));
    }
    let version = u32::from_be_bytes([pack[4], pack[5], pack[6], pack[7]]);
    if version != 2 && version != 3 {
        return Err(Flat::Err(format!("unsupported pack version {}", version)));
    }
    let declared = u32::from_be_bytes([pack[8], pack[9], pack[10], pack[11]]) as usize;
    let trailer_start = pack.len() - 20;
    let mut objects: HashMap<String, GitObject> = HashMap::with_capacity(declared);
    let mut offset = 12usize;
    for _ in 0..declared {
        let (size, next, first) = match read_varint_size(pack, offset) {
            Ok(v) => v,
            Err(e) => return Err(Flat::Err(e)),
        };
        offset = next;
        let tc = (first >> 4) & 0x07;
        if tc == 6 || tc == 7 {
            return Err(Flat::Delta);
        }
        let typ = match ObjType::from_code(tc) {
            Some(t) => t,
            None => return Err(Flat::Delta),
        };
        let (inflated, consumed) = match zlib_inflate(&pack[offset..trailer_start], size) {
            Ok(v) => v,
            Err(e) => return Err(Flat::Err(format!("inflate: {:?}", e))),
        };
        offset += consumed;
        let oid = hash_obj(typ, &inflated);
        objects.entry(oid).or_insert(GitObject { typ, data: inflated });
    }
    if offset != trailer_start {
        return Err(Flat::Err("pack has trailing bytes before its trailer".to_string()));
    }
    Ok(objects)
}

struct RawEntry {
    offset: usize,
    type_code: u8,
    data: Vec<u8>,
    base_off: usize,
    base_oid: String,
    has_base: bool,
    resolved: Option<Arc<GitObject>>,
}

fn read_pack_general(
    pack: &[u8],
    lookup: &Lookup,
) -> Result<HashMap<String, GitObject>, String> {
    if pack.len() < 32 {
        return Err("pack too short".to_string());
    }
    if &pack[0..4] != b"PACK" {
        return Err("not a pack".to_string());
    }
    let version = u32::from_be_bytes([pack[4], pack[5], pack[6], pack[7]]);
    if version != 2 && version != 3 {
        return Err(format!("unsupported pack version {}", version));
    }
    let declared = u32::from_be_bytes([pack[8], pack[9], pack[10], pack[11]]) as usize;
    let trailer_start = pack.len() - 20;
    let mut raw: Vec<RawEntry> = Vec::with_capacity(declared);
    let mut by_offset: HashMap<usize, usize> = HashMap::with_capacity(declared);
    let mut offset = 12usize;
    for _ in 0..declared {
        let entry_offset = offset;
        let (size, next, first) = read_varint_size(pack, offset)?;
        offset = next;
        let tc = (first >> 4) & 0x07;
        let mut entry = RawEntry {
            offset: entry_offset,
            type_code: tc,
            data: Vec::new(),
            base_off: 0,
            base_oid: String::new(),
            has_base: false,
            resolved: None,
        };
        if tc == 6 {
            let (base_off, next2) = read_ofs_delta_base(pack, offset)?;
            offset = next2;
            entry.base_off = entry_offset - base_off;
            entry.has_base = true;
        } else if tc == 7 {
            if offset + 20 > pack.len() {
                return Err("pack truncated".to_string());
            }
            let mut hex = [0u8; 40];
            const DIGITS: &[u8; 16] = b"0123456789abcdef";
            for (i, &b) in pack[offset..offset + 20].iter().enumerate() {
                hex[i * 2] = DIGITS[(b >> 4) as usize];
                hex[i * 2 + 1] = DIGITS[(b & 0xf) as usize];
            }
            entry.base_oid = String::from_utf8_lossy(&hex).into_owned();
            entry.has_base = true;
            offset += 20;
        } else if ObjType::from_code(tc).is_none() {
            return Err(format!("unknown pack type code {}", tc));
        }
        let (inflated, consumed) = zlib_inflate(&pack[offset..trailer_start], size)
            .map_err(|e| format!("inflate: {:?}", e))?;
        if inflated.len() != size {
            return Err("pack entry size mismatch".to_string());
        }
        entry.data = inflated;
        offset += consumed;
        by_offset.insert(entry_offset, raw.len());
        raw.push(entry);
    }
    if offset != trailer_start {
        return Err("pack has trailing bytes before its trailer".to_string());
    }

    let mut objects: HashMap<String, GitObject> = HashMap::with_capacity(raw.len());
    let mut resolved_by_oid: HashMap<String, usize> = HashMap::new();
    // Resolve with an explicit stack (no recursion): resolve(i) walks the
    // base chain iteratively, then applies deltas on the way back.
    for start in 0..raw.len() {
        if raw[start].resolved.is_some() {
            continue;
        }
        // Walk the chain.
        let mut chain: Vec<usize> = Vec::new();
        let mut cur = start;
        let mut resolving: HashSet<usize> = HashSet::new();
        loop {
            if raw[cur].resolved.is_some() {
                break;
            }
            if !resolving.insert(cur) {
                return Err("delta cycle".to_string());
            }
            chain.push(cur);
            if raw[cur].type_code == 6 {
                match by_offset.get(&raw[cur].base_off) {
                    Some(&base_idx) => {
                        cur = base_idx;
                        continue;
                    }
                    None => return Err("ofs-delta base missing".to_string()),
                }
            } else if raw[cur].type_code == 7 {
                // REF delta: base is another entry (by oid) or external.
                if let Some(&base_idx) = resolved_by_oid.get(&raw[cur].base_oid) {
                    cur = base_idx;
                    continue;
                }
                // External base: resolve this entry directly from the store.
                break;
            } else {
                break;
            }
        }
        // Unwind the chain from the base outward.
        while let Some(idx) = chain.pop() {
            if raw[idx].resolved.is_some() {
                continue;
            }
            let (typ, data) = if raw[idx].type_code == 6 {
                let base_idx = by_offset[&raw[idx].base_off];
                let base = raw[base_idx].resolved.clone().unwrap();
                let data = apply_delta(&base.data, &raw[idx].data)?;
                (base.typ, data)
            } else if raw[idx].type_code == 7 {
                if let Some(&base_idx) = resolved_by_oid.get(&raw[idx].base_oid) {
                    let base = raw[base_idx].resolved.clone().unwrap();
                    let data = apply_delta(&base.data, &raw[idx].data)?;
                    (base.typ, data)
                } else {
                    let external = lookup(&raw[idx].base_oid)
                        .ok_or_else(|| format!("ref-delta base {} missing", raw[idx].base_oid))?;
                    let data = apply_delta(&external.data, &raw[idx].data)?;
                    (external.typ, data)
                }
            } else {
                (ObjType::from_code(raw[idx].type_code).unwrap(), raw[idx].data.clone())
            };
            let oid = hash_obj(typ, &data);
            let obj = Arc::new(GitObject { typ, data });
            raw[idx].resolved = Some(obj);
            resolved_by_oid.insert(oid, idx);
        }
    }
    for e in &raw {
        let r = e.resolved.clone().unwrap();
        objects.entry(hash_obj(r.typ, &r.data)).or_insert(GitObject {
            typ: r.typ,
            data: r.data.clone(),
        });
    }
    Ok(objects)
}

// ---------------------------------------------------------------------------
// tree path operations, diff, merge
// ---------------------------------------------------------------------------

/// Flatten a tree to path -> (oid, mode), files only (dirs recursed).
pub fn flatten_tree(
    root_oid: &str,
    lookup: &Lookup,
) -> HashMap<String, (String, String)> {
    let mut out: HashMap<String, (String, String)> = HashMap::new();
    if root_oid.is_empty() {
        return out;
    }
    fn walk(
        tree_oid: &str,
        prefix: &str,
        lookup: &Lookup,
        out: &mut HashMap<String, (String, String)>,
    ) {
        let obj = match lookup(tree_oid) {
            Some(o) if o.typ == ObjType::Tree => o,
            _ => return,
        };
        let entries = match parse_tree(&obj.data) {
            Ok(e) => e,
            Err(()) => return,
        };
        for e in entries {
            let path = if prefix.is_empty() {
                e.name.clone()
            } else {
                format!("{}/{}", prefix, e.name)
            };
            if e.mode == "40000" {
                walk(&e.oid, &path, lookup, out);
            } else {
                out.insert(path, (e.oid, e.mode));
            }
        }
    }
    walk(root_oid, "", lookup, &mut out);
    out
}

pub fn set_path(
    root_oid: &str,
    segments: &[String],
    blob_oid: &str,
    lookup: &Lookup,
    put: &mut dyn FnMut(ObjType, Vec<u8>) -> String,
) -> String {
    fn rec(
        tree_oid: &str,
        depth: usize,
        segments: &[String],
        blob_oid: &str,
        lookup: &Lookup,
        put: &mut dyn FnMut(ObjType, Vec<u8>) -> String,
    ) -> String {
        let mut entries: Vec<TreeEntry> = Vec::new();
        if !tree_oid.is_empty() {
            if let Some(obj) = lookup(tree_oid) {
                entries = parse_tree(&obj.data).unwrap_or_default();
            }
        }
        let name = &segments[depth];
        if depth == segments.len() - 1 {
            if let Some(e) = entries.iter_mut().find(|e| &e.name == name) {
                *e = TreeEntry { mode: "100644".to_string(), name: name.clone(), oid: blob_oid.to_string() };
            } else {
                entries.push(TreeEntry { mode: "100644".to_string(), name: name.clone(), oid: blob_oid.to_string() });
            }
            return put(ObjType::Tree, build_tree(&entries));
        }
        let idx = entries.iter().position(|e| &e.name == name);
        let child_oid = match idx {
            Some(i) if entries[i].mode == "40000" => entries[i].oid.clone(),
            _ => String::new(),
        };
        let new_child = rec(&child_oid, depth + 1, segments, blob_oid, lookup, put);
        if let Some(i) = idx {
            entries[i] = TreeEntry { mode: "40000".to_string(), name: name.clone(), oid: new_child };
        } else {
            entries.push(TreeEntry { mode: "40000".to_string(), name: name.clone(), oid: new_child });
        }
        put(ObjType::Tree, build_tree(&entries))
    }
    rec(root_oid, 0, segments, blob_oid, lookup, put)
}

pub fn delete_path(
    root_oid: &str,
    segments: &[String],
    lookup: &Lookup,
    put: &mut dyn FnMut(ObjType, Vec<u8>) -> String,
) -> String {
    if root_oid.is_empty() {
        return EMPTY_TREE_OID.to_string();
    }
    fn rec(
        tree_oid: &str,
        depth: usize,
        segments: &[String],
        lookup: &Lookup,
        put: &mut dyn FnMut(ObjType, Vec<u8>) -> String,
    ) -> String {
        let mut entries: Vec<TreeEntry> = Vec::new();
        if let Some(obj) = lookup(tree_oid) {
            entries = parse_tree(&obj.data).unwrap_or_default();
        }
        let name = &segments[depth];
        let idx = match entries.iter().position(|e| &e.name == name) {
            Some(i) => i,
            None => return tree_oid.to_string(),
        };
        if depth == segments.len() - 1 {
            entries.remove(idx);
            return put(ObjType::Tree, build_tree(&entries));
        }
        if entries[idx].mode != "40000" {
            return tree_oid.to_string();
        }
        let new_child = rec(&entries[idx].oid, depth + 1, segments, lookup, put);
        if new_child == entries[idx].oid {
            return tree_oid.to_string();
        }
        let child_empty = match lookup(&new_child) {
            Some(obj) => parse_tree(&obj.data).map(|e| e.is_empty()).unwrap_or(false),
            None => false,
        };
        if child_empty {
            entries.remove(idx);
        } else {
            entries[idx] = TreeEntry { mode: "40000".to_string(), name: name.clone(), oid: new_child };
        }
        put(ObjType::Tree, build_tree(&entries))
    }
    rec(root_oid, 0, segments, lookup, put)
}

/// Blob oid at segments, or "" when missing or a directory.
pub fn get_path(root_oid: &str, segments: &[String], lookup: &Lookup) -> String {
    if root_oid.is_empty() {
        return String::new();
    }
    let mut tree_oid = root_oid.to_string();
    for (depth, seg) in segments.iter().enumerate() {
        let obj = match lookup(&tree_oid) {
            Some(o) if o.typ == ObjType::Tree => o,
            _ => return String::new(),
        };
        let entries = match parse_tree(&obj.data) {
            Ok(e) => e,
            Err(()) => return String::new(),
        };
        let found = match entries.iter().find(|e| &e.name == seg) {
            Some(e) => e,
            None => return String::new(),
        };
        if depth == segments.len() - 1 {
            if found.mode == "40000" {
                return String::new();
            }
            return found.oid.clone();
        }
        if found.mode != "40000" {
            return String::new();
        }
        tree_oid = found.oid.clone();
    }
    String::new()
}

/// Entries of the directory at segments (empty = root), or None when the path
/// is not a directory.
pub fn list_dir(root_oid: &str, segments: &[String], lookup: &Lookup) -> Option<Vec<TreeEntry>> {
    if root_oid.is_empty() {
        if segments.is_empty() {
            return Some(Vec::new());
        }
        return None;
    }
    let mut tree_oid = root_oid.to_string();
    for seg in segments {
        let obj = lookup(&tree_oid).filter(|o| o.typ == ObjType::Tree)?;
        let entries = parse_tree(&obj.data).ok()?;
        let found = entries.iter().find(|e| &e.name == seg)?;
        if found.mode != "40000" {
            return None;
        }
        tree_oid = found.oid.clone();
    }
    let obj = lookup(&tree_oid).filter(|o| o.typ == ObjType::Tree)?;
    parse_tree(&obj.data).ok()
}

/// Whether ancestor is reachable from tip.
pub fn is_descendant(tip: &str, ancestor: &str, lookup: &Lookup) -> bool {
    if tip == ancestor {
        return true;
    }
    if let Some(obj) = lookup(tip) {
        if obj.typ == ObjType::Commit {
            for p in &parse_commit(&obj.data).parents {
                if p == ancestor {
                    return true;
                }
            }
        }
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: Vec<String> = vec![tip.to_string()];
    while let Some(oid) = queue.pop() {
        if !seen.insert(oid.clone()) {
            continue;
        }
        let obj = match lookup(&oid) {
            Some(o) if o.typ == ObjType::Commit => o,
            _ => continue,
        };
        for p in &parse_commit(&obj.data).parents {
            if p == ancestor {
                return true;
            }
            queue.push(p.clone());
        }
    }
    false
}

/// First common ancestor of two commits, or "".
pub fn merge_base(a: &str, b: &str, lookup: &Lookup) -> String {
    let mut ancestors: HashSet<String> = HashSet::new();
    let mut queue_a: Vec<String> = vec![a.to_string()];
    while let Some(oid) = queue_a.pop() {
        if !ancestors.insert(oid.clone()) {
            continue;
        }
        if let Some(obj) = lookup(&oid) {
            if obj.typ == ObjType::Commit {
                queue_a.extend(parse_commit(&obj.data).parents.iter().cloned());
            }
        }
    }
    let mut seen_b: HashSet<String> = HashSet::new();
    let mut queue_b: Vec<String> = vec![b.to_string()];
    while let Some(oid) = queue_b.pop() {
        if !seen_b.insert(oid.clone()) {
            continue;
        }
        if ancestors.contains(&oid) {
            return oid;
        }
        if let Some(obj) = lookup(&oid) {
            if obj.typ == ObjType::Commit {
                queue_b.extend(parse_commit(&obj.data).parents.iter().cloned());
            }
        }
    }
    String::new()
}

#[derive(Clone, Debug)]
pub struct FileChange {
    pub filename: String,
    pub status: String, // added | modified | removed
    pub additions: i64,
    pub deletions: i64,
    pub changes: i64,
    pub old_oid: String,
    pub new_oid: String,
}

/// Multiset line difference: [added, removed].
pub fn line_diff(old_text: &str, new_text: &str) -> (i64, i64) {
    let count = |text: &str| -> HashMap<String, i64> {
        let mut m: HashMap<String, i64> = HashMap::new();
        if text.is_empty() {
            return m;
        }
        for line in text.split('\n') {
            *m.entry(line.to_string()).or_insert(0) += 1;
        }
        m
    };
    let a = count(old_text);
    let b = count(new_text);
    let mut added = 0i64;
    let mut removed = 0i64;
    for (line, n) in &b {
        let d = n - a.get(line).copied().unwrap_or(0);
        if d > 0 {
            added += d;
        }
    }
    for (line, n) in &a {
        let d = n - b.get(line).copied().unwrap_or(0);
        if d > 0 {
            removed += d;
        }
    }
    (added, removed)
}

pub fn diff_trees(base_tip: &str, head_tip: &str, lookup: &Lookup) -> Vec<FileChange> {
    let tree_of = |tip: &str| -> String {
        if tip.is_empty() {
            return String::new();
        }
        match lookup(tip) {
            Some(obj) => parse_commit(&obj.data).tree,
            None => String::new(),
        }
    };
    let base_flat = flatten_tree(&tree_of(base_tip), lookup);
    let head_flat = flatten_tree(&tree_of(head_tip), lookup);
    let mut paths: HashSet<String> = HashSet::new();
    for p in base_flat.keys() {
        paths.insert(p.clone());
    }
    for p in head_flat.keys() {
        paths.insert(p.clone());
    }
    let mut sorted: Vec<String> = paths.into_iter().collect();
    sorted.sort();
    let mut out = Vec::new();
    for path in sorted {
        let old_e = base_flat.get(&path);
        let new_e = head_flat.get(&path);
        if let (Some(o), Some(n)) = (old_e, new_e) {
            if o.0 == n.0 {
                continue;
            }
        }
        let old_text = old_e
            .and_then(|(oid, _)| lookup(oid))
            .map(|o| String::from_utf8_lossy(&o.data).into_owned())
            .unwrap_or_default();
        let new_text = new_e
            .and_then(|(oid, _)| lookup(oid))
            .map(|o| String::from_utf8_lossy(&o.data).into_owned())
            .unwrap_or_default();
        let (additions, deletions) = line_diff(&old_text, &new_text);
        let status = if old_e.is_none() {
            "added"
        } else if new_e.is_none() {
            "removed"
        } else {
            "modified"
        };
        out.push(FileChange {
            filename: path,
            status: status.to_string(),
            additions,
            deletions,
            changes: additions + deletions,
            old_oid: old_e.map(|e| e.0.clone()).unwrap_or_default(),
            new_oid: new_e.map(|e| e.0.clone()).unwrap_or_default(),
        });
    }
    out
}

/// Whether merging head into base would conflict.
pub fn merge_conflicts(base_tip: &str, head_tip: &str, lookup: &Lookup) -> bool {
    let base = merge_base(base_tip, head_tip, lookup);
    if base.is_empty() {
        return false;
    }
    let tree_of = |tip: &str| -> String {
        match lookup(tip) {
            Some(obj) => parse_commit(&obj.data).tree,
            None => String::new(),
        }
    };
    let base_flat = flatten_tree(&tree_of(&base), lookup);
    let base_tip_flat = flatten_tree(&tree_of(base_tip), lookup);
    let head_tip_flat = flatten_tree(&tree_of(head_tip), lookup);
    let mut paths: HashSet<String> = HashSet::new();
    for p in base_flat.keys() {
        paths.insert(p.clone());
    }
    for p in base_tip_flat.keys() {
        paths.insert(p.clone());
    }
    for p in head_tip_flat.keys() {
        paths.insert(p.clone());
    }
    for path in paths {
        let m = base_flat.get(&path).map(|e| e.0.as_str()).unwrap_or("");
        let b = base_tip_flat.get(&path).map(|e| e.0.as_str()).unwrap_or("");
        let h = head_tip_flat.get(&path).map(|e| e.0.as_str()).unwrap_or("");
        if b != m && h != m && b != h {
            return true;
        }
    }
    false
}

// Unused-import guard for FlateError (kept for the error mapping above).
#[allow(dead_code)]
fn _flate_err(_: FlateError) {}
