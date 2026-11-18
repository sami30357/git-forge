// DEFLATE (RFC 1951) + zlib (RFC 1950), std-only.
//   inflate: stored / fixed / dynamic Huffman, with Adler-32 verification.
//   deflate: fixed-Huffman literals-only — valid for every inflater, fast to
//   emit, and the packs this forge writes are ~1 KB, so compression ratio
//   does not matter (the wire is loopback).

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FlateError {
    Truncated,
    BadHeader,
    BadBlock,
    BadCode,
    BadDistance,
    BadLength,
    AdlerMismatch,
    TooBig,
}

// ---------------------------------------------------------------------------
// Adler-32
// ---------------------------------------------------------------------------

pub fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += x as u32;
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    (b << 16) | a
}

// ---------------------------------------------------------------------------
// Bit reader (LSB-first, as DEFLATE packs bits)
// ---------------------------------------------------------------------------

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bitbuf: u32,
    bitcnt: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0, bitbuf: 0, bitcnt: 0 }
    }

    fn bits(&mut self, n: u32) -> Result<u32, FlateError> {
        while self.bitcnt < n {
            if self.pos >= self.data.len() {
                return Err(FlateError::Truncated);
            }
            self.bitbuf |= (self.data[self.pos] as u32) << self.bitcnt;
            self.pos += 1;
            self.bitcnt += 8;
        }
        let v = self.bitbuf & ((1u32 << n) - 1);
        self.bitbuf >>= n;
        self.bitcnt -= n;
        Ok(v)
    }

    fn align_byte(&mut self) {
        let drop = self.bitcnt % 8;
        self.bitbuf >>= drop;
        self.bitcnt -= drop;
    }

    /// Bytes consumed from the input (whole bytes only).
    fn byte_pos(&self) -> usize {
        self.pos - (self.bitcnt as usize / 8)
    }
}

// ---------------------------------------------------------------------------
// Huffman decoding (canonical codes, puff-style)
// ---------------------------------------------------------------------------

struct Huffman {
    counts: [u16; 16], // counts[len] = number of codes of that length
    symbols: Vec<u16>, // symbols sorted by (length, symbol)
}

impl Huffman {
    fn build(lengths: &[u8]) -> Result<Huffman, FlateError> {
        let mut counts = [0u16; 16];
        for &len in lengths {
            if len > 15 {
                return Err(FlateError::BadCode);
            }
            counts[len as usize] += 1;
        }
        counts[0] = 0;
        // Oversubscribed check (incomplete codes are tolerated: some
        // encoders emit a degenerate distance table).
        let mut left: i32 = 1;
        for len in 1..16 {
            left <<= 1;
            left -= counts[len] as i32;
            if left < 0 {
                return Err(FlateError::BadCode);
            }
        }
        let mut offsets = [0u16; 16];
        for len in 1..15 {
            offsets[len + 1] = offsets[len] + counts[len];
        }
        let total: usize = counts.iter().map(|&c| c as usize).sum();
        let mut symbols = vec![0u16; total];
        for (sym, &len) in lengths.iter().enumerate() {
            if len != 0 {
                symbols[offsets[len as usize] as usize] = sym as u16;
                offsets[len as usize] += 1;
            }
        }
        Ok(Huffman { counts, symbols })
    }

