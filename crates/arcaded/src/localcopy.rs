//! "Take it from this folder": a chain's Core datadir on this computer (an old arcade's
//! Dogecoin Core, say) as the source of a one-time bootstrap, instead of a "cb" copy fetched
//! over the mesh (crate::coreboot, docs/compat/COREBOOT.md).
//!
//! Only blocks/blk*.dat and chainstate/ are read, in place and READ ONLY: nothing is created,
//! renamed, locked or written in the folder (crate::leveldb is a read-only reader, so the
//! chainstate needs no writable copy). Wallets, the conf file, peers and the rest are never
//! opened. The import itself is the "all" copy's (crate::cbimport): headers from the blk
//! files checked by proof of work like any other, the folder's tip on our own header chain,
//! the blocks stored, every coin checked against the transaction at its place in our blocks,
//! the set marked trusted, then derived again from the blocks in the background.
//!
//! Refused before anything is read: a Core still running on the folder (a lock held on its
//! .lock, chainstate/LOCK or blocks/index/LOCK; Core must be stopped first, its coin set is
//! rewritten while it runs), a pruned datadir, block files of another chain (magic or genesis),
//! and obfuscated block files (blocks/xor.dat, newer Bitcoin Cores; not read here).

use arcade_chain::params::ChainParams;
use arcade_chain::Hash256;
use std::path::{Path, PathBuf};

/// What a datadir holds, once checked.
#[derive(Debug, Clone)]
pub struct LocalDir {
    /// The datadir (the folder holding blocks/ and chainstate/).
    pub dir: PathBuf,
    /// The block the coin set is at (chainstate 'B').
    pub best: Hash256,
    pub layout: Option<crate::corecoins::Layout>,
    /// Number of blk*.dat files.
    pub blk_files: usize,
}

/// Normalize what the operator typed: quotes and trailing separators off; the blocks/ or
/// chainstate/ folder itself means its parent.
pub fn normalize(raw: &str) -> PathBuf {
    let t = raw.trim().trim_matches('"').trim_matches('\'');
    let t = t.trim_end_matches(['/', '\\']);
    let mut p = PathBuf::from(if t.is_empty() { raw.trim() } else { t });
    let last = p.file_name().and_then(|n| n.to_str()).map(|n| n.to_ascii_lowercase());
    if matches!(last.as_deref(), Some("blocks") | Some("chainstate")) {
        if let Some(parent) = p.parent().filter(|x| x.join("blocks").is_dir() && x.join("chainstate").is_dir()) {
            p = parent.to_path_buf();
        }
    }
    p
}

/// Whether another process holds a lock on `f` (Core's .lock, LevelDB's LOCK). The file is
/// opened for reading and only asked: no lock is kept, nothing is written. A missing file is
/// not locked.
pub fn locked(f: &Path) -> Result<bool, String> {
    let fh = match std::fs::File::open(f) {
        Ok(h) => h,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        // Windows: a sharing violation means a running process has it open exclusively.
        #[cfg(windows)]
        Err(e) if e.raw_os_error() == Some(32) || e.raw_os_error() == Some(33) => return Ok(true),
        Err(e) => return Err(format!("{}: {e}", f.display())),
    };
    #[cfg(unix)]
    {
        // Core and LevelDB lock with fcntl (POSIX record locks): ask (F_GETLK), never take.
        use std::os::unix::io::AsRawFd;
        let mut fl: libc::flock = unsafe { std::mem::zeroed() };
        fl.l_type = libc::F_WRLCK as _;
        fl.l_whence = libc::SEEK_SET as _;
        let r = unsafe { libc::fcntl(fh.as_raw_fd(), libc::F_GETLK, &mut fl) };
        if r == 0 && fl.l_type as i32 != libc::F_UNLCK as i32 {
            return Ok(true);
        }
    }
    // flock (Unix) / LockFileEx (Windows, how Core and LevelDB lock there): a shared lock
    // tried and let go at once fails while someone holds it exclusively.
    match fh.try_lock_shared() {
        Ok(()) => {
            let _ = fh.unlock();
            Ok(false)
        }
        Err(std::fs::TryLockError::WouldBlock) => Ok(true),
        Err(std::fs::TryLockError::Error(_)) => Ok(false),
    }
}

/// The lock files a running Core holds in its datadir.
pub fn lock_files(dir: &Path) -> Vec<PathBuf> {
    vec![dir.join(".lock"), dir.join("chainstate").join("LOCK"), dir.join("blocks").join("index").join("LOCK")]
}

/// Refuse while a Core node runs on the folder.
pub fn check_not_running(dir: &Path) -> Result<(), String> {
    for f in lock_files(dir) {
        if locked(&f)? {
            return Err(format!(
                "a Core node is running on {} ({} is locked). Stop it first (dogecoin-cli stop, or close Dogecoin Core / the old arcade's node) \
                 and try again: its coin set is rewritten while it runs, so it can't be read in place",
                dir.display(),
                f.display()
            ));
        }
    }
    Ok(())
}

