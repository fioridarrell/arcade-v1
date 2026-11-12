//! Scripts: what locks a coin and what unlocks it (docs/ARCADECHAIN.md §8).
//!
//! ARCADE's script is a small, strict part of Bitcoin's, enough for everything V1.0 signs on
//! the Dogecoin family: pay to a key hash, pay to a script hash, the exchange's pre-signed
//! trades, the prize pool, bare multisig, a hash lock and a block-height lock.
//!
//! Where Bitcoin grew its strict rules one soft fork at a time, here they hold from block 1,
//! so a transaction has one way to be written and nobody but its signers can change its id:
//!
//! - an opcode outside the list below makes the script invalid, even in a branch that
//!   isn't run (new opcodes come by upgrade);
//! - every push is written the shortest way, and an unlocking script only pushes;
//! - `OP_IF` takes exactly nothing or the number 1;
//! - a signature is strict DER with a low S, a public key is compressed or uncompressed;
//! - a signature that isn't empty must hold, or the whole script fails;
//! - the hash types are the three V1.0 signs with: ALL, ALL|ANYONECANPAY and
//!   SINGLE|ANYONECANPAY, and a SINGLE without its own output is refused;
//! - at the end exactly one item is left, and it is true.
//!
//! There is no `OP_CODESEPARATOR` and nothing is cut out of the script before signing: the
//! script code a signature covers is the whole locking script (or redeem script).

use arcade_chain::encode::write_varint;
use arcade_chain::hash::{hash160, sha256, sha256d};
use arcade_chain::Tx;
use secp256k1::{ecdsa::Signature, Message, PublicKey, SECP256K1};

pub const MAX_SCRIPT_BYTES: usize = 10_000;
pub const MAX_PUSH_BYTES: usize = 520;
/// Opcodes that aren't pushes in one script; a multisig counts once more for each key.
pub const MAX_OPS: usize = 201;
pub const MAX_STACK: usize = 1_000;
pub const MAX_MULTISIG_KEYS: i64 = 20;

pub const SIGHASH_ALL: u8 = 0x01;
pub const SIGHASH_ALL_ANYONECANPAY: u8 = 0x81;
pub const SIGHASH_SINGLE_ANYONECANPAY: u8 = 0x83;

pub const OP_0: u8 = 0x00;
pub const OP_PUSHDATA1: u8 = 0x4c;
pub const OP_PUSHDATA2: u8 = 0x4d;
pub const OP_1NEGATE: u8 = 0x4f;
pub const OP_1: u8 = 0x51;
pub const OP_16: u8 = 0x60;
pub const OP_IF: u8 = 0x63;
pub const OP_NOTIF: u8 = 0x64;
pub const OP_ELSE: u8 = 0x67;
pub const OP_ENDIF: u8 = 0x68;
pub const OP_VERIFY: u8 = 0x69;
pub const OP_RETURN: u8 = 0x6a;
pub const OP_2DROP: u8 = 0x6d;
pub const OP_DROP: u8 = 0x75;
pub const OP_DUP: u8 = 0x76;
pub const OP_EQUAL: u8 = 0x87;
pub const OP_EQUALVERIFY: u8 = 0x88;
pub const OP_SHA256: u8 = 0xa8;
pub const OP_HASH160: u8 = 0xa9;
pub const OP_HASH256: u8 = 0xaa;
pub const OP_CHECKSIG: u8 = 0xac;
pub const OP_CHECKSIGVERIFY: u8 = 0xad;
pub const OP_CHECKMULTISIG: u8 = 0xae;
pub const OP_CHECKMULTISIGVERIFY: u8 = 0xaf;
pub const OP_CHECKLOCKTIMEVERIFY: u8 = 0xb1;

