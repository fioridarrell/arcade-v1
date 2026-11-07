//! Key building. Integers are big-endian so keys sort numerically.

#[derive(Default, Clone)]
pub struct KeyBuf(Vec<u8>);

impl KeyBuf {
    pub fn new() -> Self {
        KeyBuf(Vec::with_capacity(48))
    }
    pub fn u8(mut self, v: u8) -> Self {
        self.0.push(v);
        self
    }
    pub fn u16(mut self, v: u16) -> Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn u32(mut self, v: u32) -> Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn u64(mut self, v: u64) -> Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn bytes(mut self, b: &[u8]) -> Self {
        self.0.extend_from_slice(b);
        self
    }
    /// Length-prefixed bytes (1-byte length), so variable fields don't run into the next.
    pub fn var(mut self, b: &[u8]) -> Self {
        assert!(b.len() <= 255, "key field too long");
        self.0.push(b.len() as u8);
        self.0.extend_from_slice(b);
        self
    }
    pub fn done(self) -> Vec<u8> {
        self.0
    }
}

/// Read big-endian integers back out of a key or value.
pub fn be_u16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes(b[at..at + 2].try_into().unwrap())
}
pub fn be_u32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}
pub fn be_u64(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(b[at..at + 8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn big_endian_sorts_numerically() {
        let a = KeyBuf::new().u32(255).done();
        let b = KeyBuf::new().u32(256).done();
        assert!(a < b);
        assert_eq!(be_u32(&b, 0), 256);
    }
}