/// Whether Core marked the block index pruned (blocks/index: 'F' + "prunedblockfiles" = '1').
/// Unknown (no index, unreadable) counts as not pruned: the genesis check and the walk from
/// the tip still find missing blocks.
pub fn pruned_flag(dir: &Path) -> bool {
    let idx = dir.join("blocks").join("index");
    if !idx.join("CURRENT").is_file() {
        return false;
    }
    let Ok(mut db) = crate::leveldb::Db::open(&idx) else { return false };
    let mut want = vec![b'F', 16];
    want.extend_from_slice(b"prunedblockfiles");
    while let Ok(Some((k, v))) = db.next_kv() {
        if k == want {
            return v.first() == Some(&b'1');
        }
        if k.first().is_some_and(|c| *c > b'F') {
            break;
        }
    }
    false
}

/// The first record of blk00000.dat: Core writes the genesis block there first.
fn first_block_id(blk0: &Path, p: &ChainParams) -> Result<Hash256, String> {
    use std::io::Read;
    let mut f = std::fs::File::open(blk0).map_err(|e| format!("{}: {e}", blk0.display()))?;
    let mut head = [0u8; 88];
    f.read_exact(&mut head).map_err(|_| format!("{} is empty or too short", blk0.display()))?;
    if head[..4] != p.magic {
        return Err(format!(
            "{} is not a {} block file (network magic {} instead of {}); is this the right folder (testnet keeps its own in testnet3/)?",
            blk0.display(),
            p.name,
            hex::encode(&head[..4]),
            hex::encode(p.magic)
        ));
    }
    Ok(p.block_id(&arcade_chain::Header::parse(head[8..88].try_into().unwrap())))
}

/// Check a Core datadir before an import: present, not in use, not pruned, this chain's.
pub fn check_datadir(dir: &Path, p: &ChainParams) -> Result<LocalDir, String> {
    if dir.as_os_str().is_empty() {
        return Err("no folder given".into());
    }
    if !dir.is_absolute() {
        return Err(format!("{}: give the folder's full path (e.g. C:\\Users\\you\\AppData\\Roaming\\Dogecoin or ~/.dogecoin as /home/you/.dogecoin)", dir.display()));
    }
    if dir.components().any(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase().starts_with("wallet")) {
        return Err(format!("{}: that is a wallet folder; give the Core datadir that holds blocks/ and chainstate/", dir.display()));
    }
    if !dir.is_dir() {
        return Err(format!("{}: no such folder", dir.display()));
    }
    let (blocks, cs) = (dir.join("blocks"), dir.join("chainstate"));
    if !blocks.is_dir() || !cs.is_dir() {
        let hint: Vec<String> = ["testnet3", "testnet4", "regtest", "signet"]
            .iter()
            .filter(|n| dir.join(n).join("blocks").is_dir())
            .map(|n| dir.join(n).display().to_string())
            .collect();
        return Err(format!(
            "{} holds no blocks/ and chainstate/: not a Core datadir{}",
            dir.display(),
            if hint.is_empty() { String::new() } else { format!(" (its test network's is {})", hint.join(", ")) }
        ));
    }
    check_not_running(dir)?;
    if let Ok(x) = std::fs::read(blocks.join("xor.dat")) {
        if x.iter().any(|b| *b != 0) {
            return Err("the block files are obfuscated (blocks/xor.dat, a newer Core): not read here".into());
        }
    }
    if pruned_flag(dir) {
        return Err(format!("{} is a pruned Core datadir: it no longer has every block, and the import needs them all", dir.display()));
    }
    let blk0 = blocks.join("blk00000.dat");
    if !blk0.is_file() {
        return Err(format!("{} lacks blocks/blk00000.dat: pruned or incomplete; the import needs every block", dir.display()));
    }
    let g = first_block_id(&blk0, p)?;
    if g != p.genesis_hash() {
        return Err(format!("the folder's first block is {g}, not {}'s genesis {}: another chain's datadir", p.name, p.genesis));
    }
    let blk_files = std::fs::read_dir(&blocks)
        .map_err(|e| e.to_string())?
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.starts_with("blk") && n.ends_with(".dat")))
        .count();
    let st = crate::corecoins::Chainstate::open(&cs).map_err(|e| format!("chainstate: {e}"))?;
    let best = st.best.ok_or("the chainstate names no best block (an empty or unfinished Core datadir)")?;
    Ok(LocalDir { dir: dir.to_path_buf(), best, layout: st.layout, blk_files })
}

