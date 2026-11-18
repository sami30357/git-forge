// Raw TCP HTTP/1.1 server: no framework, no header maps, thread per
// connection, requests on a connection handled strictly in order.
// The head terminator is found with SWAR (8 bytes at a time, branchless
// byte-match) instead of a byte-at-a-time scan; responses go out with one
// writev (head + body).

use crate::util::{path_unescape, query_unescape};
use std::collections::HashMap;
use std::io::{IoSlice, Read, Write};
use std::net::{TcpListener, TcpStream};

#[derive(Clone)]
pub enum ResponseBody {
    Owned(Vec<u8>),
    Shared(std::sync::Arc<Vec<u8>>),
}

impl ResponseBody {
    pub fn as_slice(&self) -> &[u8] {
        match self {
            ResponseBody::Owned(v) => v,
            ResponseBody::Shared(v) => v,
        }
    }
    pub fn len(&self) -> usize {
        self.as_slice().len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl From<Vec<u8>> for ResponseBody {
    fn from(v: Vec<u8>) -> Self {
        ResponseBody::Owned(v)
    }
}

pub struct HttpRequest {
    pub method: String,
    pub path: String, // decoded path (no query)
    pub query: HashMap<String, Vec<String>>,
    pub body: Vec<u8>,
    pub host: String,
    pub auth: String,
    pub git_protocol: String,
    pub keep_alive: bool,
}

pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>, // extra headers, in order
    pub body: ResponseBody,
    pub content_type: Option<String>, // overrides the default application/json
    pub head_only: bool,
    pub no_content_type: bool,
    pub no_content_length: bool,
}

impl HttpResponse {
    pub fn new(status: u16, body: ResponseBody) -> HttpResponse {
        HttpResponse {
            status,
            headers: Vec::new(),
            body,
            content_type: None,
            head_only: false,
            no_content_type: false,
            no_content_length: false,
        }
    }
}

const REASONS: &[(u16, &str)] = &[
    (200, "OK"),
    (201, "Created"),
    (202, "Accepted"),
    (204, "No Content"),
    (400, "Bad Request"),
    (401, "Unauthorized"),
    (404, "Not Found"),
    (405, "Method Not Allowed"),
    (409, "Conflict"),
    (422, "Unprocessable Entity"),
    (500, "Internal Server Error"),
];

fn reason(status: u16) -> &'static str {
    for (code, text) in REASONS {
        if *code == status {
            return text;
        }
    }
    "Status"
}

// --- SWAR head-terminator scan -------------------------------------------
// Find "\r\n\r\n" by locating '\n' bytes 8 at a time (SWAR has-byte), then
// checking the 3 preceding bytes. The Go core uses bytes.Index (memchr);
// this is the same idea expressed branchlessly over whole words.

#[inline]
fn swar_has_byte(word: u64, byte: u8) -> u64 {
    let repeated = (byte as u64).wrapping_mul(0x0101_0101_0101_0101);
    let x = word ^ repeated;
    x.wrapping_sub(0x0101_0101_0101_0101) & !x & 0x8080_8080_8080_8080
}