    fn decode(&self, br: &mut BitReader) -> Result<u16, FlateError> {
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for len in 1..16 {
            code |= br.bits(1)? as i32;
            let count = self.counts[len] as i32;
            if code - count < first {
                return Ok(self.symbols[(index + (code - first)) as usize]);
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err(FlateError::BadCode)
    }
}

fn fixed_literal() -> Huffman {
    let mut lengths = [0u8; 288];
    for i in 0..144 {
        lengths[i] = 8;
    }
    for i in 144..256 {
        lengths[i] = 9;
    }
    for i in 256..280 {
        lengths[i] = 7;
    }
    for i in 280..288 {
        lengths[i] = 8;
    }
    Huffman::build(&lengths).expect("fixed literal table is valid")
}

fn fixed_distance() -> Huffman {
    let lengths = [5u8; 30];
    Huffman::build(&lengths).expect("fixed distance table is valid")
}

// Length code bases / extra bits (RFC 1951 §3.2.5).
const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u32; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u32; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
// Code-length code order (RFC 1951 §3.2.7).
const CL_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

/// Inflate one raw DEFLATE stream. `out` is filled exactly (the caller knows
/// the expected size); returns bytes consumed from `data`.
fn inflate_raw(data: &[u8], out: &mut [u8]) -> Result<usize, FlateError> {
    let mut br = BitReader::new(data);
    let mut written = 0usize;
    loop {
        let bfinal = br.bits(1)?;
        let btype = br.bits(2)?;
        match btype {
            0 => {
                // Stored block.
                br.align_byte();
                if br.byte_pos() + 4 > data.len() {
                    return Err(FlateError::Truncated);
                }
                let p = br.byte_pos();
                let len = u16::from_le_bytes([data[p], data[p + 1]]) as usize;
                let nlen = u16::from_le_bytes([data[p + 2], data[p + 3]]) as usize;
                if len != (!nlen & 0xffff) {
                    return Err(FlateError::BadBlock);
                }
                if p + 4 + len > data.len() || written + len > out.len() {
                    return Err(FlateError::Truncated);
                }
                out[written..written + len].copy_from_slice(&data[p + 4..p + 4 + len]);
                written += len;
                br.pos = p + 4 + len;
                br.bitbuf = 0;
                br.bitcnt = 0;
            }
            1 => {
                let lit = fixed_literal();
                let dist = fixed_distance();
                inflate_block(&mut br, &lit, &dist, out, &mut written)?;
            }
            2 => {
                let hlit = br.bits(5)? as usize + 257;
                let hdist = br.bits(5)? as usize + 1;
                let hclen = br.bits(4)? as usize + 4;
                if hlit > 286 || hdist > 30 {
                    return Err(FlateError::BadBlock);
                }
                let mut cl_lengths = [0u8; 19];
                for i in 0..hclen {
                    cl_lengths[CL_ORDER[i]] = br.bits(3)? as u8;
                }
                let cl = Huffman::build(&cl_lengths)?;
                let mut lengths = vec![0u8; hlit + hdist];
                let mut i = 0;
                while i < hlit + hdist {
                    let sym = cl.decode(&mut br)?;
                    match sym {
                        0..=15 => {
                            lengths[i] = sym as u8;
                            i += 1;
                        }
                        16 => {
                            if i == 0 {
                                return Err(FlateError::BadCode);
                            }
                            let prev = lengths[i - 1];
                            let rep = 3 + br.bits(2)? as usize;
                            if i + rep > hlit + hdist {
                                return Err(FlateError::BadCode);
                            }
                            for _ in 0..rep {
                                lengths[i] = prev;
                                i += 1;
                            }
                        }
                        17 => {
                            let rep = 3 + br.bits(3)? as usize;
                            if i + rep > hlit + hdist {
                                return Err(FlateError::BadCode);
                            }
                            i += rep;
                        }
                        18 => {
                            let rep = 11 + br.bits(7)? as usize;
                            if i + rep > hlit + hdist {
                                return Err(FlateError::BadCode);
                            }
                            i += rep;
                        }
                        _ => return Err(FlateError::BadCode),
                    }
                }
                let lit = Huffman::build(&lengths[..hlit])?;
                let dist = Huffman::build(&lengths[hlit..])?;
                inflate_block(&mut br, &lit, &dist, out, &mut written)?;
            }
            _ => return Err(FlateError::BadBlock),
        }
        if bfinal == 1 {
            break;
        }
    }
    if written != out.len() {
        return Err(FlateError::BadLength);
    }
    Ok(br.byte_pos())
}

fn inflate_block(
    br: &mut BitReader,
    lit: &Huffman,
    dist: &Huffman,
    out: &mut [u8],
    written: &mut usize,
) -> Result<(), FlateError> {
    loop {
        let sym = lit.decode(br)?;
        match sym {
            0..=255 => {
                if *written >= out.len() {
                    return Err(FlateError::TooBig);
                }
                out[*written] = sym as u8;
                *written += 1;
            }
            256 => return Ok(()),
            257..=285 => {
                let idx = (sym - 257) as usize;
                let len = LEN_BASE[idx] as usize + br.bits(LEN_EXTRA[idx])? as usize;
                let dsym = dist.decode(br)? as usize;
                if dsym >= 30 {
                    return Err(FlateError::BadDistance);
                }
                let d = DIST_BASE[dsym] as usize + br.bits(DIST_EXTRA[dsym])? as usize;
                if d > *written || *written + len > out.len() {
                    return Err(FlateError::BadDistance);
                }
                // Overlapping copy (the common case for repeated text).
                let mut src = *written - d;
                for _ in 0..len {
                    let b = out[src];
                    out[*written] = b;
                    *written += 1;
                    src += 1;
                }
            }
            _ => return Err(FlateError::BadCode),
        }
    }
}

/// Inflate one zlib stream starting at `data[0]`. Returns the inflated bytes
/// and the total bytes consumed (header + deflate + Adler trailer).
pub fn zlib_inflate(data: &[u8], expected_size: usize) -> Result<(Vec<u8>, usize), FlateError> {
    if data.len() < 2 {
        return Err(FlateError::Truncated);
    }
    let cmf = data[0];
    let flg = data[1];
    if cmf & 0x0f != 8 || ((cmf as u32 * 256 + flg as u32) % 31) != 0 || flg & 0x20 != 0 {
        return Err(FlateError::BadHeader);
    }
    let mut out = vec![0u8; expected_size];
    let used = inflate_raw(&data[2..], &mut out)?;
    let trailer = 2 + used;
    if trailer + 4 > data.len() {
        return Err(FlateError::Truncated);
    }
    let want = u32::from_be_bytes([data[trailer], data[trailer + 1], data[trailer + 2], data[trailer + 3]]);
    if adler32(&out) != want {
        return Err(FlateError::AdlerMismatch);
    }
    Ok((out, trailer + 4))
}

// ---------------------------------------------------------------------------
// Deflate (fixed Huffman, literals only)
// ---------------------------------------------------------------------------

struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    nbits: u32,
}

impl BitWriter {
    fn new() -> Self {
        BitWriter { out: Vec::new(), acc: 0, nbits: 0 }
    }