/// A short id for a folder import (stands where a copy's manifest id would).
pub fn snap_id(dir: &Path, best: &Hash256) -> String {
    use sha2::Digest;
    let h = sha2::Sha256::digest(format!("{}|{}", dir.display(), best.to_hex()).as_bytes());
    format!("dir-{}", &hex::encode(h)[..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_typed_paths() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("blocks")).unwrap();
        std::fs::create_dir_all(d.path().join("chainstate")).unwrap();
        let s = d.path().display().to_string();
        assert_eq!(normalize(&format!("\"{s}\"")), d.path());
        assert_eq!(normalize(&format!("{s}/")), d.path());
        assert_eq!(normalize(&format!("{s}/blocks")), d.path());
        assert_eq!(normalize(&format!("  {s}/chainstate/ ")), d.path());
        // A Windows path keeps its drive and backslashes (only trailing ones go).
        assert_eq!(normalize(r"C:\Users\Darrell\AppData\Roaming\Dogecoin\"), PathBuf::from(r"C:\Users\Darrell\AppData\Roaming\Dogecoin"));
    }

    /// A lock held by another holder (as a running Core holds .lock and LevelDB's LOCK) is
    /// seen; once let go it isn't; asking leaves the file as it was.
    #[test]
    fn sees_a_held_lock_and_leaves_the_file_alone() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("LOCK");
        assert!(!locked(&f).unwrap(), "missing = not locked");
        std::fs::write(&f, b"").unwrap();
        let before = std::fs::metadata(&f).unwrap().modified().unwrap();
        assert!(!locked(&f).unwrap());
        let holder = std::fs::OpenOptions::new().read(true).write(true).open(&f).unwrap();
        holder.lock().unwrap();
        assert!(locked(&f).unwrap());
        holder.unlock().unwrap();
        assert!(!locked(&f).unwrap());
        assert_eq!(std::fs::metadata(&f).unwrap().modified().unwrap(), before);
        assert_eq!(std::fs::read(&f).unwrap(), b"");
    }

    /// A POSIX record lock (fcntl, how Core and LevelDB lock on Linux and macOS) held by
    /// another process is seen too.
    #[cfg(target_os = "linux")]
    #[test]
    fn sees_a_record_lock_of_another_process() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join(".lock");
        std::fs::write(&f, b"").unwrap();
        // An open-file-description lock: owned by the open file, not the process, so it
        // conflicts with a record lock this process asks about, as Core's would.
        use std::os::unix::io::AsRawFd;
        let holder = std::fs::OpenOptions::new().read(true).write(true).open(&f).unwrap();
        let mut fl: libc::flock = unsafe { std::mem::zeroed() };
        fl.l_type = libc::F_WRLCK as _;
        fl.l_whence = libc::SEEK_SET as _;
        assert_eq!(unsafe { libc::fcntl(holder.as_raw_fd(), libc::F_OFD_SETLK, &mut fl) }, 0);
        assert!(locked(&f).unwrap());
        drop(holder);
        assert!(!locked(&f).unwrap());
    }

    #[test]
    fn refuses_what_is_not_a_datadir() {
        let p = &arcade_chain::params::DOGECOIN_MAIN;
        let d = tempfile::tempdir().unwrap();
        assert!(check_datadir(Path::new("relative/dir"), p).unwrap_err().contains("full path"));
        assert!(check_datadir(&d.path().join("nope"), p).unwrap_err().contains("no such folder"));
        assert!(check_datadir(&d.path().join("wallets"), p).unwrap_err().contains("wallet"));
        std::fs::create_dir_all(d.path().join("testnet3").join("blocks")).unwrap();
        let e = check_datadir(d.path(), p).unwrap_err();
        assert!(e.contains("not a Core datadir") && e.contains("testnet3"), "{e}");
        std::fs::create_dir_all(d.path().join("blocks")).unwrap();
        std::fs::create_dir_all(d.path().join("chainstate")).unwrap();
        assert!(check_datadir(d.path(), p).unwrap_err().contains("blk00000.dat"));
        // Another network's block file.
        let mut rec = vec![0xfa, 0xbf, 0xb5, 0xda];
        rec.extend_from_slice(&81u32.to_le_bytes());
        rec.extend_from_slice(&[0u8; 81]);
        std::fs::write(d.path().join("blocks").join("blk00000.dat"), &rec).unwrap();
        assert!(check_datadir(d.path(), p).unwrap_err().contains("network magic"));
        // Right magic, wrong genesis.
        rec[..4].copy_from_slice(&p.magic);
        std::fs::write(d.path().join("blocks").join("blk00000.dat"), &rec).unwrap();
        assert!(check_datadir(d.path(), p).unwrap_err().contains("another chain"));
        // Obfuscated block files.
        std::fs::write(d.path().join("blocks").join("xor.dat"), [1u8; 8]).unwrap();
        assert!(check_datadir(d.path(), p).unwrap_err().contains("obfuscated"));
    }

    #[test]
    fn reads_the_pruned_flag() {
        use crate::leveldb::write;
        let d = tempfile::tempdir().unwrap();
        let idx = d.path().join("blocks").join("index");
        assert!(!pruned_flag(d.path()), "no index: unknown, not pruned");
        let mut k = vec![b'F', 16];
        k.extend_from_slice(b"prunedblockfiles");
        for (v, want) in [(b"0", false), (b"1", true)] {
            let _ = std::fs::remove_dir_all(&idx);
            let ops = vec![(b"Ftxindex_other".to_vec(), Some(b"1".to_vec())), (k.clone(), Some(v.to_vec())), (b"b\x01\x02".to_vec(), Some(vec![0]))];
            write::folder(&idx, &[], 3, &[write::batch(1, &ops)]);
            assert_eq!(pruned_flag(d.path()), want);
        }
    }
}
