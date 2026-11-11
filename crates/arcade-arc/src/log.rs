//! The block log: how an arcade keeps its chain across a restart.
//!
//! Every block the chain takes in, on any branch, is added to the end of one file, in the
//! order it was taken in. Nothing else is saved. At start the blocks are read back and
//! handed to a new [`Chain`](crate::chain::Chain) in that order, with every check run again,
//! so what the arcade holds after a restart is what the rules give for those blocks and
//! nothing a damaged or edited file could add: a block that no longer passes is left out.
//!
//! The file starts with a line that names block 0, so a log is never read into another
//! network's chain. A record is the block's length, its bytes, and the first four bytes of
//! their double SHA-256. Reading stops at the first record that is cut short or whose check
//! bytes are wrong (a write the power cut off), and the file is cut back to the last whole one.
//!
//! Checking every block again takes longer as the chain grows. When it takes too long, the
//! state at a checkpoint is saved beside the log; the log stays the record.

use crate::block::Block;
use crate::chain::{Chain, Storage};
use crate::time::TimeProofs;
use arcade_chain::hash::{sha256d, Hash256};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

const MAGIC: &[u8; 8] = b"ARCLOG1\n";
const HEAD: u64 = 8 + 32;
/// No block is bigger than this (two 1 MB sections and a header), so a length above it is
/// damage, not a block.
const MAX_RECORD: u32 = 4 * 1024 * 1024;

pub struct BlockLog {
    file: File,
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

fn check_bytes(block: &[u8]) -> [u8; 4] {
    sha256d(block).0[..4].try_into().expect("4 bytes")
}

impl BlockLog {
    /// Open the log of the chain whose block 0 is `genesis`, making it if it isn't there,
    /// and read back the blocks it holds, oldest first.
    pub fn open(path: impl AsRef<Path>, genesis: &Hash256) -> io::Result<(BlockLog, Vec<Block>)> {
        let mut file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        let len = file.metadata()?.len();
        if len < HEAD {
            // New, or cut off while its first line was written: nothing in it yet.
            file.set_len(0)?;
            file.write_all(MAGIC)?;
            file.write_all(&genesis.0)?;
            file.sync_all()?;
            return Ok((BlockLog { file }, Vec::new()));
        }
        let mut bytes = Vec::with_capacity(len as usize);
        file.read_to_end(&mut bytes)?;
        if &bytes[..8] != MAGIC {
            return Err(bad("not a block log"));
        }
        if bytes[8..40] != genesis.0 {
            return Err(bad("this block log is another chain's"));
        }

        let mut blocks = Vec::new();
        let mut at = HEAD as usize;
        while let Some(head) = bytes.get(at..at + 4) {
            let n = u32::from_le_bytes(head.try_into().expect("4 bytes"));
            if n > MAX_RECORD {
                break;
            }
            let end = at + 4 + n as usize;
            let (Some(body), Some(check)) = (bytes.get(at + 4..end), bytes.get(end..end + 4)) else { break };
            if check != check_bytes(body) {
                break;
            }
            let Ok(block) = Block::decode(body) else { break };
            blocks.push(block);
            at = end + 4;
        }
        if (at as u64) < len {
            file.set_len(at as u64)?;
            file.sync_all()?;
        }
        file.seek(SeekFrom::End(0))?;
        Ok((BlockLog { file }, blocks))
    }