/// Every opcode that isn't a push.
const OPCODES: [u8; 18] = [
    OP_IF,
    OP_NOTIF,
    OP_ELSE,
    OP_ENDIF,
    OP_VERIFY,
    OP_RETURN,
    OP_2DROP,
    OP_DROP,
    OP_DUP,
    OP_EQUAL,
    OP_EQUALVERIFY,
    OP_SHA256,
    OP_HASH160,
    OP_HASH256,
    OP_CHECKSIG,
    OP_CHECKSIGVERIFY,
    OP_CHECKMULTISIG,
    OP_CHECKMULTISIGVERIFY,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ScriptError {
    #[error("there is no such input")]
    NoInput,
    #[error("a script of more than 10,000 bytes")]
    TooLong,
    #[error("a push runs past the end of the script")]
    Truncated,
    #[error("a push of more than 520 bytes")]
    PushTooBig,
    #[error("a push that isn't written the shortest way")]
    PushNotMinimal,
    #[error("opcode {0:#04x} is not part of ARCADE's script")]
    UnknownOp(u8),
    #[error("an unlocking script may only push")]
    NotPushOnly,
    #[error("more than 201 opcodes")]
    TooManyOps,
    #[error("more than 1,000 items on the stack")]
    StackTooBig,
    #[error("an opcode found too few items on the stack")]
    StackEmpty,
    #[error("a VERIFY found false")]
    Verify,
    #[error("the script ran into OP_RETURN")]
    Return,
    #[error("IF, ELSE and ENDIF don't pair up")]
    Conditional,
    #[error("OP_IF takes nothing or the number 1")]
    IfArgument,
    #[error("a number that is too long or not written the shortest way")]
    Number,
    #[error("a signature that isn't strict DER with a low S")]
    SigEncoding,
    #[error("a hash type other than ALL, ALL|ANYONECANPAY and SINGLE|ANYONECANPAY")]
    SigHashType,
    #[error("SINGLE in an input that has no output of its own")]
    SingleWithoutOutput,
    #[error("a public key that is neither compressed nor uncompressed, or not on the curve")]
    KeyEncoding,
    #[error("a signature that doesn't hold")]
    SigFailed,
    #[error("a multisig's key or signature count is out of range")]
    MultisigCount,
    #[error("a multisig's extra item must be empty")]
    MultisigDummy,
    #[error("the lock height isn't reached, or the input's sequence switches locks off")]
    LockTime,
    #[error("the redeem script doesn't match the coin's script hash")]
    RedeemHash,
    #[error("the script must end with exactly one true item")]
    NotClean,
}

/// The 32 bytes input `index` signs when spending a coin whose script code is `code`: the
/// legacy signature hash, byte for byte what V1.0's wallet signs (`arcade-wallet/legacy.rs`).
pub fn sighash(tx: &Tx, index: usize, code: &[u8], hash_type: u8) -> Result<[u8; 32], ScriptError> {
    if index >= tx.inputs.len() {
        return Err(ScriptError::NoInput);
    }
    let one_input = match hash_type {
        SIGHASH_ALL => false,
        SIGHASH_ALL_ANYONECANPAY | SIGHASH_SINGLE_ANYONECANPAY => true,
        _ => return Err(ScriptError::SigHashType),
    };
    let single = hash_type == SIGHASH_SINGLE_ANYONECANPAY;
    if single && index >= tx.outputs.len() {
        return Err(ScriptError::SingleWithoutOutput);
    }
    let mut raw = Vec::with_capacity(tx.raw.len() + code.len() + 8);
    raw.extend_from_slice(&tx.version.to_le_bytes());
    write_varint(&mut raw, if one_input { 1 } else { tx.inputs.len() as u64 });
    for (n, input) in tx.inputs.iter().enumerate() {
        if one_input && n != index {
            continue;
        }
        raw.extend_from_slice(&input.prevout.txid.0);
        raw.extend_from_slice(&input.prevout.vout.to_le_bytes());
        if n == index {
            write_varint(&mut raw, code.len() as u64);
            raw.extend_from_slice(code);
        } else {
            write_varint(&mut raw, 0);
        }
        raw.extend_from_slice(&input.sequence.to_le_bytes());
    }
    if single {
        // The outputs before its own stand empty at value −1; those after aren't covered.
        write_varint(&mut raw, index as u64 + 1);
        for _ in 0..index {
            raw.extend_from_slice(&u64::MAX.to_le_bytes());
            write_varint(&mut raw, 0);
        }
    } else {
        write_varint(&mut raw, tx.outputs.len() as u64);
    }
    for (n, out) in tx.outputs.iter().enumerate() {
        if single && n != index {
            continue;
        }
        raw.extend_from_slice(&out.value.to_le_bytes());
        write_varint(&mut raw, out.script_pubkey.len() as u64);
        raw.extend_from_slice(&out.script_pubkey);
    }
    raw.extend_from_slice(&tx.lock_time.to_le_bytes());
    raw.extend_from_slice(&(hash_type as u32).to_le_bytes());
    Ok(sha256d(&raw).0)
}

/// The script hash of a pay-to-script-hash coin: `HASH160 <20 bytes> EQUAL`, nothing else.
pub fn p2sh_hash(script: &[u8]) -> Option<&[u8]> {
    match script {
        [OP_HASH160, 20, hash @ .., OP_EQUAL] if hash.len() == 20 => Some(hash),
        _ => None,
    }
}

/// The script that pays an address: a version byte, then the 20-byte hash of a key or of a
/// script. `None` when the version isn't one of the network's two.
pub fn pay_to(net: &crate::params::Net, address: &[u8; 21]) -> Option<Vec<u8>> {
    let hash = &address[1..];
    let mut s = Vec::with_capacity(25);
    if address[0] == net.p2pkh_version {
        s.extend_from_slice(&[OP_DUP, OP_HASH160, 20]);
        s.extend_from_slice(hash);
        s.extend_from_slice(&[OP_EQUALVERIFY, OP_CHECKSIG]);
    } else if address[0] == net.p2sh_version {
        s.extend_from_slice(&[OP_HASH160, 20]);
        s.extend_from_slice(hash);
        s.push(OP_EQUAL);
    } else {
        return None;
    }
    Some(s)
}

/// A coin nobody can ever spend, which the coin set therefore doesn't keep: a script that
/// starts with `OP_RETURN` or is longer than a script may be.
pub fn unspendable(script: &[u8]) -> bool {
    script.first() == Some(&OP_RETURN) || script.len() > MAX_SCRIPT_BYTES
}

/// Push `data` the one way the rules allow.
pub fn push(script: &mut Vec<u8>, data: &[u8]) {
    match data {
        [] => script.push(OP_0),
        [n @ 1..=16] => script.push(OP_1 - 1 + n),
        [0x81] => script.push(OP_1NEGATE),
        _ => {
            match data.len() {
                0..=75 => script.push(data.len() as u8),
                76..=255 => script.extend_from_slice(&[OP_PUSHDATA1, data.len() as u8]),
                _ => {
                    script.push(OP_PUSHDATA2);
                    script.extend_from_slice(&(data.len() as u16).to_le_bytes());
                }
            }
            script.extend_from_slice(data);
        }
    }
}

enum Op {
    Push(Vec<u8>),
    Code(u8),
}

/// The next opcode of `script` at `pos`, refused if it is a push written a longer way.
fn next_op(script: &[u8], pos: &mut usize) -> Result<Op, ScriptError> {
    let op = script[*pos];
    *pos += 1;
    let len = match op {
        OP_0 => return Ok(Op::Push(Vec::new())),
        1..=75 => op as usize,
        OP_PUSHDATA1 => {
            let n = *script.get(*pos).ok_or(ScriptError::Truncated)? as usize;
            *pos += 1;
            if n <= 75 {
                return Err(ScriptError::PushNotMinimal);
            }
            n
        }
        OP_PUSHDATA2 => {
            let b = script.get(*pos..*pos + 2).ok_or(ScriptError::Truncated)?;
            *pos += 2;
            let n = u16::from_le_bytes([b[0], b[1]]) as usize;
            if n <= 255 {
                return Err(ScriptError::PushNotMinimal);
            }
            n
        }
        OP_1NEGATE => return Ok(Op::Push(vec![0x81])),
        OP_1..=OP_16 => return Ok(Op::Push(vec![op - OP_1 + 1])),
        _ if OPCODES.contains(&op) || op == OP_CHECKLOCKTIMEVERIFY => return Ok(Op::Code(op)),
        _ => return Err(ScriptError::UnknownOp(op)),
    };
    if len > MAX_PUSH_BYTES {
        return Err(ScriptError::PushTooBig);
    }
    let data = script.get(*pos..*pos + len).ok_or(ScriptError::Truncated)?;
    *pos += len;
    // One byte that an opcode stands for is written as that opcode.
    if len == 1 && (matches!(data[0], 1..=16) || data[0] == 0x81) {
        return Err(ScriptError::PushNotMinimal);
    }
    Ok(Op::Push(data.to_vec()))
}

/// Bitcoin's truth: any byte set, except in a zero with only its sign bit.
fn truthy(v: &[u8]) -> bool {
    match v.split_last() {
        None => false,
        Some((&last, rest)) => rest.iter().any(|&b| b != 0) || (last != 0 && last != 0x80),
    }
}

fn boolean(b: bool) -> Vec<u8> {
    if b {
        vec![1]
    } else {
        Vec::new()
    }
}

/// A script number of at most `max_len` bytes: little-endian, the top bit its sign, written
/// the shortest way.
fn number(bytes: &[u8], max_len: usize) -> Result<i64, ScriptError> {
    if bytes.len() > max_len {
        return Err(ScriptError::Number);
    }
    let Some((&last, rest)) = bytes.split_last() else { return Ok(0) };
    if last & 0x7f == 0 && rest.last().map_or(true, |b| b & 0x80 == 0) {
        return Err(ScriptError::Number);
    }
    let mut v = ((last & 0x7f) as i64) << (8 * rest.len());
    for (i, &b) in rest.iter().enumerate() {
        v |= (b as i64) << (8 * i);
    }
    Ok(if last & 0x80 != 0 { -v } else { v })
}

/// BIP 66: `30 len 02 rlen R 02 slen S`, each number positive and without spare zeros.
fn strict_der(sig: &[u8]) -> bool {
    if sig.len() < 8 || sig.len() > 72 || sig[0] != 0x30 || sig[1] as usize != sig.len() - 2 {
        return false;
    }
    let rlen = sig[3] as usize;
    if 5 + rlen >= sig.len() {
        return false;
    }
    let slen = sig[5 + rlen] as usize;
    if rlen + slen + 6 != sig.len() {
        return false;
    }
    let number_ok = |at: usize, len: usize| {
        sig[at - 2] == 0x02 && len != 0 && sig[at] & 0x80 == 0 && !(len > 1 && sig[at] == 0 && sig[at + 1] & 0x80 == 0)
    };
    number_ok(4, rlen) && number_ok(6 + rlen, slen)
}

fn pop(stack: &mut Vec<Vec<u8>>) -> Result<Vec<u8>, ScriptError> {
    stack.pop().ok_or(ScriptError::StackEmpty)
}

struct Machine<'a> {
    tx: &'a Tx,
    index: usize,
    /// Signature checks done so far (a block has a limit).
    checks: &'a mut u32,
}