pub fn find_head_end(buf: &[u8]) -> Option<usize> {
    let mut i = 0usize;
    while i + 8 <= buf.len() {
        let word = u64::from_ne_bytes(buf[i..i + 8].try_into().unwrap());
        let mask = swar_has_byte(word, b'\n');
        if mask != 0 {
            // First '\n' in this word.
            let bit = mask.trailing_zeros() / 8;
            let pos = i + bit as usize;
            if pos >= 3
                && buf[pos - 3] == b'\r'
                && buf[pos - 2] == b'\n'
                && buf[pos - 1] == b'\r'
            {
                return Some(pos + 1);
            }
            // A '\n' that is not the end of "\r\n\r\n": keep scanning from
            // just past it (a terminator cannot start inside the 3 bytes
            // before this '\n', and the next candidate '\n' is at or after
            // pos+1; resuming at pos+1 is safe because any "\r\n\r\n"
            // ending at a later '\n' starts after pos-2 >= pos+1-3... to
            // stay obviously correct, resume at pos+1 and let the byte
            // loop below handle the overlap region).
            i = pos + 1;
            // Fall through to the byte loop for the tail of this word to
            // avoid missing a terminator straddling the resume point.
            while i < buf.len() && i < pos + 8 {
                if buf[i] == b'\n'
                    && i >= 3
                    && buf[i - 3] == b'\r'
                    && buf[i - 2] == b'\n'
                    && buf[i - 1] == b'\r'
                {
                    return Some(i + 1);
                }
                i += 1;
            }
            continue;
        }
        i += 8;
    }
    while i < buf.len() {
        if buf[i] == b'\n'
            && i >= 3
            && buf[i - 3] == b'\r'
            && buf[i - 2] == b'\n'
            && buf[i - 1] == b'\r'
        {
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

// --- request parsing -------------------------------------------------------

pub enum ParseOutcome {
    Request(HttpRequest, usize), // request + bytes consumed
    NeedMore,
    Error,
}

fn parse_chunked(buf: &[u8]) -> Option<(Vec<u8>, usize)> {
    let mut out: Vec<u8> = Vec::new();
    let mut pos = 0usize;
    loop {
        let eol = find_crlf(&buf[pos..]).map(|e| pos + e)?;
        let mut line = &buf[pos..eol];
        if let Some(semi) = line.iter().position(|&c| c == b';') {
            line = &line[..semi];
        }
        let text = std::str::from_utf8(line).ok()?.trim();
        let size = usize::from_str_radix(text, 16).ok()?;
        pos = eol + 2;
        if size == 0 {
            loop {
                let eol2 = find_crlf(&buf[pos..]).map(|e| pos + e)?;
                if eol2 == pos {
                    return Some((out, pos + 2));
                }
                pos = eol2 + 2;
            }
        }
        if buf.len() < pos + size + 2 {
            return None;
        }
        out.extend_from_slice(&buf[pos..pos + size]);
        pos += size + 2;
    }
}

fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

pub fn parse_request(buf: &[u8]) -> ParseOutcome {
    let head_end = match find_head_end(buf) {
        Some(e) => e,
        None => {
            if buf.len() > 65536 {
                return ParseOutcome::Error;
            }
            return ParseOutcome::NeedMore;
        }
    };
    let head = &buf[..head_end];
    let line_end = head.windows(2).position(|w| w == b"\r\n").unwrap_or(head.len());
    let req_line = &head[..line_end];
    let sp1 = match req_line.iter().position(|&c| c == b' ') {
        Some(i) => i,
        None => return ParseOutcome::Error,
    };
    let sp2 = match req_line.iter().rposition(|&c| c == b' ') {
        Some(i) if i > sp1 => i,
        _ => return ParseOutcome::Error,
    };
    let version = &req_line[sp2 + 1..];
    if version != b"HTTP/1.1" && version != b"HTTP/1.0" {
        return ParseOutcome::Error;
    }
    let method = String::from_utf8_lossy(&req_line[..sp1]).into_owned();
    let mut target = String::from_utf8_lossy(&req_line[sp1 + 1..sp2]).into_owned();
    if target.starts_with("http://") || target.starts_with("https://") {
        if let Some(scheme_end) = target.find("://") {
            let rest = &target[scheme_end + 3..];
            match rest.find('/') {
                Some(slash) => target = rest[slash..].to_string(),
                None => target = "/".to_string(),
            }
        }
    }
    let (raw_path, raw_q) = match target.find('?') {
        Some(q) => (target[..q].to_string(), target[q + 1..].to_string()),
        None => (target, String::new()),
    };
    let decoded_path = match path_unescape(&raw_path) {
        Some(p) => p,
        None => return ParseOutcome::Error,
    };
    let mut req = HttpRequest {
        method,
        path: decoded_path,
        query: HashMap::new(),
        body: Vec::new(),
        host: String::new(),
        auth: String::new(),
        git_protocol: String::new(),
        keep_alive: version == b"HTTP/1.1",
    };
    if !raw_q.is_empty() {
        req.query = parse_query(&raw_q);
    }
    // Headers, line by line; keep only what the forge reads.
    let mut pos = if line_end < head.len() { line_end + 2 } else { head.len() };
    let mut content_length: i64 = -1;
    let mut chunked = false;
    while pos < head.len() {
        let eol = match head[pos..].windows(2).position(|w| w == b"\r\n") {
            Some(e) => pos + e,
            None => head.len(),
        };
        let line = &head[pos..eol];
        pos = if eol < head.len() { eol + 2 } else { head.len() };
        let colon = match line.iter().position(|&c| c == b':') {
            Some(c) => c,
            None => continue,
        };
        let name = &line[..colon];
        let value = trim_bytes(&line[colon + 1..]);
        if eq_ignore_case(name, b"authorization") {
            if req.auth.is_empty() {
                req.auth = String::from_utf8_lossy(value).into_owned();
            }
        } else if eq_ignore_case(name, b"git-protocol") {
            if req.git_protocol.is_empty() {
                req.git_protocol = String::from_utf8_lossy(value).into_owned();
            }
        } else if eq_ignore_case(name, b"host") {
            if req.host.is_empty() {
                req.host = String::from_utf8_lossy(value).into_owned();
            }
        } else if eq_ignore_case(name, b"content-length") {
            if content_length < 0 {
                match std::str::from_utf8(value).ok().and_then(|s| s.parse::<i64>().ok()) {
                    Some(cl) if cl >= 0 => content_length = cl,
                    _ => return ParseOutcome::Error,
                }
            }
        } else if eq_ignore_case(name, b"transfer-encoding") {
            if eq_ignore_case(value, b"chunked") {
                chunked = true;
            }
        } else if eq_ignore_case(name, b"connection") {
            if eq_ignore_case(value, b"close") {
                req.keep_alive = false;
            } else if eq_ignore_case(value, b"keep-alive") {
                req.keep_alive = true;
            }
        }
    }
    let body_start = head_end;
    if content_length >= 0 {
        let cl = content_length as usize;
        if buf.len() < body_start + cl {
            return ParseOutcome::NeedMore;
        }
        req.body = buf[body_start..body_start + cl].to_vec();
        return ParseOutcome::Request(req, body_start + cl);
    }
    if chunked {
        return match parse_chunked(&buf[body_start..]) {
            Some((body, used)) => {
                req.body = body;
                ParseOutcome::Request(req, body_start + used)
            }
            None => ParseOutcome::NeedMore,
        };
    }
    ParseOutcome::Request(req, body_start)
}

fn trim_bytes(b: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < b.len() && matches!(b[start], b' ' | b'\t') {
        start += 1;
    }
    let mut end = b.len();
    while end > start && matches!(b[end - 1], b' ' | b'\t') {
        end -= 1;
    }
    &b[start..end]
}

fn eq_ignore_case(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

pub fn parse_query(q: &str) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    if q.is_empty() {
        return out;
    }
    for part in q.split('&') {
        if part.is_empty() {
            continue;
        }
        let (key, value) = match part.find('=') {
            Some(eq) => (&part[..eq], &part[eq + 1..]),
            None => (part, ""),
        };
        let key = query_unescape(key);
        let value = query_unescape(value);
        out.entry(key).or_default().push(value);
    }
    out
}

pub fn query_get<'a>(q: &'a HashMap<String, Vec<String>>, key: &str) -> &'a str {
    q.get(key).and_then(|v| v.first()).map(|s| s.as_str()).unwrap_or("")
}

// --- response writing ------------------------------------------------------

fn build_head(resp: &HttpResponse, keep_alive: bool, out: &mut Vec<u8>) {
    out.extend_from_slice(b"HTTP/1.1 ");
    out.extend_from_slice(resp.status.to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(reason(resp.status).as_bytes());
    out.extend_from_slice(b"\r\n");
    if !resp.no_content_type {
        match &resp.content_type {
            Some(ct) => {
                out.extend_from_slice(b"content-type: ");
                out.extend_from_slice(ct.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            None => out.extend_from_slice(b"content-type: application/json\r\n"),
        }
    }
    for (k, v) in &resp.headers {
        out.extend_from_slice(k.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(v.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    if !resp.no_content_length {
        out.extend_from_slice(b"content-length: ");
        out.extend_from_slice(resp.body.len().to_string().as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    if keep_alive {
        out.extend_from_slice(b"connection: keep-alive\r\n");
    } else {
        out.extend_from_slice(b"connection: close\r\n");
    }
    out.extend_from_slice(b"\r\n");
}

/// Write the whole response. Returns false when the socket is gone.
pub fn write_response(stream: &mut TcpStream, resp: &HttpResponse, keep_alive: bool) -> bool {
    let mut head: Vec<u8> = Vec::with_capacity(256);
    build_head(resp, keep_alive, &mut head);
    if resp.head_only || resp.body.is_empty() {
        return stream.write_all(&head).is_ok();
    }
    let body = resp.body.as_slice();
    let mut slices = [IoSlice::new(&head), IoSlice::new(body)];
    // write_vectored may write partially; advance through both slices.
    let mut head_off = 0usize;
    let mut body_off = 0usize;
    loop {
        let mut bufs: Vec<IoSlice> = Vec::with_capacity(2);
        if head_off < head.len() {
            bufs.push(IoSlice::new(&head[head_off..]));
        }
        if body_off < body.len() {
            bufs.push(IoSlice::new(&body[body_off..]));
        }
        if bufs.is_empty() {
            return true;
        }
        match stream.write_vectored(&bufs) {
            Ok(0) => return false,
            Ok(n) => {
                let mut remaining = n;
                let head_left = head.len() - head_off;
                if remaining < head_left {
                    head_off += remaining;
                    continue;
                }
                remaining -= head_left;
                head_off = head.len();
                body_off += remaining;
                let _ = &mut slices;
            }
            Err(_) => return false,
        }
    }
}

// --- connection serving ----------------------------------------------------

pub fn serve_conn<F>(mut stream: TcpStream, handler: F)
where
    F: Fn(&HttpRequest) -> HttpResponse,
{
    let _ = stream.set_nodelay(true);
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut tmp = [0u8; 8192];
    loop {
        match parse_request(&buf) {
            ParseOutcome::Request(req, consumed) => {
                let keep_alive = req.keep_alive;
                let resp = handler(&req);
                if !write_response(&mut stream, &resp, keep_alive) {
                    return;
                }
                buf.drain(..consumed);
                if !keep_alive {
                    return;
                }
            }
            ParseOutcome::NeedMore => {
                if buf.len() > 8 << 20 {
                    return;
                }
                match stream.read(&mut tmp) {
                    Ok(0) => return,
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    Err(_) => return,
                }
            }
            ParseOutcome::Error => {
                let resp = HttpResponse::new(
                    400,
                    ResponseBody::Owned(
                        b"{\"message\":\"Bad Request\",\"documentation_url\":\"https://example.invalid/docs\"}"
                            .to_vec(),
                    ),
                );
                let _ = write_response(&mut stream, &resp, false);
                return;
            }
        }
    }
}

pub fn listen() -> std::io::Result<(TcpListener, u16)> {
    let ln = TcpListener::bind("0.0.0.0:0")?;
    let port = ln.local_addr()?.port();
    Ok((ln, port))
}
