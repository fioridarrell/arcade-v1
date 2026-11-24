//! The client arcaded uses to ask arcade-signer to sign.
//!
//! The seed stays in that process. This one sends the unsigned transaction over the local
//! socket and broadcasts whatever comes back. A locked signer, or a transaction that is not the
//! card the desktop approved, comes back as an error and nothing is signed here instead.

use arcade_chain::params::ChainParams;
use arcade_signer::{Client, SignJob};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// Connect, and start the signer if it is not already listening. A missing binary is reported
/// and left for the signing call to fail closed: arcaded does not sign the card itself.
pub fn ensure_running(home: &Path) {
    if Client::connect(home).is_ok() {
        return;
    }
    let exe = signer_exe();
    match Command::new(&exe).arg("--home").arg(home).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::inherit()).spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            let until = Instant::now() + Duration::from_secs(3);
            while Instant::now() < until {
                if Client::connect(home).is_ok() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            eprintln!("arcaded: arcade-signer did not open {}", arcade_signer::socket_path(home).display());
        }
        Err(e) => eprintln!("arcaded: arcade-signer ({}) did not start: {e}. Approving a send stays locked until it is running.", exe.display()),
    }
}

/// Write the phrase into the signer's file and, if that changed anything, tell a running signer
/// to drop the seed it has in memory. Ignored when the signer is not running. The database is
/// not opened here: the caller already has it open.
pub fn publish(home: &Path, account: &str, phrase: &str) {
    let changed = arcade_signer::import_account(home, account, phrase).unwrap_or(false);
    if changed {
        let _ = Client::connect(home).and_then(|mut c| c.adopt());
    }
}

pub fn sign(home: &Path, job: &SignJob) -> Result<Vec<u8>, String> {
    Client::connect(home)?.sign(job)
}

pub fn derive(home: &Path, p: &ChainParams, index: u32) -> Result<(String, String), String> {
    Client::connect(home)?.derive(p.bip44_coin_type, index, p.p2pkh_version)
}

fn signer_exe() -> std::path::PathBuf {
    let name = if cfg!(windows) { "arcade-signer.exe" } else { "arcade-signer" };
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let beside = dir.join(name);
            if beside.is_file() {
                return beside;
            }
        }
    }
    std::path::PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcade_signer::{Card, JobIn, JobOut, ShownOutput};
    use std::path::Path;
    use std::sync::Mutex;

    const PHRASE: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    fn serve_at(home: &Path) {
        let owned = home.to_path_buf();
        std::thread::spawn(move || {
            let _ = arcade_signer::serve(&owned);
        });
        let until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < until {
            if Client::connect(home).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the signer did not start");
    }

    struct Running {
        dir: tempfile::TempDir,
    }

    fn running() -> Running {
        let dir = tempfile::tempdir().unwrap();
        arcade_signer::import_phrase(dir.path(), PHRASE).unwrap();
        serve_at(dir.path());
        Running { dir }
    }

    fn coins(amount: u64, fee: u64, to_index: u32) -> (SignJob, Card) {
        let p = &arcade_chain::params::PEPECOIN_TEST;
        let seed = arcade_wallet::keys::seed_from_phrase(PHRASE).unwrap();
        let mine = arcade_wallet::keys::tag_key(&seed[..], p.bip44_coin_type, 0).unwrap();
        let mine_script = arcade_wallet::keys::p2pkh_script_for(&mine.public());
        let to = arcade_wallet::keys::tag_key(&seed[..], p.bip44_coin_type, to_index).unwrap();
        let to_script = arcade_wallet::keys::p2pkh_script_for(&to.public());
        let to_addr = arcade_wallet::keys::p2pkh_address(&to.public(), p.p2pkh_version);
        let outputs = vec![
            JobOut { value: amount, script: hex::encode(&to_script) },
            JobOut { value: 50_000_000 - amount - fee, script: hex::encode(&mine_script) },
        ];
        let job = SignJob {
            chain: p.key.into(),
            index: 0,
            kind: "coins".into(),
            to: to_addr.clone(),
            amount,
            fee,
            time: 1_700_000_000,
            inputs: vec![JobIn { txid: hex::encode([9u8; 32]), vout: 1, value: 50_000_000, script: hex::encode(&mine_script) }],
            outputs: outputs.clone(),
        };
        let card = Card {
            to: to_addr,
            amount,
            fee,
            outputs: outputs
                .iter()
                .map(|o| ShownOutput {
                    address: arcade_ledger::address::address_of(&hex::decode(&o.script).unwrap(), p),
                    value: o.value,
                    script: Some(o.script.clone()),
                })
                .collect(),
        };
        (job, card)
    }

    /// One signer at a time in this process: the tests each have their own home, so this only
    /// keeps their startup from stacking.
    static ONE: Mutex<()> = Mutex::new(());

    #[test]
    fn arcaded_cannot_sign_while_the_signer_is_locked() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let t = running();
        let (job, card) = coins(10_000_000, 1_000_000, 1);
        let err = sign(t.dir.path(), &job).unwrap_err();
        assert!(err.contains("locked"), "{err}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(arcade_signer::socket_path(t.dir.path())).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "only this user can open the signer socket");
        }
        Client::connect(t.dir.path()).unwrap().approve(&card).unwrap();
        let raw = sign(t.dir.path(), &job).unwrap();
        assert!(raw.len() > 60, "the approved transaction is signed");
        let err = sign(t.dir.path(), &job).unwrap_err();
        assert!(err.contains("locked"), "signing locks the signer again: {err}");
    }

    #[test]
    fn a_changed_transaction_is_refused() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let t = running();
        let (job, card) = coins(10_000_000, 1_000_000, 1);
        let mut client = Client::connect(t.dir.path()).unwrap();
        client.approve(&card).unwrap();

        let mut amount = job.clone();
        amount.amount += 1;
        amount.outputs[0].value += 1;
        amount.outputs[1].value -= 1;
        let err = sign(t.dir.path(), &amount).unwrap_err();
        assert!(err.contains("amount"), "{err}");

        let mut address = job.clone();
        let other = coins(10_000_000, 1_000_000, 2).0;
        address.to = other.to;
        address.outputs[0] = other.outputs[0].clone();
        let err = sign(t.dir.path(), &address).unwrap_err();
        assert!(err.contains("address"), "{err}");

        let mut fee = job.clone();
        fee.fee -= 1;
        fee.outputs[1].value += 1;
        let err = sign(t.dir.path(), &fee).unwrap_err();
        assert!(err.contains("fee"), "{err}");

        // The approval is still there: the matching transaction signs, and then the signer locks.
        let raw = sign(t.dir.path(), &job).unwrap();
        assert!(raw.len() > 60);
        assert!(Client::connect(t.dir.path()).unwrap().status().unwrap()["locked"] == true);
    }
}