impl Machine<'_> {
    /// What a signature signs under script code `code`, once its form is checked.
    fn digest(&self, sig: &[u8], code: &[u8]) -> Result<[u8; 32], ScriptError> {
        let (&hash_type, der) = sig.split_last().ok_or(ScriptError::SigEncoding)?;
        if !matches!(hash_type, SIGHASH_ALL | SIGHASH_ALL_ANYONECANPAY | SIGHASH_SINGLE_ANYONECANPAY) {
            return Err(ScriptError::SigHashType);
        }
        if !strict_der(der) {
            return Err(ScriptError::SigEncoding);
        }
        sighash(self.tx, self.index, code, hash_type)
    }

    /// Whether `sig` (already through `digest`) is `key`'s over `digest`.
    fn holds(&mut self, sig: &[u8], key: &[u8], digest: &[u8; 32]) -> Result<bool, ScriptError> {
        if !matches!((key.len(), key.first()), (33, Some(2 | 3)) | (65, Some(4))) {
            return Err(ScriptError::KeyEncoding);
        }
        let key = PublicKey::from_slice(key).map_err(|_| ScriptError::KeyEncoding)?;
        let sig = Signature::from_der(&sig[..sig.len() - 1]).map_err(|_| ScriptError::SigEncoding)?;
        let mut low = sig;
        low.normalize_s();
        if low != sig {
            return Err(ScriptError::SigEncoding);
        }
        *self.checks += 1;
        Ok(SECP256K1.verify_ecdsa(&Message::from_digest(*digest), &sig, &key).is_ok())
    }

    /// `OP_CHECKSIG`: an empty signature is false; any other must hold.
    fn check_sig(&mut self, sig: &[u8], key: &[u8], code: &[u8]) -> Result<bool, ScriptError> {
        if sig.is_empty() {
            return Ok(false);
        }
        let digest = self.digest(sig, code)?;
        if !self.holds(sig, key, &digest)? {
            return Err(ScriptError::SigFailed);
        }
        Ok(true)
    }

    /// `OP_CHECKMULTISIG`: the signatures must hold for keys in the keys' order.
    fn check_multisig(&mut self, stack: &mut Vec<Vec<u8>>, ops: &mut usize, code: &[u8]) -> Result<bool, ScriptError> {
        let n = number(&pop(stack)?, 1)?;
        if !(0..=MAX_MULTISIG_KEYS).contains(&n) {
            return Err(ScriptError::MultisigCount);
        }
        *ops += n as usize;
        if *ops > MAX_OPS {
            return Err(ScriptError::TooManyOps);
        }
        // Popped last-first, and matched last-first as Bitcoin does.
        let keys = (0..n).map(|_| pop(stack)).collect::<Result<Vec<_>, _>>()?;
        let m = number(&pop(stack)?, 1)?;
        if !(0..=n).contains(&m) {
            return Err(ScriptError::MultisigCount);
        }
        let sigs = (0..m).map(|_| pop(stack)).collect::<Result<Vec<_>, _>>()?;
        if !pop(stack)?.is_empty() {
            return Err(ScriptError::MultisigDummy);
        }
        let mut digests = Vec::with_capacity(sigs.len());
        for sig in &sigs {
            digests.push(if sig.is_empty() { None } else { Some(self.digest(sig, code)?) });
        }
        let (mut s, mut k) = (0, 0);
        let mut ok = true;
        while ok && s < sigs.len() {
            let held = match &digests[s] {
                Some(d) => self.holds(&sigs[s], &keys[k], d)?,
                None => false,
            };
            if held {
                s += 1;
            }
            k += 1;
            ok = sigs.len() - s <= keys.len() - k;
        }
        if !ok && sigs.iter().any(|s| !s.is_empty()) {
            return Err(ScriptError::SigFailed);
        }
        Ok(ok)
    }

    fn run(&mut self, script: &[u8], stack: &mut Vec<Vec<u8>>) -> Result<(), ScriptError> {
        if script.len() > MAX_SCRIPT_BYTES {
            return Err(ScriptError::TooLong);
        }
        let mut pos = 0;
        let mut ops = 0usize;
        // One entry for each open IF: (is this branch run, has its ELSE come).
        let mut open: Vec<(bool, bool)> = Vec::new();
        while pos < script.len() {
            let op = next_op(script, &mut pos)?;
            let live = open.iter().all(|c| c.0);
            let code = match op {
                Op::Push(data) => {
                    if live {
                        stack.push(data);
                        if stack.len() > MAX_STACK {
                            return Err(ScriptError::StackTooBig);
                        }
                    }
                    continue;
                }
                Op::Code(code) => code,
            };
            ops += 1;
            if ops > MAX_OPS {
                return Err(ScriptError::TooManyOps);
            }
            match code {
                OP_IF | OP_NOTIF => {
                    let mut branch = false;
                    if live {
                        branch = match pop(stack)?.as_slice() {
                            [] => false,
                            [1] => true,
                            _ => return Err(ScriptError::IfArgument),
                        };
                        if code == OP_NOTIF {
                            branch = !branch;
                        }
                    }
                    open.push((branch, false));
                }
                OP_ELSE => {
                    let last = open.last_mut().ok_or(ScriptError::Conditional)?;
                    if last.1 {
                        return Err(ScriptError::Conditional);
                    }
                    *last = (!last.0, true);
                }
                OP_ENDIF => {
                    open.pop().ok_or(ScriptError::Conditional)?;
                }
                _ if !live => {}
                OP_VERIFY => {
                    if !truthy(&pop(stack)?) {
                        return Err(ScriptError::Verify);
                    }
                }
                OP_RETURN => return Err(ScriptError::Return),
                OP_DROP => {
                    pop(stack)?;
                }
                OP_2DROP => {
                    pop(stack)?;
                    pop(stack)?;
                }
                OP_DUP => {
                    let top = stack.last().ok_or(ScriptError::StackEmpty)?.clone();
                    stack.push(top);
                }
                OP_EQUAL | OP_EQUALVERIFY => {
                    let equal = pop(stack)? == pop(stack)?;
                    if code == OP_EQUAL {
                        stack.push(boolean(equal));
                    } else if !equal {
                        return Err(ScriptError::Verify);
                    }
                }
                OP_SHA256 => {
                    let top = pop(stack)?;
                    stack.push(sha256(&top).to_vec());
                }
                OP_HASH160 => {
                    let top = pop(stack)?;
                    stack.push(hash160(&top).to_vec());
                }
                OP_HASH256 => {
                    let top = pop(stack)?;
                    stack.push(sha256d(&top).0.to_vec());
                }
                OP_CHECKSIG | OP_CHECKSIGVERIFY => {
                    let key = pop(stack)?;
                    let sig = pop(stack)?;
                    let ok = self.check_sig(&sig, &key, script)?;
                    if code == OP_CHECKSIG {
                        stack.push(boolean(ok));
                    } else if !ok {
                        return Err(ScriptError::Verify);
                    }
                }
                OP_CHECKMULTISIG | OP_CHECKMULTISIGVERIFY => {
                    let ok = self.check_multisig(stack, &mut ops, script)?;
                    if code == OP_CHECKMULTISIG {
                        stack.push(boolean(ok));
                    } else if !ok {
                        return Err(ScriptError::Verify);
                    }
                }
                OP_CHECKLOCKTIMEVERIFY => {
                    // The lock is a block height (ARCADE has no clock-time locks); it stays
                    // on the stack, as in Bitcoin.
                    let lock = number(stack.last().ok_or(ScriptError::StackEmpty)?, 5)?;
                    if lock < 0 || lock > self.tx.lock_time as i64 || self.tx.inputs[self.index].sequence == u32::MAX {
                        return Err(ScriptError::LockTime);
                    }
                }
                other => unreachable!("next_op let {other:#04x} through"),
            }
            if stack.len() > MAX_STACK {
                return Err(ScriptError::StackTooBig);
            }
        }
        if !open.is_empty() {
            return Err(ScriptError::Conditional);
        }
        Ok(())
    }
}

