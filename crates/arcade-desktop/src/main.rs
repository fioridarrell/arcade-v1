//! Arcade V1.0 desktop app: a window over the local arcaded API (same socket as the CLI).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

fn home() -> std::path::PathBuf {
    if let Some(h) = std::env::var_os("ARCADE_HOME") {
        return h.into();
    }
    let system = std::path::Path::new("/var/lib/arcade");
    if system.join("arcaded.sock").exists() {
        return system.to_path_buf();
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"));
    base.join("arcade")
}

/// Forward one request to arcaded and return its answer.
#[tauri::command]
fn api(req: Value) -> Value {
    let sock = home().join("arcaded.sock");
    let mut s = match UnixStream::connect(&sock) {
        Ok(s) => s,
        Err(e) => return json!({"ok": false, "error": format!("the arcade service is not running ({}: {e})", sock.display())}),
    };
    if writeln!(s, "{req}").is_err() {
        return json!({"ok": false, "error": "could not talk to the arcade service"});
    }
    let mut line = String::new();
    let _ = BufReader::new(s).read_line(&mut line);
    serde_json::from_str(&line).unwrap_or(json!({"ok": false, "error": "bad reply from the arcade service"}))
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            // ARCADE_DEMO=chains: open the Chains & Coins mockup and run a sample install
            // (for screenshots).
            if std::env::var("ARCADE_DEMO").ok().as_deref() == Some("chains") {
                use tauri::Manager;
                if let Some(w) = app.get_webview_window("main") {
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_secs(2));
                        let _ = w.eval(r#"
                          document.querySelector('nav button[data-tab="chains"]').click();
                          setTimeout(() => { [...document.querySelectorAll('.chain')].find(c => c.textContent.includes('Dogecoin')).querySelector('button').click(); }, 400);
                          setTimeout(() => { document.getElementById('c-go').click(); }, 1800);
                        "#);
                    });
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![api])
        .run(tauri::generate_context!())
        .expect("error while running the Arcade app");
}
