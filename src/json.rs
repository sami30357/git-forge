// JSON: a value type, a strict parser, a Go-compatible writer (no HTML
// escaping, \u00XX for other control bytes), and the fast flat-object path
// the hot POST bodies take (mirrors the Go forge's fastParseObject).

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    /// Last value for a key (Go map semantics: duplicate keys keep the last).
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(fields) => fields.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }
    /// Integer view: only when the number is integral and in range.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Num(n) if n.is_finite() && n.fract() == 0.0 && n.abs() < 9.007_199_254_740_992e15 => {
                Some(*n as i64)
            }
            _ => None,
        }
    }
    pub fn as_arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Append `s` as a JSON string literal (Go appendJSONString semantics).
pub fn push_json_string(out: &mut Vec<u8>, s: &str) {
    out.push(b'"');
    let bytes = s.as_bytes();
    let mut start = 0usize;
    for i in 0..bytes.len() {
        let c = bytes[i];
        if c == b'"' || c == b'\\' || c < 0x20 {
            out.extend_from_slice(&bytes[start..i]);
            match c {
                b'"' => out.extend_from_slice(b"\\\""),
                b'\\' => out.extend_from_slice(b"\\\\"),
                b'\n' => out.extend_from_slice(b"\\n"),
                b'\r' => out.extend_from_slice(b"\\r"),
                b'\t' => out.extend_from_slice(b"\\t"),
                0x08 => out.extend_from_slice(b"\\b"),
                0x0c => out.extend_from_slice(b"\\f"),
                _ => {
                    out.extend_from_slice(b"\\u00");
                    out.push(hex_digit(c >> 4));
                    out.push(hex_digit(c & 0xf));
                }
            }
            start = i + 1;
        }
    }
    out.extend_from_slice(&bytes[start..]);
    out.push(b'"');
}

fn hex_digit(v: u8) -> u8 {
    if v < 10 {
        b'0' + v
    } else {
        b'a' + v - 10
    }
}

/// Go strconv.FormatFloat(f, 'g', -1, 64) for the values this forge emits
/// (small integers and short decimals). Integral values print as integers.
pub fn push_json_float(out: &mut Vec<u8>, f: f64) {
    if f.is_nan() || f.is_infinite() {
        // encoding/json would error; the Go forge's writeJSONValue never sees
        // these from parsed input. Emit null rather than invalid JSON.
        out.extend_from_slice(b"null");
        return;
    }
    if f == f.trunc() && f.abs() < 1e15 {
        push_i64(out, f as i64);
        return;
    }
    // Shortest round-trip via Rust's Debug, then normalise the exponent to
    // Go's 'g' form (e+21, two exponent digits).
    let s = format!("{:?}", f);
    if let Some(epos) = s.find('e') {
        let (mant, exp) = s.split_at(epos);
        let exp: i32 = exp[1..].parse().unwrap_or(0);
        out.extend_from_slice(mant.as_bytes());
        out.push(b'e');
        if exp >= 0 {
            out.push(b'+');
        } else {
            out.push(b'-');
        }
        let a = exp.unsigned_abs();
        if a < 10 {
            out.push(b'0');
        }
        out.extend_from_slice(a.to_string().as_bytes());
    } else {
        out.extend_from_slice(s.as_bytes());
    }
}

pub fn push_i64(out: &mut Vec<u8>, n: i64) {
    out.extend_from_slice(n.to_string().as_bytes());
}

pub fn push_usize(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(n.to_string().as_bytes());
}

pub fn write_json(out: &mut Vec<u8>, v: &Json) {
    match v {
        Json::Null => out.extend_from_slice(b"null"),
        Json::Bool(true) => out.extend_from_slice(b"true"),
        Json::Bool(false) => out.extend_from_slice(b"false"),
        Json::Num(f) => push_json_float(out, *f),
        Json::Str(s) => push_json_string(out, s),
        Json::Arr(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_json(out, item);
            }
            out.push(b']');
        }
        Json::Obj(fields) => {
            out.push(b'{');
            for (i, (k, val)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                push_json_string(out, k);
                out.push(b':');
                write_json(out, val);
            }
            out.push(b'}');
        }
    }
}

pub fn to_json_bytes(v: &Json) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    write_json(&mut out, v);
    out
}

// ---------------------------------------------------------------------------
// Parser (strict JSON, encoding/json-compatible for our inputs)
// ---------------------------------------------------------------------------

