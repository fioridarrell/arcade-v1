//! The local socket. One connection, one request. The seed never crosses it, and this process
//! opens no TCP port: the mesh and the HTTP API have nothing here to connect to.

use crate::client::{read_frame, write_frame};
use crate::engine::Engine;
use crate::seed::SeedStore;
use arcade_local::UnixListener;
use serde_json::json;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Listen until the process is stopped. Binds `home/signer/signer.sock` for this user only.
pub fn serve(home: &Path) -> Result<(), String> {
    let store = SeedStore::open(home)?;
    let _ = store.adopt(home);
    let engine = Mutex::new(Engine::new(store, Duration::from_secs(crate::unlock_secs())));
    let path = crate::socket_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        arcade_app::restrict_to_owner(parent, 0o700).map_err(|e| e.to_string())?;
    }
    arcade_local::check_socket_path(&path)?;
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).map_err(|e| format!("binding {}: {e}", path.display()))?;
    arcade_app::restrict_to_owner(&path, 0o600).map_err(|e| format!("the signer socket: {e}"))?;
    for conn in listener.incoming() {
        let mut conn = match conn {
            Ok(c) => c,
            Err(_) => continue,
        };
        if let Err(e) = handle(&mut conn, &engine) {
            let _ = write_frame(&mut conn, &json!({"ok": false, "error": e}));
        }
    }
    Ok(())
}

fn handle(conn: &mut arcade_local::UnixStream, engine: &Mutex<Engine>) -> Result<(), String> {
    let req = read_frame(conn)?;
    let op = req["op"].as_str().unwrap_or("");
    let now = Instant::now();
    let answer = match op {
        "status" => {
            let e = lock(engine)?;
            json!({"ok": true, "locked": e.locked(now), "has_wallet": e.has_wallet(), "seconds_left": e.seconds_left(now)})
        }
        "approve" => {
            let card = serde_json::from_value(req["card"].clone()).map_err(|_| "the approval card is not one this signer understands".to_string())?;
            let mut e = lock(engine)?;
            e.approve(card, now)?;
            json!({"ok": true, "locked": false, "seconds_left": e.seconds_left(now)})
        }
        "sign" => {
            let job = serde_json::from_value(req["job"].clone()).map_err(|_| "the signing request is not one this signer understands".to_string())?;
            let mut e = lock(engine)?;
            let raw = e.sign(&job, now)?;
            json!({"ok": true, "raw": hex::encode(raw), "locked": true})
        }
        "lock" => {
            lock(engine)?.lock();
            json!({"ok": true, "locked": true})
        }
        // The seed file was just rewritten by arcaded. Drop any decrypted seed so the next
        // approval reads the file. The phrase itself is not in this message.
        "adopt" => {
            let mut e = lock(engine)?;
            e.lock();
            json!({"ok": true, "has_wallet": e.has_wallet(), "locked": true})
        }
        "derive" => {
            let coin_type = req["coin_type"].as_u64().ok_or("coin_type required")? as u32;
            let index = req["index"].as_u64().unwrap_or(0) as u32;
            let version = req["version"].as_u64().ok_or("version required")? as u8;
            let (pk, address) = lock(engine)?.derive(coin_type, index, version)?;
            let locked = lock(engine)?.locked(now);
            json!({"ok": true, "pubkey": pk, "address": address, "locked": locked})
        }
        _ => return Err(format!("unknown signer request {op}")),
    };
    write_frame(conn, &answer)
}

fn lock(engine: &Mutex<Engine>) -> Result<MutexGuard<'_, Engine>, String> {
    engine.lock().map_err(|_| "the signer stopped".to_string())
}
