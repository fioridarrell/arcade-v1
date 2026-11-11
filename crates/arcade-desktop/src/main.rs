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

/// Forward one request to arcaded and return its answer (off the UI thread: some wait, such as
/// realtime.poll).
#[tauri::command(async)]
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

fn pct(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// Open an app in its own window: our wrapper page with the app sandboxed in a frame inside it
/// (one window per app; opening it again brings that window forward).
#[tauri::command]
fn open_app(app: tauri::AppHandle, id: String, title: String, frame: String) -> Result<(), String> {
    use std::hash::{Hash, Hasher};
    use tauri::Manager;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut h);
    let label = format!("app-{:016x}", h.finish());
    if let Some(w) = app.get_webview_window(&label) {
        let _ = w.unminimize();
        return w.set_focus().map_err(|e| e.to_string());
    }
    if !frame.starts_with("http://127.0.0.1:") {
        return Err("apps run only from this arcade".into());
    }
    let url = format!("appwindow.html?frame={}&id={}&title={}", pct(&frame), pct(&id), pct(&title));
    tauri::WebviewWindowBuilder::new(&app, label, tauri::WebviewUrl::App(url.into()))
        .title(&title)
        .inner_size(1000.0, 720.0)
        .min_inner_size(320.0, 240.0)
        .build()
        .map(|w| heal(&w))
        .map_err(|e| e.to_string())
}

/// Open an inscription's content full size in its own window (the Explorer's ↗; Martin
/// 2026-11-10): the page straight from this arcade's server, which serves it sandboxed (an
/// opaque origin, scripts only). The window has no access to the arcade API (only the main
/// window and app windows do). One window per address; opening it again brings it forward.
#[tauri::command]
fn open_content(app: tauri::AppHandle, url: String, title: String) -> Result<(), String> {
    use std::hash::{Hash, Hasher};
    use tauri::Manager;
    let ok = url.strip_prefix("http://127.0.0.1:").and_then(|r| r.split_once('/')).is_some_and(|(port, path)| {
        let id_ok = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric());
        port.parse::<u16>().is_ok()
            && match path.split('/').collect::<Vec<_>>().as_slice() {
                ["content", id] => id_ok(id),
                ["app", chain, id] | ["ord", chain, id] => chain.bytes().all(|b| b.is_ascii_lowercase()) && id_ok(id),
                _ => false,
            }
    });
    if !ok {
        return Err("only content from this arcade opens here".into());
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut h);
    let label = format!("content-{:016x}", h.finish());
    if let Some(w) = app.get_webview_window(&label) {
        let _ = w.unminimize();
        return w.set_focus().map_err(|e| e.to_string());
    }
    let u: tauri::Url = url.parse().map_err(|e| format!("{e}"))?;
    tauri::WebviewWindowBuilder::new(&app, label, tauri::WebviewUrl::External(u))
        .title(&title)
        .inner_size(900.0, 900.0)
        .min_inner_size(240.0, 240.0)
        .build()
        .map(|w| heal(&w))
        .map_err(|e| e.to_string())
}

/// If a window's web process dies (it crashed, or the system ended it), reload the page instead
/// of leaving the window blank.
fn heal(w: &tauri::WebviewWindow) {
    #[cfg(target_os = "linux")]
    let _ = w.with_webview(|wv| {
        use webkit2gtk::WebViewExt;
        wv.inner().connect_web_process_terminated(|v, reason| {
            eprintln!("arcade-desktop: web process ended ({reason:?}); reloading the page");
            v.reload();
        });
    });
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            {
                use tauri::Manager;
                if let Some(w) = app.get_webview_window("main") {
                    heal(&w);
                }
            }
            // ARCADE_DEMO=chains: open the Chains & Coins mockup and run a sample install
            // (for screenshots).
            if std::env::var("ARCADE_DEMO").ok().as_deref() == Some("chains") {
                use tauri::Manager;
                if let Some(w) = app.get_webview_window("main") {
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_secs(2));
                        let _ = w.eval(r#"
                          document.querySelector('nav button[data-tab="chains"]').click();

                        "#);
                    });
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![api, open_app, open_content])
        .run(tauri::generate_context!())
        .expect("error while running the Arcade app");
}