pub struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Parser { b, i: 0 }
    }

    pub fn parse_value(&mut self) -> Result<Json, ()> {
        self.skip_ws();
        if self.i >= self.b.len() {
            return Err(());
        }
        match self.b[self.i] {
            b'{' => self.parse_object(),
            b'[' => self.parse_array(),
            b'"' => Ok(Json::Str(self.parse_string()?)),
            b't' => self.lit("true", Json::Bool(true)),
            b'f' => self.lit("false", Json::Bool(false)),
            b'n' => self.lit("null", Json::Null),
            b'-' | b'0'..=b'9' => self.parse_number(),
            _ => Err(()),
        }
    }

    /// Parse one value and require end-of-input (after whitespace).
    pub fn parse_document(&mut self) -> Result<Json, ()> {
        let v = self.parse_value()?;
        self.skip_ws();
        if self.i != self.b.len() {
            return Err(());
        }
        Ok(v)
    }

    fn skip_ws(&mut self) {
        while self.i < self.b.len() {
            match self.b[self.i] {
                b' ' | b'\t' | b'\n' | b'\r' => self.i += 1,
                _ => break,
            }
        }
    }

    fn lit(&mut self, s: &str, v: Json) -> Result<Json, ()> {
        if self.b.len() - self.i >= s.len() && &self.b[self.i..self.i + s.len()] == s.as_bytes() {
            self.i += s.len();
            Ok(v)
        } else {
            Err(())
        }
    }

    fn parse_string(&mut self) -> Result<String, ()> {
        // Caller guarantees b[i] == '"'.
        self.i += 1;
        let mut out: Vec<u8> = Vec::new();
        let mut start = self.i;
        loop {
            if self.i >= self.b.len() {
                return Err(());
            }
            let c = self.b[self.i];
            match c {
                b'"' => {
                    out.extend_from_slice(&self.b[start..self.i]);
                    self.i += 1;
                    return String::from_utf8(out).map_err(|_| ());
                }
                b'\\' => {
                    out.extend_from_slice(&self.b[start..self.i]);
                    self.i += 1;
                    if self.i >= self.b.len() {
                        return Err(());
                    }
                    let esc = self.b[self.i];
                    self.i += 1;
                    match esc {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let cp = self.parse_hex4()?;
                            if (0xD800..0xDC00).contains(&cp) {
                                // Surrogate pair.
                                if self.i + 1 < self.b.len()
                                    && self.b[self.i] == b'\\'
                                    && self.b[self.i + 1] == b'u'
                                {
                                    self.i += 2;
                                    let lo = self.parse_hex4()?;
                                    if (0xDC00..0xE000).contains(&lo) {
                                        let c =
                                            0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                        push_utf8(&mut out, c);
                                    } else {
                                        push_utf8(&mut out, 0xFFFD);
                                        // The low half was consumed; keep going.
                                        let _ = lo;
                                    }
                                } else {
                                    push_utf8(&mut out, 0xFFFD);
                                }
                            } else if (0xDC00..0xE000).contains(&cp) {
                                push_utf8(&mut out, 0xFFFD);
                            } else {
                                push_utf8(&mut out, cp);
                            }
                        }
                        _ => return Err(()),
                    }
                    start = self.i;
                }
                0x00..=0x1f => return Err(()),
                _ => {
                    self.i += 1;
                }
            }
        }
    }

    fn parse_hex4(&mut self) -> Result<u32, ()> {
        if self.i + 4 > self.b.len() {
            return Err(());
        }
        let mut n: u32 = 0;
        for _ in 0..4 {
            let c = self.b[self.i];
            let v = match c {
                b'0'..=b'9' => (c - b'0') as u32,
                b'a'..=b'f' => (c - b'a' + 10) as u32,
                b'A'..=b'F' => (c - b'A' + 10) as u32,
                _ => return Err(()),
            };
            n = (n << 4) | v;
            self.i += 1;
        }
        Ok(n)
    }

    fn parse_number(&mut self) -> Result<Json, ()> {
        let start = self.i;
        if self.i < self.b.len() && self.b[self.i] == b'-' {
            self.i += 1;
        }
        if self.i >= self.b.len() {
            return Err(());
        }
        if self.b[self.i] == b'0' {
            self.i += 1;
        } else if self.b[self.i].is_ascii_digit() {
            while self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
                self.i += 1;
            }
        } else {
            return Err(());
        }
        if self.i < self.b.len() && self.b[self.i] == b'.' {
            self.i += 1;
            if self.i >= self.b.len() || !self.b[self.i].is_ascii_digit() {
                return Err(());
            }
            while self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
                self.i += 1;
            }
        }
        if self.i < self.b.len() && (self.b[self.i] == b'e' || self.b[self.i] == b'E') {
            self.i += 1;
            if self.i < self.b.len() && (self.b[self.i] == b'+' || self.b[self.i] == b'-') {
                self.i += 1;
            }
            if self.i >= self.b.len() || !self.b[self.i].is_ascii_digit() {
                return Err(());
            }
            while self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
                self.i += 1;
            }
        }
        let text = std::str::from_utf8(&self.b[start..self.i]).map_err(|_| ())?;
        text.parse::<f64>().map(Json::Num).map_err(|_| ())
    }

    fn parse_array(&mut self) -> Result<Json, ()> {
        self.i += 1; // '['
        let mut out = Vec::new();
        self.skip_ws();
        if self.i < self.b.len() && self.b[self.i] == b']' {
            self.i += 1;
            return Ok(Json::Arr(out));
        }
        loop {
            let v = self.parse_value()?;
            out.push(v);
            self.skip_ws();
            if self.i >= self.b.len() {
                return Err(());
            }
            match self.b[self.i] {
                b',' => {
                    self.i += 1;
                }
                b']' => {
                    self.i += 1;
                    return Ok(Json::Arr(out));
                }
                _ => return Err(()),
            }
        }
    }

    fn parse_object(&mut self) -> Result<Json, ()> {
        self.i += 1; // '{'
        let mut out: Vec<(String, Json)> = Vec::new();
        self.skip_ws();
        if self.i < self.b.len() && self.b[self.i] == b'}' {
            self.i += 1;
            return Ok(Json::Obj(out));
        }
        loop {
            self.skip_ws();
            if self.i >= self.b.len() || self.b[self.i] != b'"' {
                return Err(());
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.i >= self.b.len() || self.b[self.i] != b':' {
                return Err(());
            }
            self.i += 1;
            let val = self.parse_value()?;
            // Duplicate keys: keep the last (replace in place to preserve
            // first-seen order, like Go's map iteration is unordered anyway;
            // lookups take the last).
            if let Some(slot) = out.iter_mut().find(|(k, _)| *k == key) {
                slot.1 = val;
            } else {
                out.push((key, val));
            }
            self.skip_ws();
            if self.i >= self.b.len() {
                return Err(());
            }
            match self.b[self.i] {
                b',' => {
                    self.i += 1;
                }
                b'}' => {
                    self.i += 1;
                    return Ok(Json::Obj(out));
                }
                _ => return Err(()),
            }
        }
    }
}

