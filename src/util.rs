// Small shared helpers: base64 (Node Buffer semantics), JS Number(),
// ISO timestamps, integer formatting.

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn b64encode(data: &[u8]) -> String {
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let n: u32 = match chunk.len() {
            3 => ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8) | (chunk[2] as u32),
            2 => ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8),
            _ => (chunk[0] as u32) << 16,
        };
        out.push(B64[((n >> 18) & 63) as usize] as char);
        out.push(B64[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(B64[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Node Buffer.from(s, 'base64'): strips whitespace, stops at the first
/// invalid character, honours padding.
pub fn b64decode(s: &str) -> Vec<u8> {
    let mut rev = [-1i32; 256];
    for (i, &c) in B64.iter().enumerate() {
        rev[c as usize] = i as i32;
    }
    let clean: Vec<u8> = s
        .bytes()
        .filter(|&c| c != b' ' && c != b'\t' && c != b'\n' && c != b'\r')
        .collect();
    let mut out = Vec::with_capacity(clean.len() / 4 * 3);
    let mut i = 0;
    while i + 4 <= clean.len() {
        let mut n: u32 = 0;
        let mut pad = 0;
        for j in 0..4 {
            let c = clean[i + j];
            if c == b'=' {
                pad += 1;
                n <<= 6;
            } else {
                let v = rev[c as usize];
                if v < 0 {
                    return out;
                }
                n = (n << 6) | v as u32;
            }
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
        i += 4;
        if pad > 0 {
            break;
        }
    }
    out
}

pub fn itoa(n: i64) -> String {
    n.to_string()
}

pub fn is_oid(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase() || (b'0'..=b'9').contains(&c))
}

pub fn is_oid_bytes(b: &[u8]) -> bool {
    b.len() == 40
        && b.iter().all(|&c| (b'0'..=b'9').contains(&c) || (b'a'..=b'f').contains(&c))
}

/// Unix seconds -> "2006-01-02T15:04:05.000Z" (civil date, proleptic Gregorian).
pub fn iso_of(ts: i64) -> String {
    let days = ts.div_euclid(86400);
    let secs = ts.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.000Z",
        y,
        m,
        d,
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    )
}

/// Howard Hinnant's civil-from-days algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn is_space_byte(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

pub fn trim_space(s: &str) -> &str {
    let b = s.as_bytes();
    let mut start = 0;
    while start < b.len() && is_space_byte(b[start]) {
        start += 1;
    }
    let mut end = b.len();
    while end > start && is_space_byte(b[end - 1]) {
        end -= 1;
    }
    &s[start..end]
}

/// JavaScript Number() for the small numeric strings this forge sees:
/// trims space, '' is 0, hex 0x.. works, anything else is NaN (None).
pub fn js_number(s: &str) -> Option<f64> {
    let t = trim_space(s);
    if t.is_empty() {
        return Some(0.0);
    }
    if t.len() > 2 && t.as_bytes()[0] == b'0' && (t.as_bytes()[1] == b'x' || t.as_bytes()[1] == b'X') {
        let n = i64::from_str_radix(&t[2..], 16).ok()?;
        return Some(n as f64);
    }
    // Rust's parse accepts "inf"/"nan"/"1e5" etc.; JS Number() accepts the
    // same shapes for our inputs (plain decimals, exponents). Reject the
    // spellings JS rejects but Rust accepts.
    let lower = t.to_ascii_lowercase();
    if lower == "infinity" || lower == "+infinity" || lower == "-infinity" || lower == "nan" {
        // JS Number("Infinity") is Infinity (not NaN) — but the forge only
        // ever feeds this toIntNumber, which rejects non-finite anyway.
        return match lower.as_str() {
            "infinity" | "+infinity" => Some(f64::INFINITY),
            "-infinity" => Some(f64::NEG_INFINITY),
            _ => None,
        };
    }
    t.parse::<f64>().ok()
}

/// toIntNumber: a JS Number() result as an int map key, or None when it is
/// not an integer (or is out of range, which can never be found).
pub fn to_int_number(f: f64) -> Option<i64> {
    if !f.is_finite() || f.fract() != 0.0 {
        return None;
    }
    if f > 9_007_199_254_740_992.0 || f < -9_007_199_254_740_992.0 {
        return None;
    }
    Some(f as i64)
}

/// Percent-decode a path segment (like decodeURIComponent for a path).
/// Returns None on a bad escape.
pub fn path_unescape(s: &str) -> Option<String> {
    if !s.contains('%') {
        return Some(s.to_string());
    }
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'%' {
            out.push(b[i]);
            i += 1;
            continue;
        }
        if i + 2 >= b.len() {
            return None;
        }
        let hi = hex_val(b[i + 1])?;
        let lo = hex_val(b[i + 2])?;
        out.push((hi << 4) | lo);
        i += 3;
    }
    String::from_utf8(out).ok()
}

pub fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Query-string unescape: '+' is a space, %XX decoded (bad escapes stay literal).
pub fn query_unescape(s: &str) -> String {
    if !s.contains(['+', '%']) {
        return s.to_string();
    }
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'+' {
            out.push(b' ');
            i += 1;
        } else if c == b'%' && i + 2 < b.len() {
            if let (Some(hi), Some(lo)) = (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
            } else {
                out.push(c);
                i += 1;
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_dates() {
        assert_eq!(iso_of(1735689600), "2025-01-01T00:00:00.000Z");
        assert_eq!(iso_of(1735689601), "2025-01-01T00:00:01.000Z");
        assert_eq!(iso_of(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_of(1700000000), "2023-11-14T22:13:20.000Z");
    }

    #[test]
    fn b64_roundtrip() {
        for s in ["", "f", "fo", "foo", "foob", "fooba", "foobar", "Repository:3", "cursor:12"] {
            assert_eq!(String::from_utf8(b64decode(&b64encode(s.as_bytes()))).unwrap(), s);
        }
        assert_eq!(b64encode(b"Repository:3"), "UmVwb3NpdG9yeToz");
    }
}