    fn bit(&mut self, b: u32) {
        self.acc |= (b & 1) << self.nbits;
        self.nbits += 1;
        if self.nbits == 8 {
            self.out.push(self.acc as u8);
            self.acc = 0;
            self.nbits = 0;
        }
    }

    /// Emit a Huffman code, most-significant bit first (DEFLATE order).
    fn code(&mut self, code: u32, len: u32) {
        for i in (0..len).rev() {
            self.bit((code >> i) & 1);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.nbits > 0 {
            self.out.push(self.acc as u8);
            self.acc = 0;
            self.nbits = 0;
        }
        self.out
    }
}

fn fixed_lit_code(byte: u8) -> (u32, u32) {
    match byte {
        0..=143 => (0x30 + byte as u32, 8),
        _ => (0x190 + (byte as u32 - 144), 9),
    }
}

/// One zlib stream: fixed-Huffman block with literal bytes only.
pub fn zlib_deflate(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 8);
    out.push(0x78);
    out.push(0x01);
    let mut bw = BitWriter::new();
    bw.bit(1); // BFINAL
    // BTYPE is a data element: packed LSB-first, value 01 (fixed Huffman).
    bw.bit(1);
    bw.bit(0);
    for &b in data {
        let (code, len) = fixed_lit_code(b);
        bw.code(code, len);
    }
    bw.code(0, 7); // end of block (256)
    out.extend_from_slice(&bw.finish());
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(data: &[u8]) {
        let enc = zlib_deflate(data);
        let (dec, used) = zlib_inflate(&enc, data.len()).expect("inflate our own deflate");
        assert_eq!(dec, data);
        assert_eq!(used, enc.len());
    }

    #[test]
    fn deflate_roundtrips() {
        roundtrip(b"");
        roundtrip(b"hello world\n");
        roundtrip(b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        roundtrip(&vec![0u8; 1000]);
        let mut v = Vec::new();
        for i in 0..10000 {
            v.push((i * 37 % 251) as u8);
        }
        roundtrip(&v);
    }

    #[test]
    fn inflate_stored_block() {
        // A stored-block zlib stream for "abc", built by hand:
        // header 78 01, block header byte 0x01 (BFINAL=1, BTYPE=00),
        // LEN 3, NLEN ~3, data, Adler-32("abc") = 0x024d0127.
        let stored: [u8; 14] = [
            0x78, 0x01, 0x01, 0x03, 0x00, 0xfc, 0xff, b'a', b'b', b'c', 0x02, 0x4d, 0x01,
            0x27,
        ];
        let (dec, used) = zlib_inflate(&stored, 3).expect("stored block inflates");
        assert_eq!(dec, b"abc");
        assert_eq!(used, stored.len());
    }

    #[test]
    fn inflate_matches() {
        let data = b"abcabcabcabcabcabcabcabc";
        roundtrip(data);
    }
}