    /// Add a block. It is on the disk when this returns.
    pub fn append(&mut self, block: &Block) -> io::Result<()> {
        let body = block.encode();
        let n = u32::try_from(body.len()).ok().filter(|n| *n <= MAX_RECORD).ok_or_else(|| bad("too big for a block"))?;
        let mut record = Vec::with_capacity(body.len() + 8);
        record.extend_from_slice(&n.to_le_bytes());
        record.extend_from_slice(&body);
        record.extend_from_slice(&check_bytes(&body));
        self.file.write_all(&record)?;
        self.file.sync_data()
    }
}

/// Hand the blocks of a log to a chain, as they were taken in the first time. The answer is
/// how many the chain no longer takes (none, unless the rules changed or the file was edited).
pub fn replay<T: TimeProofs, S: Storage>(chain: &mut Chain<T, S>, blocks: Vec<Block>) -> usize {
    // A block that was ahead of the clock when it came was only logged once it wasn't.
    blocks.into_iter().filter(|b| chain.accept(b.clone(), u64::MAX).is_err()).count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::tests::{chain, grow_to, second_best_block, LATE};
    use crate::chain::Accepted;

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("arcade-arc-log-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("blocks.log")
    }

    #[test]
    fn a_chain_comes_back_from_its_log_as_it_was() {
        let path = temp("back");
        let mut c = chain();
        let genesis = c.id_at(0).unwrap();
        let (mut log, held) = BlockLog::open(&path, &genesis).unwrap();
        assert!(held.is_empty());
        // 120 blocks, with a side block along the way that the log keeps too.
        for b in grow_to(&mut c, 50) {
            log.append(&b).unwrap();
        }
        let side = second_best_block(&c);
        for b in grow_to(&mut c, 51) {
            log.append(&b).unwrap();
        }
        assert_eq!(c.accept(side.clone(), LATE), Ok(Accepted::Side));
        log.append(&side).unwrap();
        for b in grow_to(&mut c, 120) {
            log.append(&b).unwrap();
        }
        drop(log);

        let (_, held) = BlockLog::open(&path, &genesis).unwrap();
        assert_eq!(held.len(), 121);
        let mut back = chain();
        assert_eq!(replay(&mut back, held), 0);
        assert_eq!(back.tip(), c.tip());
        assert_eq!(back.coins(), c.coins());
        assert_eq!(back.storage(), c.storage());
        assert!(back.entry(&side.id()).is_some());
    }

    #[test]
    fn a_torn_or_damaged_end_is_cut_off() {
        let path = temp("torn");
        let mut c = chain();
        let genesis = c.id_at(0).unwrap();
        let blocks = grow_to(&mut c, 6);
        let (mut log, _) = BlockLog::open(&path, &genesis).unwrap();
        let mut ends = Vec::new();
        for b in &blocks {
            log.append(b).unwrap();
            ends.push(std::fs::metadata(&path).unwrap().len());
        }
        drop(log);

        // The last write stopped halfway: five whole blocks are left, and the next block
        // goes where the torn one was.
        let f = OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(ends[5] - 7).unwrap();
        drop(f);
        let (mut log, held) = BlockLog::open(&path, &genesis).unwrap();
        assert_eq!(held, blocks[..5]);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), ends[4]);
        log.append(&blocks[5]).unwrap();
        drop(log);
        assert_eq!(BlockLog::open(&path, &genesis).unwrap().1, blocks);

        // A flipped bit inside the fourth block: the log ends before it.
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[ends[2] as usize + 40] ^= 1;
        std::fs::write(&path, &bytes).unwrap();
        let (_, held) = BlockLog::open(&path, &genesis).unwrap();
        assert_eq!(held, blocks[..3]);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), ends[2]);
    }

    #[test]
    fn a_log_belongs_to_one_chain() {
        let path = temp("other");
        let c = chain();
        let genesis = c.id_at(0).unwrap();
        drop(BlockLog::open(&path, &genesis).unwrap());
        assert!(BlockLog::open(&path, &Hash256([7; 32])).is_err());
        std::fs::write(&path, b"something else entirely, longer than the first line is").unwrap();
        assert!(BlockLog::open(&path, &genesis).is_err());
        // A file cut off inside its first line holds nothing and starts again.
        std::fs::write(&path, b"ARCLOG1\nxx").unwrap();
        let (_, held) = BlockLog::open(&path, &genesis).unwrap();
        assert!(held.is_empty());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), HEAD);
    }

    #[test]
    fn an_edited_log_adds_nothing_the_rules_refuse() {
        let mut c = chain();
        let mut blocks = grow_to(&mut c, 8);
        // Somebody rewrites block 5's time in the file (with fresh check bytes): its
        // signature no longer holds, so it and the blocks on it are left out.
        blocks[4].header.time += 1;
        let mut back = chain();
        assert_eq!(replay(&mut back, blocks), 4);
        assert_eq!(back.tip().height, 4);
    }
}
