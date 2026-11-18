// SHA-1 (FIPS 180-1), written from the spec: the forge hashes every git
// object with "<type> <len>\0" + data, and every pack carries a SHA-1 trailer.

pub struct Sha1 {
    state: [u32; 5],
    len: u64,
    buf: [u8; 64],
    buflen: usize,
}

impl Sha1 {
    pub fn new() -> Self {
        Sha1 {
            state: [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0],
            len: 0,
            buf: [0; 64],
            buflen: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.len = self.len.wrapping_add(data.len() as u64);
        if self.buflen > 0 {
            let need = 64 - self.buflen;
            let take = need.min(data.len());
            self.buf[self.buflen..self.buflen + take].copy_from_slice(&data[..take]);
            self.buflen += take;
            data = &data[take..];
            if self.buflen == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buflen = 0;
            }
        }
        while data.len() >= 64 {
            let (block, rest) = data.split_at(64);
            let mut b = [0u8; 64];
            b.copy_from_slice(block);
            self.compress(&b);
            data = rest;
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buflen = data.len();
        }
    }

    pub fn finalize(mut self) -> [u8; 20] {
        let bitlen = self.len.wrapping_mul(8);
        self.update(&[0x80]);
        while self.buflen != 56 {
            self.update(&[0]);
        }
        // Feed the length directly (bypassing update's len accounting).
        let mut lenbuf = [0u8; 8];
        lenbuf.copy_from_slice(&bitlen.to_be_bytes());
        // Manually push the 8 length bytes into the block buffer.
        for &b in lenbuf.iter() {
            self.buf[self.buflen] = b;
            self.buflen += 1;
            if self.buflen == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buflen = 0;
            }
        }
        let mut out = [0u8; 20];
        for (i, w) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&w.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([block[i * 4], block[i * 4 + 1], block[i * 4 + 2], block[i * 4 + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) =
            (self.state[0], self.state[1], self.state[2], self.state[3], self.state[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
    }
}

/// SHA-1 of a single slice.
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h = Sha1::new();
    h.update(data);
    h.finalize()
}

/// Lowercase hex of a SHA-1 digest.
pub fn hex20(digest: &[u8; 20]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(40);
    for &b in digest.iter() {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

/// Git object id: SHA-1 of "<type> <len>\0" + data.
pub fn hash_object(typ: &str, data: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(typ.as_bytes());
    h.update(b" ");
    // Decimal length, no formatting machinery.
    let mut num = [0u8; 20];
    let mut n = data.len();
    let mut i = 20;
    if n == 0 {
        i -= 1;
        num[i] = b'0';
    } else {
        while n > 0 {
            i -= 1;
            num[i] = b'0' + (n % 10) as u8;
            n /= 10;
        }
    }
    h.update(&num[i..]);
    h.update(b"\0");
    h.update(data);
    hex20(&h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        assert_eq!(hex20(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(hex20(&sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
        // 1_000_000 x 'a' (multi-block, exact boundary crossings)
        let big = vec![b'a'; 1_000_000];
        assert_eq!(hex20(&sha1(&big)), "34aa973cd4c4daa4f61eeb2bdbad27316534016f");
        // Git blob id of "hello\n"
        assert_eq!(hash_object("blob", b"hello\n"), "ce013625030ba8dba906f756967f9e9ca394464a");
    }
}
