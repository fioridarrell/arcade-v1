//! `arcade`: command line for a running arcaded (whiteboard step 12).

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

const HELP: &str = "arcade — Arcade V1.0

  arcade status                  sync, tag index, mesh links
  arcade peers                   mesh links
  arcade wallet create           new wallet; shows the 12-word recovery phrase ONCE
  arcade wallet restore <words>  restore from a recovery phrase
  arcade wallet phrase           show the recovery phrase
  arcade wallet keys             tag keys, addresses, balances
  arcade tag check <tag>         is @tag free?
  arcade tag claim <tag>         claim @tag (asks the mesh for test coins if needed)
  arcade tag list                your claims and their status
  arcade faucet                  ask the mesh for test coins
  arcade events                  recent activity

  --home DIR   arcade data folder (default: $ARCADE_HOME or ~/.local/share/arcade)";

fn home(args: &[String]) -> std::path::PathBuf {
    if let Some(i) = args.iter().position(|a| a == "--home") {
        return args[i + 1].clone().into();
    }
    if let Some(h) = std::env::var_os("ARCADE_HOME") {
        return h.into();
    }
    // The installed service lives in /var/lib/arcade.
    let system = std::path::Path::new("/var/lib/arcade");
    if system.join("arcaded.sock").exists() || system.join("arcade.mdbx").exists() {
        return system.to_path_buf();
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"));
    base.join("arcade")
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let sock = home(&args).join("arcaded.sock");
    if let Some(i) = args.iter().position(|a| a == "--home") {
        args.drain(i..i + 2);
    }
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let req = match a.as_slice() {
        ["status"] => json!({"cmd": "status"}),
        ["peers"] => json!({"cmd": "peers"}),
        ["wallet", "create"] => json!({"cmd": "wallet.create"}),
        ["wallet", "restore", words @ ..] if !words.is_empty() => json!({"cmd": "wallet.create", "phrase": words.join(" ")}),
        ["wallet", "phrase"] => json!({"cmd": "wallet.phrase"}),
        ["wallet", "keys"] => json!({"cmd": "wallet.keys"}),
        ["tag", "check", t] => json!({"cmd": "tag.check", "tag": t}),
        ["tag", "claim", t] => json!({"cmd": "tag.claim", "tag": t}),
        ["tag", "list"] => json!({"cmd": "tag.list"}),
        ["faucet"] => json!({"cmd": "faucet.ask"}),
        ["events"] => json!({"cmd": "events"}),
        _ => {
            println!("{HELP}");
            return;
        }
    };
    let mut s = match UnixStream::connect(&sock) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("arcaded is not running ({}: {e})", sock.display());
            std::process::exit(2);
        }
    };
    writeln!(s, "{req}").unwrap();
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).unwrap();
    let v: Value = serde_json::from_str(&line).unwrap_or(json!({"ok": false, "error": "bad reply"}));
    if v["ok"] == true {
        if a.first() == Some(&"wallet") && a.get(1) == Some(&"create") {
            if let Some(p) = v["result"]["phrase"].as_str() {
                println!("Your recovery phrase (write it down, it is shown only now):\n\n  {p}\n");
                return;
            }
        }
        println!("{}", serde_json::to_string_pretty(&v["result"]).unwrap());
    } else {
        eprintln!("error: {}", v["error"].as_str().unwrap_or("unknown"));
        std::process::exit(1);
    }
}