/// The items an unlocking script pushes; it may do nothing else.
fn pushes(script_sig: &[u8]) -> Result<Vec<Vec<u8>>, ScriptError> {
    if script_sig.len() > MAX_SCRIPT_BYTES {
        return Err(ScriptError::TooLong);
    }
    let mut stack = Vec::new();
    let mut pos = 0;
    while pos < script_sig.len() {
        match next_op(script_sig, &mut pos)? {
            Op::Push(data) => stack.push(data),
            Op::Code(_) => return Err(ScriptError::NotPushOnly),
        }
        if stack.len() > MAX_STACK {
            return Err(ScriptError::StackTooBig);
        }
    }
    Ok(stack)
}

/// Whether input `index` of `tx` may spend a coin locked by `coin_script`. `checks` counts
/// the signature checks done, for the block's limit.
pub fn verify_input(tx: &Tx, index: usize, coin_script: &[u8], checks: &mut u32) -> Result<(), ScriptError> {
    let input = tx.inputs.get(index).ok_or(ScriptError::NoInput)?;
    let mut stack = pushes(&input.script_sig)?;
    let mut machine = Machine { tx, index, checks };
    if let Some(hash) = p2sh_hash(coin_script) {
        let redeem = pop(&mut stack)?;
        if hash160(&redeem) != hash {
            return Err(ScriptError::RedeemHash);
        }
        machine.run(&redeem, &mut stack)?;
    } else {
        machine.run(coin_script, &mut stack)?;
    }
    match stack.as_slice() {
        [last] if truthy(last) => Ok(()),
        _ => Err(ScriptError::NotClean),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::block::TX_FORMAT;
    use arcade_chain::hash::Hash256;
    use arcade_chain::{OutPoint, TxIn, TxOut};
    use arcade_wallet::legacy;
    use secp256k1::SecretKey;

    pub(crate) fn key(n: u8) -> (SecretKey, Vec<u8>) {
        let sk = SecretKey::from_slice(&[n; 32]).unwrap();
        (sk, PublicKey::from_secret_key(SECP256K1, &sk).serialize().to_vec())
    }

    pub(crate) fn p2pkh(pubkey: &[u8]) -> Vec<u8> {
        let mut s = vec![OP_DUP, OP_HASH160, 20];
        s.extend_from_slice(&hash160(pubkey));
        s.extend_from_slice(&[OP_EQUALVERIFY, OP_CHECKSIG]);
        s
    }

    pub(crate) fn p2sh(redeem: &[u8]) -> Vec<u8> {
        let mut s = vec![OP_HASH160, 20];
        s.extend_from_slice(&hash160(redeem));
        s.push(OP_EQUAL);
        s
    }

    /// A transaction as it comes off the wire: id and raw bytes filled in.
    pub(crate) fn tx(inputs: Vec<(OutPoint, Vec<u8>, u32)>, outputs: Vec<(u64, Vec<u8>)>, lock_time: u32) -> Tx {
        let t = Tx {
            version: 1,
            time: None,
            inputs: inputs.into_iter().map(|(prevout, script_sig, sequence)| TxIn { prevout, script_sig, sequence, witness: Vec::new() }).collect(),
            outputs: outputs.into_iter().map(|(value, script_pubkey)| TxOut { value, script_pubkey }).collect(),
            lock_time,
            txid: Hash256([0; 32]),
            raw: Vec::new(),
            hogex: false,
        };
        Tx::parse_with(&t.encode_legacy(), &TX_FORMAT).unwrap()
    }

    pub(crate) fn coin(n: u8) -> OutPoint {
        OutPoint { txid: Hash256([n; 32]), vout: n as u32 }
    }

    /// `t` with input `index` given the unlocking script `script_sig`.
    pub(crate) fn with_sig(t: &Tx, index: usize, script_sig: Vec<u8>) -> Tx {
        let mut t = t.clone();
        t.inputs[index].script_sig = script_sig;
        Tx::parse_with(&t.encode_legacy(), &TX_FORMAT).unwrap()
    }

    pub(crate) fn sign(t: &Tx, index: usize, code: &[u8], sk: &SecretKey, hash_type: u8) -> Vec<u8> {
        legacy::sign(sk, &sighash(t, index, code, hash_type).unwrap(), hash_type as u32)
    }

    fn sig_and_key(sig: &[u8], pubkey: &[u8]) -> Vec<u8> {
        let mut s = Vec::new();
        push(&mut s, sig);
        push(&mut s, pubkey);
        s
    }

    fn run(t: &Tx, index: usize, coin_script: &[u8]) -> Result<(), ScriptError> {
        verify_input(t, index, coin_script, &mut 0)
    }

    /// Two inputs, two outputs, nothing signed.
    fn plain() -> Tx {
        tx(vec![(coin(1), vec![], u32::MAX), (coin(2), vec![], u32::MAX)], vec![(5_000, p2pkh(&key(9).1)), (7_000, p2pkh(&key(8).1))], 0)
    }

    #[test]
    fn the_signature_hash_is_the_wallets() {
        let t = tx(
            vec![(coin(1), vec![1, 2, 3], 7), (coin(2), vec![], u32::MAX), (coin(3), vec![9; 80], 0)],
            vec![(1, vec![OP_RETURN, 2, 0xaa, 0xbb]), (u64::MAX / 3, p2pkh(&key(1).1)), (0, vec![]), (77, p2sh(&[OP_1]))],
            123_456,
        );
        let shape = legacy::Shape {
            version: t.version,
            time: None,
            inputs: t.inputs.iter().map(|i| legacy::In { prevout: i.prevout, sequence: i.sequence }).collect(),
            outputs: t.outputs.iter().map(|o| (o.value, o.script_pubkey.clone())).collect(),
            lock_time: t.lock_time,
        };
        let code = p2pkh(&key(5).1);
        for index in 0..3 {
            for hash_type in [SIGHASH_ALL, SIGHASH_ALL_ANYONECANPAY, SIGHASH_SINGLE_ANYONECANPAY] {
                let want = legacy::sighash(&shape, index, &code, hash_type as u32).unwrap();
                assert_eq!(sighash(&t, index, &code, hash_type), Ok(want), "input {index} type {hash_type:#x}");
            }
        }
        // Every other hash type is refused, and a SINGLE with no output of its own.
        for hash_type in [0x00, 0x02, 0x03, 0x04, 0x80, 0x82, 0x41, 0xff] {
            assert_eq!(sighash(&t, 0, &code, hash_type), Err(ScriptError::SigHashType));
        }
        let short = tx(vec![(coin(1), vec![], 0), (coin(2), vec![], 0)], vec![(1, vec![])], 0);
        assert!(sighash(&short, 0, &code, SIGHASH_SINGLE_ANYONECANPAY).is_ok());
        assert_eq!(sighash(&short, 1, &code, SIGHASH_SINGLE_ANYONECANPAY), Err(ScriptError::SingleWithoutOutput));
        assert_eq!(sighash(&short, 2, &code, SIGHASH_ALL), Err(ScriptError::NoInput));
    }

    #[test]
    fn a_key_hash_coin_is_spent_by_its_key_alone() {
        let (sk, pk) = key(1);
        let lock = p2pkh(&pk);
        let t = plain();
        let good = with_sig(&t, 0, sig_and_key(&sign(&t, 0, &lock, &sk, SIGHASH_ALL), &pk));
        let mut checks = 0;
        assert_eq!(verify_input(&good, 0, &lock, &mut checks), Ok(()));
        assert_eq!(checks, 1);
        // Exactly as the wallet writes it.
        assert_eq!(good.inputs[0].script_sig, arcade_wallet::legs::script_sig(&sign(&t, 0, &lock, &sk, SIGHASH_ALL), &pk));

        // Another key's signature, or the right signature beside another key.
        let (sk2, pk2) = key(2);
        let other = with_sig(&t, 0, sig_and_key(&sign(&t, 0, &lock, &sk2, SIGHASH_ALL), &pk));
        assert_eq!(run(&other, 0, &lock), Err(ScriptError::SigFailed));
        let other = with_sig(&t, 0, sig_and_key(&sign(&t, 0, &lock, &sk2, SIGHASH_ALL), &pk2));
        assert_eq!(run(&other, 0, &lock), Err(ScriptError::Verify));
        // The signature is for this input and these outputs.
        assert_eq!(run(&good, 0, &p2pkh(&pk2)), Err(ScriptError::Verify));
        let moved = with_sig(&t, 1, good.inputs[0].script_sig.clone());
        assert_eq!(run(&moved, 1, &lock), Err(ScriptError::SigFailed));
        let mut paid_elsewhere = good.clone();
        paid_elsewhere.outputs[0].value += 1;
        assert_eq!(run(&paid_elsewhere, 0, &lock), Err(ScriptError::SigFailed));
        // No signature at all is false, not an error of form; the script still fails.
        let empty = with_sig(&t, 0, sig_and_key(&[], &pk));
        assert_eq!(run(&empty, 0, &lock), Err(ScriptError::NotClean));
        // An uncompressed key spends the coin locked to its own hash.
        let long = PublicKey::from_slice(&pk).unwrap().serialize_uncompressed().to_vec();
        let lock = p2pkh(&long);
        let good = with_sig(&t, 0, sig_and_key(&sign(&t, 0, &lock, &sk, SIGHASH_ALL), &long));
        assert_eq!(run(&good, 0, &lock), Ok(()));
    }

    #[test]
    fn a_pre_signed_trade_holds_as_others_complete_it() {
        // The seller's leg: input 0 for output 0, signed SINGLE|ANYONECANPAY.
        let (seller, seller_pk) = key(1);
        let (buyer, buyer_pk) = key(2);
        let (seller_lock, buyer_lock) = (p2pkh(&seller_pk), p2pkh(&buyer_pk));
        let leg = tx(vec![(coin(1), vec![], u32::MAX)], vec![(9_000, seller_lock.clone())], 0);
        let leg_sig = sig_and_key(&sign(&leg, 0, &seller_lock, &seller, SIGHASH_SINGLE_ANYONECANPAY), &seller_pk);

        // The buyer adds coins and outputs and signs everything.
        let whole = tx(
            vec![(coin(1), leg_sig.clone(), u32::MAX), (coin(2), vec![], u32::MAX)],
            vec![(9_000, seller_lock.clone()), (1, buyer_lock.clone()), (400, buyer_lock.clone())],
            0,
        );
        let whole = with_sig(&whole, 1, sig_and_key(&sign(&whole, 1, &buyer_lock, &buyer, SIGHASH_ALL), &buyer_pk));
        assert_eq!(run(&whole, 0, &seller_lock), Ok(()));
        assert_eq!(run(&whole, 1, &buyer_lock), Ok(()));

        // Paying the seller less, or moving the leg to another place, breaks the leg.
        let mut less = whole.clone();
        less.outputs[0].value = 8_999;
        assert_eq!(run(&less, 0, &seller_lock), Err(ScriptError::SigFailed));
        let swapped = tx(
            vec![(coin(2), vec![], u32::MAX), (coin(1), leg_sig, u32::MAX)],
            vec![(1, buyer_lock.clone()), (9_000, seller_lock.clone())],
            0,
        );
        assert_eq!(run(&swapped, 1, &seller_lock), Err(ScriptError::SigFailed));

        // A bid's coins: ALL|ANYONECANPAY lets others add inputs, never change what is paid.
        let bid = tx(vec![(coin(3), vec![], u32::MAX)], vec![(50, seller_lock.clone()), (60, buyer_lock.clone())], 0);
        let bid_sig = sig_and_key(&sign(&bid, 0, &buyer_lock, &buyer, SIGHASH_ALL_ANYONECANPAY), &buyer_pk);
        let more_inputs = tx(vec![(coin(4), vec![], 5), (coin(3), bid_sig.clone(), u32::MAX)], vec![(50, seller_lock.clone()), (60, buyer_lock.clone())], 0);
        assert_eq!(run(&more_inputs, 1, &buyer_lock), Ok(()));
        let more_outputs = tx(vec![(coin(3), bid_sig, u32::MAX)], vec![(50, seller_lock.clone()), (60, buyer_lock.clone()), (1, vec![])], 0);
        assert_eq!(run(&more_outputs, 0, &buyer_lock), Err(ScriptError::SigFailed));
    }

    /// The prize pool's script (`arcaded/prizesvc.rs`).
    fn pool_script(pool: &[u8], referee: &[u8], creator: &[u8]) -> Vec<u8> {
        let mut s = vec![OP_IF];
        push(&mut s, pool);
        s.push(OP_CHECKSIGVERIFY);
        push(&mut s, referee);
        s.extend_from_slice(&[OP_CHECKSIG, OP_ELSE]);
        push(&mut s, creator);
        s.extend_from_slice(&[OP_CHECKSIG, OP_ENDIF]);
        s
    }

    #[test]
    fn a_prize_pool_pays_by_pool_and_referee_or_back_to_its_creator() {
        let ((pool, pool_pk), (referee, referee_pk), (creator, creator_pk)) = (key(1), key(2), key(3));
        let redeem = pool_script(&pool_pk, &referee_pk, &creator_pk);
        assert_eq!(redeem.len(), 108);
        let lock = p2sh(&redeem);
        let t = plain();
        let unlock = |items: &[&[u8]]| {
            let mut s = Vec::new();
            for item in items {
                push(&mut s, item);
            }
            push(&mut s, &redeem);
            s
        };
        let (pool_sig, referee_sig, creator_sig) =
            (sign(&t, 0, &redeem, &pool, SIGHASH_ALL), sign(&t, 0, &redeem, &referee, SIGHASH_ALL), sign(&t, 0, &redeem, &creator, SIGHASH_ALL));

        // The claim: referee's signature, pool's signature, 1.
        let claim = with_sig(&t, 0, unlock(&[&referee_sig, &pool_sig, &[1]]));
        let mut checks = 0;
        assert_eq!(verify_input(&claim, 0, &lock, &mut checks), Ok(()));
        assert_eq!(checks, 2);
        // The refund: creator's signature, nothing.
        let refund = with_sig(&t, 0, unlock(&[&creator_sig, &[]]));
        assert_eq!(run(&refund, 0, &lock), Ok(()));

        // Neither alone, nor the creator through the claim branch.
        assert_eq!(run(&with_sig(&t, 0, unlock(&[&pool_sig, &pool_sig, &[1]])), 0, &lock), Err(ScriptError::SigFailed));
        assert_eq!(run(&with_sig(&t, 0, unlock(&[&creator_sig, &pool_sig, &[1]])), 0, &lock), Err(ScriptError::SigFailed));
        assert_eq!(run(&with_sig(&t, 0, unlock(&[&pool_sig, &[]])), 0, &lock), Err(ScriptError::SigFailed));
        assert_eq!(run(&with_sig(&t, 0, unlock(&[&[], &pool_sig, &[1]])), 0, &lock), Err(ScriptError::NotClean));
        // The branch is chosen by nothing or 1, no other way of saying true.
        assert_eq!(run(&with_sig(&t, 0, unlock(&[&referee_sig, &pool_sig, &[2]])), 0, &lock), Err(ScriptError::IfArgument));
        assert_eq!(run(&with_sig(&t, 0, unlock(&[&referee_sig, &pool_sig, &[1, 0]])), 0, &lock), Err(ScriptError::IfArgument));
        // Something left over, another script, no script.
        assert_eq!(run(&with_sig(&t, 0, unlock(&[&[7], &creator_sig, &[]])), 0, &lock), Err(ScriptError::NotClean));
        let mut other = Vec::new();
        push(&mut other, &[OP_1]);
        assert_eq!(run(&with_sig(&t, 0, other), 0, &lock), Err(ScriptError::RedeemHash));
        assert_eq!(run(&t, 0, &lock), Err(ScriptError::StackEmpty));
        // A script-hash coin whose script is just "true" is anyone's.
        let mut open = Vec::new();
        push(&mut open, &[OP_1]);
        assert_eq!(run(&with_sig(&t, 0, open), 0, &p2sh(&[OP_1])), Ok(()));
    }

    #[test]
    fn a_signature_or_key_has_one_form() {
        let (sk, pk) = key(1);
        let lock = p2pkh(&pk);
        let t = plain();
        let good = sign(&t, 0, &lock, &sk, SIGHASH_ALL);
        let try_sig = |sig: &[u8]| run(&with_sig(&t, 0, sig_and_key(sig, &pk)), 0, &lock);
        assert_eq!(try_sig(&good), Ok(()));

        // The same signature with S on the high side.
        let parsed = Signature::from_der(&good[..good.len() - 1]).unwrap();
        let compact = parsed.serialize_compact();
        let order = hex::decode("fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141").unwrap();
        let mut high = [0u8; 64];
        high[..32].copy_from_slice(&compact[..32]);
        let mut borrow = 0i16;
        for i in (0..32).rev() {
            let d = order[i] as i16 - compact[32 + i] as i16 - borrow;
            high[32 + i] = d.rem_euclid(256) as u8;
            borrow = (d < 0) as i16;
        }
        let mut high_der = Signature::from_compact(&high).unwrap().serialize_der().to_vec();
        high_der.push(SIGHASH_ALL);
        assert_ne!(high_der, good);
        assert_eq!(try_sig(&high_der), Err(ScriptError::SigEncoding));

        // Spare zeros, a wrong length, a wrong type, trailing bytes.
        let der = &good[..good.len() - 1];
        let rlen = der[3] as usize;
        let mut padded = vec![0x30, der[1] + 1, 0x02, der[3] + 1, 0x00];
        padded.extend_from_slice(&der[4..]);
        padded.push(SIGHASH_ALL);
        assert_eq!(try_sig(&padded), Err(ScriptError::SigEncoding));
        let mut long = good.clone();
        long.insert(good.len() - 1, 0);
        assert_eq!(try_sig(&long), Err(ScriptError::SigEncoding));
        let mut wrong = good.clone();
        wrong[4 + rlen] = 0x03;
        assert_eq!(try_sig(&wrong), Err(ScriptError::SigEncoding));
        assert_eq!(try_sig(&[SIGHASH_ALL]), Err(ScriptError::SigEncoding));

        // Hash types nobody signs with here: NONE, plain SINGLE, zero.
        for hash_type in [0x00u8, 0x02, 0x03, 0x82, 0x21] {
            let mut sig = good.clone();
            *sig.last_mut().unwrap() = hash_type;
            assert_eq!(try_sig(&sig), Err(ScriptError::SigHashType), "{hash_type:#x}");
        }

        // Keys: a hybrid form, a wrong length, a point off the curve.
        let long = PublicKey::from_slice(&pk).unwrap().serialize_uncompressed();
        let mut hybrid = long;
        hybrid[0] = 6 + (long[64] & 1);
        let mut short = pk.clone();
        short.pop();
        let mut off = pk.clone();
        off[1..].fill(0xff);
        for bad in [hybrid.to_vec(), short, off] {
            let lock = p2pkh(&bad);
            let spend = with_sig(&t, 0, sig_and_key(&good, &bad));
            assert_eq!(run(&spend, 0, &lock), Err(ScriptError::KeyEncoding));
        }
    }

    #[test]
    fn a_script_has_one_way_to_be_written() {
        let t = plain();
        let anyone = [OP_1];
        // An unlocking script only pushes.
        assert_eq!(run(&with_sig(&t, 0, vec![OP_1, OP_DROP]), 0, &anyone), Err(ScriptError::NotPushOnly));
        // Pushes the long way.
        for long in [vec![1, 5], vec![1, 0x81], vec![OP_PUSHDATA1, 1, 0x55], vec![OP_PUSHDATA1, 75], vec![OP_PUSHDATA2, 255, 0], vec![0x4e, 1, 0, 0, 0, 7]] {
            let mut lock = long.clone();
            lock.resize(long.len() + 300, 0x55);
            assert!(
                matches!(run(&t, 0, &lock), Err(ScriptError::PushNotMinimal | ScriptError::UnknownOp(0x4e))),
                "{long:02x?}: {:?}",
                run(&t, 0, &lock)
            );
        }
        // The helper writes each size the short way, and the reader takes it.
        for len in [0usize, 1, 2, 75, 76, 255, 256, 520] {
            let data = vec![0x42; len];
            let mut lock = Vec::new();
            push(&mut lock, &data);
            lock.extend_from_slice(&[OP_DROP, OP_1]);
            assert_eq!(run(&t, 0, &lock), Ok(()), "{len}");
        }
        for n in [1u8, 16, 0x81] {
            let mut s = Vec::new();
            push(&mut s, &[n]);
            assert_eq!(s.len(), 1);
            assert_eq!(pushes(&s).unwrap(), vec![vec![n]]);
        }
        let mut big = Vec::new();
        push(&mut big, &[0x42; 521]);
        assert_eq!(run(&t, 0, &big), Err(ScriptError::PushTooBig));
        assert_eq!(run(&t, 0, &[5, 1, 2]), Err(ScriptError::Truncated));
        assert_eq!(run(&t, 0, &[OP_PUSHDATA2, 1]), Err(ScriptError::Truncated));

        // An opcode outside the list fails the script even where it isn't run.
        for op in [0x50u8, 0x61, 0x62, 0x65, 0x6b, 0x7c, 0x7e, 0x93, 0xa6, 0xab, 0xb0, 0xb2, 0xba, 0xff] {
            assert_eq!(run(&t, 0, &[OP_0, OP_IF, op, OP_ENDIF, OP_1]), Err(ScriptError::UnknownOp(op)));
            assert_eq!(run(&t, 0, &[op, OP_1]), Err(ScriptError::UnknownOp(op)));
        }
        // IF, ELSE and ENDIF pair up, with one ELSE.
        assert_eq!(run(&t, 0, &[OP_1, OP_IF, OP_1, OP_ELSE, OP_0, OP_ENDIF]), Ok(()));
        assert_eq!(run(&t, 0, &[OP_0, OP_IF, OP_0, OP_ELSE, OP_1, OP_ENDIF]), Ok(()));
        assert_eq!(run(&t, 0, &[OP_0, OP_NOTIF, OP_1, OP_ENDIF]), Ok(()));
        assert_eq!(run(&t, 0, &[OP_0, OP_IF, OP_1, OP_IF, OP_RETURN, OP_ELSE, OP_RETURN, OP_ENDIF, OP_ELSE, OP_1, OP_ENDIF]), Ok(()));
        assert_eq!(run(&t, 0, &[OP_1, OP_IF, OP_1]), Err(ScriptError::Conditional));
        assert_eq!(run(&t, 0, &[OP_1, OP_ENDIF]), Err(ScriptError::Conditional));
        assert_eq!(run(&t, 0, &[OP_1, OP_ELSE, OP_1]), Err(ScriptError::Conditional));
        assert_eq!(run(&t, 0, &[OP_0, OP_IF, OP_0, OP_ELSE, OP_1, OP_ELSE, OP_1, OP_ENDIF]), Err(ScriptError::Conditional));
        assert_eq!(run(&t, 0, &[OP_IF, OP_1, OP_ENDIF]), Err(ScriptError::StackEmpty));
        // OP_RETURN, and the end of the script.
        assert_eq!(run(&t, 0, &[OP_1, OP_RETURN]), Err(ScriptError::Return));
        assert_eq!(run(&t, 0, &[]), Err(ScriptError::NotClean));
        assert_eq!(run(&t, 0, &[OP_0]), Err(ScriptError::NotClean));
        assert_eq!(run(&t, 0, &[OP_1, OP_1]), Err(ScriptError::NotClean));
        assert_eq!(run(&t, 0, &[1, 0x80]), Err(ScriptError::NotClean)); // minus zero
        assert_eq!(run(&t, 0, &[2, 0, 0x80]), Err(ScriptError::NotClean));
        assert_eq!(run(&t, 0, &[2, 0, 1]), Ok(()));
        // Limits: 201 opcodes, 1,000 items, 10,000 bytes.
        let mut ops = vec![OP_1];
        ops.extend(std::iter::repeat([OP_DUP, OP_DROP]).take(100).flatten());
        ops.extend_from_slice(&[OP_DUP, OP_DROP]);
        assert_eq!(run(&t, 0, &ops), Err(ScriptError::TooManyOps));
        ops.truncate(ops.len() - 1);
        ops[201] = OP_VERIFY;
        ops.push(OP_1);
        assert_eq!(run(&t, 0, &ops), Ok(()));
        assert_eq!(run(&t, 0, &vec![OP_1; 1_001]), Err(ScriptError::StackTooBig));
        assert_eq!(run(&with_sig(&t, 0, vec![OP_1; 1_001]), 0, &anyone), Err(ScriptError::StackTooBig));
        assert_eq!(run(&t, 0, &vec![OP_1; 10_001]), Err(ScriptError::TooLong));
        assert!(unspendable(&vec![OP_1; 10_001]) && unspendable(&[OP_RETURN, 1, 2]) && !unspendable(&anyone) && !unspendable(&[]));
    }

    #[test]
    fn hash_locks_and_equality() {
        let t = plain();
        let secret = b"open sesame".to_vec();
        for (op, hash) in [(OP_SHA256, sha256(&secret).to_vec()), (OP_HASH160, hash160(&secret).to_vec()), (OP_HASH256, sha256d(&secret).0.to_vec())] {
            let mut lock = vec![op];
            push(&mut lock, &hash);
            lock.extend_from_slice(&[OP_EQUALVERIFY, OP_1]);
            let mut right = Vec::new();
            push(&mut right, &secret);
            assert_eq!(run(&with_sig(&t, 0, right), 0, &lock), Ok(()));
            let mut wrong = Vec::new();
            push(&mut wrong, b"open barley");
            assert_eq!(run(&with_sig(&t, 0, wrong), 0, &lock), Err(ScriptError::Verify));
            assert_eq!(run(&t, 0, &lock), Err(ScriptError::StackEmpty));
        }
        // HASH160 <20 bytes> EQUAL alone is a script hash: the item is run as a script.
        let mut right = Vec::new();
        push(&mut right, &secret);
        assert_eq!(run(&with_sig(&t, 0, right), 0, &p2sh(&secret)), Err(ScriptError::UnknownOp(b'o')));
        assert_eq!(run(&t, 0, &[OP_1, OP_1, OP_EQUALVERIFY, OP_1]), Ok(()));
        assert_eq!(run(&t, 0, &[OP_1, 0x52, OP_EQUALVERIFY, OP_1]), Err(ScriptError::Verify));
        assert_eq!(run(&t, 0, &[OP_1, OP_1, OP_1, OP_2DROP]), Ok(()));
        assert_eq!(run(&t, 0, &[OP_0, OP_VERIFY, OP_1]), Err(ScriptError::Verify));
        assert_eq!(run(&t, 0, &[OP_DUP]), Err(ScriptError::StackEmpty));
    }

    fn multisig(m: u8, keys: &[&[u8]]) -> Vec<u8> {
        let mut s = vec![OP_1 - 1 + m];
        for k in keys {
            push(&mut s, k);
        }
        s.extend_from_slice(&[OP_1 - 1 + keys.len() as u8, OP_CHECKMULTISIG]);
        s
    }

    #[test]
    fn multisig_takes_signatures_in_key_order() {
        let ((a, a_pk), (b, b_pk), (c, c_pk)) = (key(1), key(2), key(3));
        let t = plain();
        let unlock = |items: &[&[u8]]| {
            let mut s = Vec::new();
            for item in items {
                push(&mut s, item);
            }
            s
        };
        // One of three, as an arcade record's data outputs are (our key first).
        let lock = multisig(1, &[&a_pk, &b_pk, &c_pk]);
        let sig = |sk: &SecretKey, lock: &[u8]| sign(&t, 0, lock, sk, SIGHASH_ALL);
        let mut checks = 0;
        assert_eq!(verify_input(&with_sig(&t, 0, unlock(&[&[], &sig(&a, &lock)])), 0, &lock, &mut checks), Ok(()));
        assert_eq!(checks, 3); // tried against the third key, the second, then the first
        assert_eq!(run(&with_sig(&t, 0, unlock(&[&[], &sig(&c, &lock)])), 0, &lock), Ok(()));
        assert_eq!(run(&with_sig(&t, 0, unlock(&[&[], &sig(&key(4).0, &lock)])), 0, &lock), Err(ScriptError::SigFailed));
        assert_eq!(run(&with_sig(&t, 0, unlock(&[&[], &[]])), 0, &lock), Err(ScriptError::NotClean));
        assert_eq!(run(&with_sig(&t, 0, unlock(&[&[1], &sig(&a, &lock)])), 0, &lock), Err(ScriptError::MultisigDummy));
        assert_eq!(run(&with_sig(&t, 0, unlock(&[&sig(&a, &lock)])), 0, &lock), Err(ScriptError::StackEmpty));

        // Two of three: in the keys' order only.
        let lock = multisig(2, &[&a_pk, &b_pk, &c_pk]);
        assert_eq!(run(&with_sig(&t, 0, unlock(&[&[], &sig(&a, &lock), &sig(&c, &lock)])), 0, &lock), Ok(()));
        assert_eq!(run(&with_sig(&t, 0, unlock(&[&[], &sig(&b, &lock), &sig(&c, &lock)])), 0, &lock), Ok(()));
        assert_eq!(run(&with_sig(&t, 0, unlock(&[&[], &sig(&c, &lock), &sig(&a, &lock)])), 0, &lock), Err(ScriptError::SigFailed));
        assert_eq!(run(&with_sig(&t, 0, unlock(&[&[], &sig(&a, &lock), &sig(&a, &lock)])), 0, &lock), Err(ScriptError::SigFailed));
        assert_eq!(run(&with_sig(&t, 0, unlock(&[&[], &sig(&a, &lock)])), 0, &lock), Err(ScriptError::StackEmpty));

        // Counts out of range.
        assert_eq!(run(&t, 0, &[OP_0, 0x52, 33, 0, OP_1, OP_CHECKMULTISIG]), Err(ScriptError::Truncated));
        let mut too_many = vec![OP_0, OP_1];
        for _ in 0..21 {
            push(&mut too_many, &a_pk);
        }
        too_many.extend_from_slice(&[1, 21, OP_CHECKMULTISIG]);
        assert_eq!(run(&t, 0, &too_many), Err(ScriptError::MultisigCount));
        let more_sigs_than_keys = [OP_0, OP_0, OP_0, 0x52, OP_1, OP_1, OP_CHECKMULTISIG];
        assert_eq!(run(&t, 0, &more_sigs_than_keys), Err(ScriptError::MultisigCount));
        assert_eq!(run(&t, 0, &[OP_0, OP_0, OP_1NEGATE, OP_CHECKMULTISIG]), Err(ScriptError::MultisigCount));
        // Each key counts as an opcode: ten multisigs of twenty keys are too many.
        let mut heavy = Vec::new();
        for _ in 0..10 {
            heavy.extend_from_slice(&[OP_0, OP_0]);
            for _ in 0..20 {
                push(&mut heavy, &a_pk);
            }
            heavy.extend_from_slice(&[1, 20, OP_CHECKMULTISIGVERIFY]);
        }
        heavy.push(OP_1);
        assert_eq!(run(&t, 0, &heavy), Err(ScriptError::TooManyOps));
    }

    #[test]
    fn a_height_lock_waits_for_its_height() {
        let (sk, pk) = key(1);
        // <height> CLTV DROP <key> CHECKSIG
        let lock_at = |height: &[u8]| {
            let mut s = Vec::new();
            push(&mut s, height);
            s.extend_from_slice(&[OP_CHECKLOCKTIMEVERIFY, OP_DROP]);
            push(&mut s, &pk);
            s.push(OP_CHECKSIG);
            s
        };
        let lock = lock_at(&[0xe8, 0x03]); // 1,000
        let spend = |lock_time: u32, sequence: u32, lock: &[u8]| {
            let t = tx(vec![(coin(1), vec![], sequence)], vec![(1, vec![OP_1])], lock_time);
            let mut s = Vec::new();
            push(&mut s, &sign(&t, 0, lock, &sk, SIGHASH_ALL));
            run(&with_sig(&t, 0, s), 0, lock)
        };
        assert_eq!(spend(1_000, 0, &lock), Ok(()));
        assert_eq!(spend(5_000, u32::MAX - 1, &lock), Ok(()));
        assert_eq!(spend(999, 0, &lock), Err(ScriptError::LockTime));
        assert_eq!(spend(0, 0, &lock), Err(ScriptError::LockTime));
        // A final sequence would switch the transaction's own lock off.
        assert_eq!(spend(1_000, u32::MAX, &lock), Err(ScriptError::LockTime));
        // Numbers: negative, padded, too long.
        assert_eq!(spend(1_000, 0, &lock_at(&[0xe8, 0x83])), Err(ScriptError::LockTime));
        assert_eq!(spend(1_000, 0, &lock_at(&[0xe8, 0x03, 0x00])), Err(ScriptError::Number));
        assert_eq!(spend(1_000, 0, &lock_at(&[1, 2, 3, 4, 5, 6])), Err(ScriptError::Number));
        assert_eq!(spend(u32::MAX, 0, &lock_at(&[0xff, 0xff, 0xff, 0xff, 0x00])), Ok(()));
        assert_eq!(number(&[], 5), Ok(0));
        assert_eq!(number(&[0x80], 5), Err(ScriptError::Number));
        assert_eq!(number(&[0x7f], 1), Ok(127));
        assert_eq!(number(&[0xff], 1), Ok(-127));
        assert_eq!(number(&[0x80, 0x00], 2), Ok(128));
        assert_eq!(number(&[0x00], 1), Err(ScriptError::Number));
    }
}
