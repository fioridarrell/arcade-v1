//! Omni-style payload decoding, as payload.py (docs/compat/LEDGER.md §2).
//! Big-endian; no version check; trailing bytes ignored; cstr to NUL or end (utf-8 "replace").

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    SimpleSend { prop: u32, amount: u64 },
    SendAll { eco: u8 },
    Trade { sale: u32, sale_amount: u64, want: u32, want_amount: u64 },
    CancelPrice { sale: u32, sale_amount: u64, want: u32, want_amount: u64 },
    CancelPair { sale: u32, want: u32 },
    CancelEcosystem { eco: u8 },
    Take { prop: u32, amount: u64, order: [u8; 32] },
    Issuance { managed: bool, eco: u8, ptype: u16, prev: u32, category: String, subcategory: String, name: String, url: String, data: String, amount: u64 },
    Grant { prop: u32, amount: u64 },
    Revoke { prop: u32, amount: u64 },
    ChangeIssuer { prop: u32 },
    Activate { feature: u16, block: u32, minver: u32 },
    Deactivate { feature: u16 },
    Alert,
    AnyData(Vec<u8>),
    /// Decodes, but no handler yet (3, 5, 20, 22, 71-74, 185, 186, 201).
    NotImplemented,
}

#[derive(Debug, Clone)]
pub struct Decoded {
    pub version: u16,
    pub mtype: u16,
    pub msg: Msg,
}

/// Errors that make the existing arcade halt its indexer (PayloadError, UnknownMessageType,
/// OutOfScopeMessageType). V1.0 records nothing for such a tx and reports it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Halt(pub String);

pub const OUT_OF_SCOPE: &[u16] = &[2, 10, 11, 12, 15, 31, 40, 51, 52, 53];

struct R<'a> {
    b: &'a [u8],
    i: usize,
}

impl R<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], Halt> {
        let s = self.b.get(self.i..self.i + n).ok_or_else(|| Halt("truncated payload".into()))?;
        self.i += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, Halt> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, Halt> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, Halt> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, Halt> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn cstr(&mut self) -> String {
        let rest = &self.b[self.i.min(self.b.len())..];
        let end = rest.iter().position(|&c| c == 0).unwrap_or(rest.len());
        let s = String::from_utf8_lossy(&rest[..end]).into_owned();
        self.i += end + if end < rest.len() { 1 } else { 0 };
        s
    }
}

pub fn decode(p: &[u8]) -> Result<Decoded, Halt> {
    if p.len() < 4 {
        return Err(Halt("payload shorter than its header".into()));
    }
    let mut r = R { b: p, i: 0 };
    let version = r.u16()?;
    let mtype = r.u16()?;
    let msg = match mtype {
        0 => Msg::SimpleSend { prop: r.u32()?, amount: r.u64()? },
        4 => Msg::SendAll { eco: r.u8()? },
        25 | 26 => {
            let (sale, sale_amount, want, want_amount) = (r.u32()?, r.u64()?, r.u32()?, r.u64()?);
            if mtype == 25 {
                Msg::Trade { sale, sale_amount, want, want_amount }
            } else {
                Msg::CancelPrice { sale, sale_amount, want, want_amount }
            }
        }
        27 => Msg::CancelPair { sale: r.u32()?, want: r.u32()? },
        28 => Msg::CancelEcosystem { eco: r.u8()? },
        29 => Msg::Take { prop: r.u32()?, amount: r.u64()?, order: r.take(32)?.try_into().unwrap() },
        50 | 54 => {
            let (eco, ptype, prev) = (r.u8()?, r.u16()?, r.u32()?);
            let (category, subcategory, name, url, data) = (r.cstr(), r.cstr(), r.cstr(), r.cstr(), r.cstr());
            let amount = if mtype == 50 { r.u64()? } else { 0 };
            Msg::Issuance { managed: mtype == 54, eco, ptype, prev, category, subcategory, name, url, data, amount }
        }
        55 => Msg::Grant { prop: r.u32()?, amount: r.u64()? },
        56 => Msg::Revoke { prop: r.u32()?, amount: r.u64()? },
        70 => Msg::ChangeIssuer { prop: r.u32()? },
        65534 => Msg::Activate { feature: r.u16()?, block: r.u32()?, minver: r.u32()? },
        65533 => Msg::Deactivate { feature: r.u16()? },
        65535 => Msg::Alert,
        200 => Msg::AnyData(p[4..].to_vec()),
        3 => {
            r.u32()?;
            r.u64()?;
            if version >= 1 {
                r.u32()?;
            }
            Msg::NotImplemented
        }
        5 => {
            r.u32()?;
            r.u64()?;
            r.u64()?;
            Msg::NotImplemented
        }
        22 => {
            r.u32()?;
            r.u64()?;
            Msg::NotImplemented
        }
        20 | 71 | 72 | 73 | 74 | 185 | 186 | 201 => Msg::NotImplemented,
        t if OUT_OF_SCOPE.contains(&t) => return Err(Halt(format!("message type {t} is out of scope"))),
        t => return Err(Halt(format!("unknown message type {t}"))),
    };
    Ok(Decoded { version, mtype, msg })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_testnet_vectors() {
        // First issuance (block 1496149), class B padded: trailing zeros ignored.
        let p = hex::decode("0000003201000200000000000041726361646500003837343733353732336361656631303530373236313162353338663639653862623530313233316639346333653837303363643864373331393164626337646600001dd7c1681d00000000000000000000000000000000000000000000000000000000").unwrap();
        let d = decode(&p).unwrap();
        assert_eq!((d.version, d.mtype), (0, 50));
        match d.msg {
            Msg::Issuance { eco, ptype, name, data, amount, .. } => {
                assert_eq!((eco, ptype, name.as_str(), amount), (1, 2, "Arcade", 8_400_000_000_000_000));
                assert_eq!(data, "874735723caef105072611b538f69e8bb501231f94c3e8703cd8d73191dbc7df");
            }
            _ => panic!(),
        }
        let t = decode(&hex::decode("0000001d0000000c0000000df84758003ff1c2ce68942ea093d54e46ec0f3bf6ad1ef92c870d44b3b3a65e2e0c232c11").unwrap()).unwrap();
        assert!(matches!(t.msg, Msg::Take { prop: 12, amount: 60_000_000_000, .. }));
        assert!(decode(&[0, 0, 0, 2]).is_err(), "out of scope halts");
        assert!(decode(&[0, 0, 0, 0, 1]).is_err(), "truncated halts");
    }
}
