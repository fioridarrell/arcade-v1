//! Script reading exactly as the existing arcade does (script.py).

/// Pushes in a script, as script.py's iter_pushes: OP_0 and non-push opcodes are
/// skipped, PUSHDATA2/4 lengths are little-endian, a truncated push ends quietly.
pub fn pushes(script: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < script.len() {
        let op = script[i];
        i += 1;
        let n = match op {
            1..=75 => op as usize,
            76 => {
                let Some(&l) = script.get(i) else { break };
                i += 1;
                l as usize
            }
            77 => {
                let Some(b) = script.get(i..i + 2) else { break };
                i += 2;
                u16::from_le_bytes([b[0], b[1]]) as usize
            }
            78 => {
                let Some(b) = script.get(i..i + 4) else { break };
                i += 4;
                u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize
            }
            _ => continue, // OP_0 and every non-push opcode
        };
        let Some(d) = script.get(i..i + n) else { break };
        out.push(d);
        i += n;
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptKind {
    P2pkh([u8; 20]),
    P2sh([u8; 20]),
    /// Pay-to-pubkey; addressed like P2PKH of hash160(pubkey).
    P2pk(Vec<u8>),
    Multisig { m: u8, keys: Vec<Vec<u8>> },
    NullData,
    Other,
}

pub fn classify(s: &[u8]) -> ScriptKind {
    if s.len() == 25 && s[0] == 0x76 && s[1] == 0xa9 && s[2] == 0x14 && s[23] == 0x88 && s[24] == 0xac {
        return ScriptKind::P2pkh(s[3..23].try_into().unwrap());
    }
    if s.len() == 23 && s[0] == 0xa9 && s[1] == 0x14 && s[22] == 0x87 {
        return ScriptKind::P2sh(s[2..22].try_into().unwrap());
    }
    if (s.len() == 35 && s[0] == 33 && s[34] == 0xac) || (s.len() == 67 && s[0] == 65 && s[66] == 0xac) {
        return ScriptKind::P2pk(s[1..s.len() - 1].to_vec());
    }
    if s.first() == Some(&0x6a) {
        return ScriptKind::NullData;
    }
    // OP_m <keys> OP_n OP_CHECKMULTISIG, key count == n, each key 33 or 65 bytes.
    if s.len() >= 3 && *s.last().unwrap() == 0xae {
        let m = s[0];
        let n = s[s.len() - 2];
        if (0x51..=0x60).contains(&m) && (0x51..=0x60).contains(&n) {
            let mut keys = Vec::new();
            let mut i = 1usize;
            let end = s.len() - 2;
            let mut ok = true;
            while i < end {
                let l = s[i] as usize;
                if (l != 33 && l != 65) || i + 1 + l > end {
                    ok = false;
                    break;
                }
                keys.push(s[i + 1..i + 1 + l].to_vec());
                i += 1 + l;
            }
            if ok && i == end && keys.len() == (n - 0x50) as usize {
                return ScriptKind::Multisig { m: m - 0x50, keys };
            }
        }
    }
    ScriptKind::Other
}

/// First push of an OP_RETURN script (the arcade reads only the first).
pub fn op_return_data(s: &[u8]) -> Option<&[u8]> {
    if s.first() != Some(&0x6a) {
        return None;
    }
    pushes(&s[1..]).into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pushes_follow_script_py() {
        // OP_0, push 2, OP_DUP (skipped), PUSHDATA1 3, truncated push 5 (ends)
        let s = [0x00, 0x02, 0xaa, 0xbb, 0x76, 0x4c, 0x03, 1, 2, 3, 0x05, 9];
        assert_eq!(pushes(&s), vec![&[0xaa, 0xbb][..], &[1, 2, 3][..]]);
        let p2 = [0x4d, 0x02, 0x00, 7, 8];
        assert_eq!(pushes(&p2), vec![&[7, 8][..]]);
    }

    #[test]
    fn classifies() {
        let mut p2pkh = vec![0x76, 0xa9, 0x14];
        p2pkh.extend([1u8; 20]);
        p2pkh.extend([0x88, 0xac]);
        assert_eq!(classify(&p2pkh), ScriptKind::P2pkh([1; 20]));
        let mut ms = vec![0x51, 33];
        ms.extend([2u8; 33]);
        ms.push(33);
        ms.extend([3u8; 33]);
        ms.extend([0x52, 0xae]);
        assert!(matches!(classify(&ms), ScriptKind::Multisig { m: 1, ref keys } if keys.len() == 2));
        assert_eq!(op_return_data(&[0x6a, 0x04, b'a', b'r', b'c', b'd']), Some(&b"arcd"[..]));
    }
}
