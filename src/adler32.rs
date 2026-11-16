//! Adler-32 (mod 65521), incremental. This is *our own content checksum*: a
//! site-independent integrity record stored in the manifest and re-checked on
//! restore/verify. Deliberately NOT claimed to equal XRootD's native adler32
//! (whose definition includes the file name).

const MOD: u32 = 65521;

#[derive(Default, Clone, Copy)]
pub struct Adler32 {
    a: u32,
    b: u32,
}

impl Adler32 {
    pub fn new() -> Self {
        Adler32 { a: 1, b: 0 }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            // b grows by a per byte; a < 65521 => safe chunk bound.
            let n = data.len().min(5552);
            for &byte in &data[..n] {
                self.a = (self.a + u32::from(byte)) % MOD;
                self.b = (self.b + self.a) % MOD;
            }
            data = &data[n..];
        }
    }

    pub fn finish(&self) -> u32 {
        (self.b << 16) | self.a
    }
}

/// Streaming file hash (256 KiB chunks).
pub fn hash_file(path: &std::path::Path) -> Result<u32, String> {
    use std::io::Read;
    let mut f =
        std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut h = Adler32::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| format!("read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values() {
        let h = Adler32::new();
        assert_eq!(h.finish(), 1); // empty
        let mut h = Adler32::new();
        h.update(b"Wikipedia");
        assert_eq!(h.finish(), 0x11E6_0398);
        let mut h2 = Adler32::new();
        h2.update(b"hello");
        assert_eq!(h2.finish(), 0x062C_0215); // zlib.adler32(b"hello")
    }

    #[test]
    fn chunking_invariance() {
        let data: Vec<u8> = (0..40000u32).map(|i| (i % 251) as u8).collect();
        let mut whole = Adler32::new();
        whole.update(&data);
        let mut piece = Adler32::new();
        for c in data.chunks(999) {
            piece.update(c);
        }
        assert_eq!(whole.finish(), piece.finish());
    }
}