fn push_utf8(out: &mut Vec<u8>, cp: u32) {
    if cp < 0x80 {
        out.push(cp as u8);
    } else if cp < 0x800 {
        out.push(0xC0 | (cp >> 6) as u8);
        out.push(0x80 | (cp & 0x3F) as u8);
    } else if cp < 0x10000 {
        out.push(0xE0 | (cp >> 12) as u8);
        out.push(0x80 | ((cp >> 6) & 0x3F) as u8);
        out.push(0x80 | (cp & 0x3F) as u8);
    } else {
        out.push(0xF0 | (cp >> 18) as u8);
        out.push(0x80 | ((cp >> 12) & 0x3F) as u8);
        out.push(0x80 | ((cp >> 6) & 0x3F) as u8);
        out.push(0x80 | (cp & 0x3F) as u8);
    }
}

// ---------------------------------------------------------------------------
// Body parsing (parseJSONBody semantics)
// ---------------------------------------------------------------------------

/// Mirrors the Go forge's parseJSONBody: empty body is {}, malformed JSON
/// fails, a well-formed non-object JSON value is silently {}.
pub fn parse_body_object(body: &[u8]) -> Option<Vec<(String, Json)>> {
    if body.is_empty() {
        return Some(Vec::new());
    }
    if let Some(fields) = fast_parse_object(body) {
        return Some(fields);
    }
    match Parser::new(body).parse_document() {
        Ok(Json::Obj(fields)) => Some(fields),
        Ok(_) => Some(Vec::new()),
        Err(()) => None,
    }
}

