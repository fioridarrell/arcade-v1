//! Wire decoding: a cursor over bytes with Bitcoin-style varints.

use crate::hash::Hash256;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DecodeError {
    #[error("unexpected end of data at byte {0}")]
    Eof(usize),
    #[error("length {0} too large")]
    TooLarge(u64),
    #[error("non-canonical varint")]
    NonCanonical,
    #[error("{0}")]
    Invalid(&'static str),
}

/// Hard cap on any single length prefix, so corrupt input cannot make us allocate gigabytes.
pub const MAX_LEN: u64 = 32 * 1024 * 1024;

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }
    pub fn pos(&self) -> usize {
        self.pos
    }
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }
    /// Bytes from `start` to the current position.
    pub fn since(&self, start: usize) -> &'a [u8] {
        &self.buf[start..self.pos]
    }
    pub fn peek(&self, n: usize) -> Result<&'a [u8], DecodeError> {
        self.buf
            .get(self.pos..self.pos + n)
            .ok_or(DecodeError::Eof(self.pos))
    }
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let s = self.peek(n)?;
        self.pos += n;
        Ok(s)
    }
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let mut a = [0u8; N];
        a.copy_from_slice(self.bytes(N)?);
        Ok(a)
    }
    pub fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.bytes(1)?[0])
    }
    pub fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.array()?))
    }
    pub fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    pub fn i32(&mut self) -> Result<i32, DecodeError> {
        Ok(i32::from_le_bytes(self.array()?))
    }
    pub fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.array()?))
    }
    pub fn i64(&mut self) -> Result<i64, DecodeError> {
        Ok(i64::from_le_bytes(self.array()?))
    }
    pub fn hash(&mut self) -> Result<Hash256, DecodeError> {
        Ok(Hash256(self.array()?))
    }
    pub fn varint(&mut self) -> Result<u64, DecodeError> {
        let v = match self.u8()? {
            0xfd => {
                let v = self.u16()? as u64;
                if v < 0xfd {
                    return Err(DecodeError::NonCanonical);
                }
                v
            }
            0xfe => {
                let v = self.u32()? as u64;
                if v <= 0xffff {
                    return Err(DecodeError::NonCanonical);
                }
                v
            }
            0xff => {
                let v = self.u64()?;
                if v <= 0xffff_ffff {
                    return Err(DecodeError::NonCanonical);
                }
                v
            }
            b => b as u64,
        };
        Ok(v)
    }
    /// A varint used as a length or count, bounded by MAX_LEN.
    pub fn len(&mut self) -> Result<usize, DecodeError> {
        let n = self.varint()?;
        if n > MAX_LEN {
            return Err(DecodeError::TooLarge(n));
        }
        Ok(n as usize)
    }
    pub fn var_bytes(&mut self) -> Result<&'a [u8], DecodeError> {
        let n = self.len()?;
        self.bytes(n)
    }
}

pub fn write_varint(out: &mut Vec<u8>, n: u64) {
    match n {
        0..=0xfc => out.push(n as u8),
        0xfd..=0xffff => {
            out.push(0xfd);
            out.extend_from_slice(&(n as u16).to_le_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(0xfe);
            out.extend_from_slice(&(n as u32).to_le_bytes());
        }
        _ => {
            out.push(0xff);
            out.extend_from_slice(&n.to_le_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip() {
        for n in [0u64, 1, 0xfc, 0xfd, 0xffff, 0x10000, 0xffff_ffff, 0x1_0000_0000] {
            let mut v = Vec::new();
            write_varint(&mut v, n);
            assert_eq!(Reader::new(&v).varint().unwrap(), n);
        }
    }

    #[test]
    fn varint_rejects_noncanonical() {
        assert_eq!(
            Reader::new(&[0xfd, 0x10, 0x00]).varint(),
            Err(DecodeError::NonCanonical)
        );
    }

    #[test]
    fn eof_is_error() {
        assert!(Reader::new(&[1, 2]).u32().is_err());
    }
}