/// Fast path for flat objects: string/bool/null/number values and string
/// arrays only; \u escapes or nested objects bail to the full parser.
fn fast_parse_object(body: &[u8]) -> Option<Vec<(String, Json)>> {
    let mut p = Parser { b: body, i: 0 };
    p.skip_ws();
    if p.i >= p.b.len() || p.b[p.i] != b'{' {
        return None;
    }
    p.i += 1;
    let mut out: Vec<(String, Json)> = Vec::new();
    p.skip_ws();
    if p.i < p.b.len() && p.b[p.i] == b'}' {
        p.i += 1;
        p.skip_ws();
        if p.i == p.b.len() {
            return Some(out);
        }
        return None;
    }
    loop {
        p.skip_ws();
        let key = fast_string(&mut p)?;
        p.skip_ws();
        if p.i >= p.b.len() || p.b[p.i] != b':' {
            return None;
        }
        p.i += 1;
        p.skip_ws();
        let val = fast_value(&mut p)?;
        // Duplicate keys keep the last.
        if let Some(slot) = out.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = val;
        } else {
            out.push((key, val));
        }
        p.skip_ws();
        if p.i >= p.b.len() {
            return None;
        }
        match p.b[p.i] {
            b',' => p.i += 1,
            b'}' => {
                p.i += 1;
                p.skip_ws();
                if p.i == p.b.len() {
                    return Some(out);
                }
                return None;
            }
            _ => return None,
        }
    }
}

/// A string without \u escapes or raw control bytes; None bails to full parse.
fn fast_string(p: &mut Parser) -> Option<String> {
    if p.i >= p.b.len() || p.b[p.i] != b'"' {
        return None;
    }
    p.i += 1;
    let mut out: Vec<u8> = Vec::new();
    let mut start = p.i;
    loop {
        if p.i >= p.b.len() {
            return None;
        }
        let c = p.b[p.i];
        match c {
            b'"' => {
                out.extend_from_slice(&p.b[start..p.i]);
                p.i += 1;
                return String::from_utf8(out).ok();
            }
            b'\\' => {
                out.extend_from_slice(&p.b[start..p.i]);
                p.i += 1;
                if p.i >= p.b.len() {
                    return None;
                }
                let esc = p.b[p.i];
                p.i += 1;
                match esc {
                    b'"' => out.push(b'"'),
                    b'\\' => out.push(b'\\'),
                    b'/' => out.push(b'/'),
                    b'b' => out.push(0x08),
                    b'f' => out.push(0x0c),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    _ => return None, // \u and anything else -> full parser
                }
                start = p.i;
            }
            0x00..=0x1f => return None,
            _ => p.i += 1,
        }
    }
}

fn fast_value(p: &mut Parser) -> Option<Json> {
    if p.i >= p.b.len() {
        return None;
    }
    match p.b[p.i] {
        b'"' => fast_string(p).map(Json::Str),
        b't' => p.lit("true", Json::Bool(true)).ok(),
        b'f' => p.lit("false", Json::Bool(false)).ok(),
        b'n' => p.lit("null", Json::Null).ok(),
        b'[' => {
            p.i += 1;
            let mut out = Vec::new();
            p.skip_ws();
            if p.i < p.b.len() && p.b[p.i] == b']' {
                p.i += 1;
                return Some(Json::Arr(out));
            }
            loop {
                p.skip_ws();
                out.push(Json::Str(fast_string(p)?));
                p.skip_ws();
                if p.i >= p.b.len() {
                    return None;
                }
                match p.b[p.i] {
                    b',' => p.i += 1,
                    b']' => {
                        p.i += 1;
                        return Some(Json::Arr(out));
                    }
                    _ => return None,
                }
            }
        }
        b'-' | b'0'..=b'9' => p.parse_number().ok(),
        _ => None,
    }
}

/// Field lookup on a parsed body (last duplicate wins).
pub fn body_get<'a>(fields: &'a [(String, Json)], key: &str) -> Option<&'a Json> {
    fields.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
}

pub fn body_string(fields: &[(String, Json)], key: &str) -> String {
    match body_get(fields, key) {
        Some(Json::Str(s)) => s.clone(),
        _ => String::new(),
    }
}

pub fn body_bool(fields: &[(String, Json)], key: &str) -> bool {
    matches!(body_get(fields, key), Some(Json::Bool(true)))
}

pub fn body_string_list(fields: &[(String, Json)], key: &str) -> Option<Vec<String>> {
    match body_get(fields, key) {
        Some(Json::Arr(items)) => {
            let mut out = Vec::new();
            for item in items {
                if let Json::Str(s) = item {
                    out.push(s.clone());
                }
            }
            Some(out)
        }
        _ => None,
    }
}
