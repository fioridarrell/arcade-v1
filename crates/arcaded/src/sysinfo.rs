//! Shared system info (opt-in): the machine an arcade runs on, told to the mesh so it can show real figures for the
//! machines behind it instead of a guess.
//!
//! - **The switch:** `sysinfo.set {on}` (Admin), `sysinfo.status` (Read: whether it is on, and the exact record this
//!   arcade sends). Off by default, kept in the arcade's settings; no release turns it on.
//! - **The record** (`{t:"si", v:1, node, at, cpu, cores, ram_gb, gpus, disk_gb, os, arch, version, seal?, sig}`):
//!   rounded (RAM to the GB, VRAM to the GB, disk to 10 GB, 100 GB above 1 TB, seal speed to 2 figures), names
//!   cleaned (brand and model only), signed with the mesh key, at most 1 KB. Never a host name, user name, path,
//!   serial number, MAC or IP address: the names are cut to plain words, and every word that could be one of those
//!   is left out ([`clean_name`]). A field that can't be read is left out, never guessed.
//! - **Seal speed** is the only *measured* figure: core-seconds per GiB from this arcade's own Sloth seals
//!   (arcseal.rs calls [`note_seal`]), absent until it has sealed 64 MiB. Everything else is *reported*.
//! - **How it travels:** a "si" frame on every link at link-up, passed on across the mesh when it is newer than what
//!   an arcade had (as the signed hold reports are, shares::on_hold), with per-arcade and per-link limits. Sent once
//!   at start (after the first seal speed, or 10 min after start without one), when a rounded value changes, and
//!   hourly. Switching off sends `{t:"si", v:1, node, at, off:true, sig}`, and every arcade drops that machine at once.
//!   Newest `at` wins; a record is dropped 24 h after its `at`, and one with a bad signature or an `at` more than
//!   10 min ahead is never kept.
//! - **Never on a chain**, and nothing that pays or weighs reads it.

use crate::node::Node;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// A record's most bytes (compact JSON, signature included).
pub const RECORD_MAX: usize = 1024;
/// A record is dropped this long after its `at` (one fresher replaces it, hourly).
pub const KEEP: u64 = 24 * 3600;
/// A record whose `at` is further ahead than this is never kept.
pub const FUTURE: u64 = 600;
/// Seal speed is told once this many bytes were sealed here.
pub const SEAL_MIN_BYTES: u64 = 64 << 20;
/// The seal speed is worked out over about the last this many bytes sealed.
const SEAL_WINDOW: u64 = 1 << 30;
/// Sent again at least this often while on.
const EVERY: u64 = 3600;
/// A changed figure is sent at most this often.
const CHANGE_GAP: u64 = 600;
/// Sent this long after start when no seal speed is known by then.
const START_WAIT: u64 = 600;
/// The most a name (CPU, GPU) keeps.
const NAME_MAX: usize = 48;
/// GPUs told, at most.
const GPUS_MAX: usize = 8;
/// Arcades' records kept, at most (node ids cost nothing to make).
const HEARD_MAX: usize = 2048;
/// An arcade's records are taken at most once in this long (an off record always).
const ORIGIN_GAP: u64 = 60;
/// Records one link may bring per LINK_WINDOW.
const PER_LINK: usize = 600;
const LINK_WINDOW: u64 = 600;

const GIB: f64 = (1u64 << 30) as f64;

// ---------------------------------------------------------------- what is read from the machine

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Gpu {
    pub name: String,
    pub vram_bytes: Option<u64>,
    /// Its memory is the machine's own (GB10, Apple silicon): already in the RAM, never counted twice.
    pub unified: bool,
}

/// The machine as read, before rounding and cleaning.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Facts {
    pub cpu: Option<String>,
    pub physical: Option<u32>,
    pub logical: Option<u32>,
    pub ram_bytes: Option<u64>,
    pub gpus: Vec<Gpu>,
    /// Total and free bytes on the volume holding the arcade's data.
    pub disk: Option<(u64, u64)>,
}

/// Read the machine (the data volume is the one holding `home`).
pub fn gather(home: &std::path::Path) -> Facts {
    let mut f = os::gather();
    f.disk = crate::diskfree::space(home);
    // NVIDIA's own names and memory, in place of the bare "NVIDIA GPU" the bus gives.
    let mut nv = nvidia_smi();
    if !nv.is_empty() {
        f.gpus.retain(|g| !g.name.to_ascii_lowercase().contains("nvidia"));
        nv.append(&mut f.gpus);
        f.gpus = nv;
        f.gpus.truncate(GPUS_MAX);
    }
    f
}

/// NVIDIA GPUs, from `nvidia-smi --query-gpu=name,memory.total` (none when it isn't there or doesn't answer in 10 s).
fn nvidia_smi() -> Vec<Gpu> {
    let Ok(mut child) = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=name,memory.total", "--format=csv,noheader,nounits"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return Vec::new();
    };
    let t = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if t.elapsed() < Duration::from_secs(10) => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Vec::new();
            }
        }
    }
    let mut out = String::new();
    if let Some(mut s) = child.stdout.take() {
        let _ = std::io::Read::read_to_string(&mut s, &mut out);
    }
    parse_nvidia_smi(&out, std::env::consts::ARCH)
}

/// `nvidia-smi` CSV lines: "NVIDIA GeForce RTX 4090, 24564" (MiB). A GPU without its own memory figure on an ARM
/// machine ("NVIDIA GB10, [N/A]") shares the machine's memory: unified.
pub fn parse_nvidia_smi(out: &str, arch: &str) -> Vec<Gpu> {
    let mut v = Vec::new();
    for line in out.lines().take(GPUS_MAX) {
        let Some((name, mem)) = line.rsplit_once(',') else { continue };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let mib = mem.trim().parse::<u64>().ok();
        v.push(Gpu { name: name.to_string(), vram_bytes: mib.map(|m| m << 20), unified: mib.is_none() && arch == "aarch64" });
    }
    v
}

#[cfg(target_os = "linux")]
mod os {
    use super::{Facts, Gpu};
    use std::collections::BTreeSet;

    pub fn gather() -> Facts {
        let info = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
        let mut f = Facts { cpu: cpu_name(&info), ..Default::default() };
        let logical = info.lines().filter(|l| l.starts_with("processor")).count() as u32;
        f.logical = (logical > 0).then_some(logical).or_else(|| std::thread::available_parallelism().ok().map(|n| n.get() as u32));
        // A core per distinct set of hardware threads.
        let mut cores = BTreeSet::new();
        if let Ok(rd) = std::fs::read_dir("/sys/devices/system/cpu") {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if n.strip_prefix("cpu").is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit())) {
                    if let Ok(s) = std::fs::read_to_string(e.path().join("topology/thread_siblings_list")) {
                        cores.insert(s.trim().to_string());
                    }
                }
            }
        }
        f.physical = if cores.is_empty() { f.logical } else { Some(cores.len() as u32) };
        let mem = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
        f.ram_bytes = mem.lines().find_map(|l| l.strip_prefix("MemTotal:")).and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok()).map(|kb| kb * 1024);
        f.gpus = drm_gpus();
        f
    }

    /// The CPU's model name: x86's "model name", else the ARM cores by their part numbers ("ARM Cortex-X925 +
    /// Cortex-A725").
    pub fn cpu_name(info: &str) -> Option<String> {
        let field = |l: &str, k: &str| l.split_once(':').filter(|(a, _)| a.trim() == k).map(|(_, b)| b.trim().to_string());
        if let Some(m) = info.lines().find_map(|l| field(l, "model name")).filter(|m| !m.is_empty()) {
            return Some(m);
        }
        let mut imp = String::new();
        let mut parts: Vec<String> = Vec::new();
        for l in info.lines() {
            if let Some(i) = field(l, "CPU implementer") {
                imp = i;
            } else if let Some(p) = field(l, "CPU part") {
                if let Some(n) = arm_part(&imp, &p) {
                    if !parts.contains(&n) {
                        parts.push(n);
                    }
                }
            }
        }
        if parts.is_empty() {
            return None;
        }
        let brand = if imp == "0x41" { "ARM " } else { "" };
        Some(format!("{brand}{}", parts.join(" + ")))
    }

    fn arm_part(imp: &str, part: &str) -> Option<String> {
        let n = match (imp, part) {
            ("0x41", "0xd03") => "Cortex-A53",
            ("0x41", "0xd04") => "Cortex-A35",
            ("0x41", "0xd05") => "Cortex-A55",
            ("0x41", "0xd07") => "Cortex-A57",
            ("0x41", "0xd08") => "Cortex-A72",
            ("0x41", "0xd09") => "Cortex-A73",
            ("0x41", "0xd0a") => "Cortex-A75",
            ("0x41", "0xd0b") => "Cortex-A76",
            ("0x41", "0xd0c") => "Neoverse-N1",
            ("0x41", "0xd0d") => "Cortex-A77",
            ("0x41", "0xd40") => "Neoverse-V1",
            ("0x41", "0xd41") => "Cortex-A78",
            ("0x41", "0xd44") => "Cortex-X1",
            ("0x41", "0xd46") => "Cortex-A510",
            ("0x41", "0xd47") => "Cortex-A710",
            ("0x41", "0xd48") => "Cortex-X2",
            ("0x41", "0xd49") => "Neoverse-N2",
            ("0x41", "0xd4d") => "Cortex-A715",
            ("0x41", "0xd4e") => "Cortex-X3",
            ("0x41", "0xd4f") => "Neoverse-V2",
            ("0x41", "0xd80") => "Cortex-A520",
            ("0x41", "0xd81") => "Cortex-A720",
            ("0x41", "0xd82") => "Cortex-X4",
            ("0x41", "0xd84") => "Neoverse-V3",
            ("0x41", "0xd85") => "Cortex-X925",
            ("0x41", "0xd87") => "Cortex-A725",
            _ => return None,
        };
        Some(n.to_string())
    }

    /// GPUs on the PCI bus by vendor (NVIDIA's names and memory come from nvidia-smi instead, when it is there).
    fn drm_gpus() -> Vec<Gpu> {
        let mut seen = BTreeSet::new();
        let mut v = Vec::new();
        let Ok(rd) = std::fs::read_dir("/sys/class/drm") else { return v };
        let mut names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        names.sort();
        for n in names {
            if !n.strip_prefix("card").is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit())) {
                continue;
            }
            let dev = std::path::Path::new("/sys/class/drm").join(&n).join("device");
            let Ok(vendor) = std::fs::read_to_string(dev.join("vendor")) else { continue };
            // One GPU with several cards (render and display): counted once.
            let Ok(real) = std::fs::canonicalize(&dev) else { continue };
            if !seen.insert(real) {
                continue;
            }
            let vram = std::fs::read_to_string(dev.join("mem_info_vram_total")).ok().and_then(|s| s.trim().parse::<u64>().ok()).filter(|b| *b > 0);
            let name = match vendor.trim() {
                "0x1002" => "AMD GPU",
                "0x8086" => "Intel GPU",
                "0x10de" => "NVIDIA GPU",
                _ => continue,
            };
            v.push(Gpu { name: name.into(), vram_bytes: vram, unified: false });
            if v.len() >= super::GPUS_MAX {
                break;
            }
        }
        v
    }
}

#[cfg(target_os = "macos")]
mod os {
    use super::{Facts, Gpu};

    fn sysctl(k: &str) -> Option<String> {
        let o = std::process::Command::new("sysctl").args(["-n", k]).stderr(std::process::Stdio::null()).output().ok()?;
        let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
        (o.status.success() && !s.is_empty()).then_some(s)
    }

    pub fn gather() -> Facts {
        let n = |k: &str| sysctl(k).and_then(|s| s.parse::<u64>().ok());
        let cpu = sysctl("machdep.cpu.brand_string");
        let mut f = Facts {
            physical: n("hw.physicalcpu").map(|x| x as u32),
            logical: n("hw.logicalcpu").map(|x| x as u32),
            ram_bytes: n("hw.memsize"),
            ..Default::default()
        };
        // Apple silicon: the GPU is on the chip and shares its memory.
        if std::env::consts::ARCH == "aarch64" {
            if let Some(c) = cpu.as_deref().filter(|c| c.starts_with("Apple")) {
                f.gpus.push(Gpu { name: format!("{c} GPU"), vram_bytes: None, unified: true });
            }
        }
        f.cpu = cpu;
        f
    }
}

#[cfg(windows)]
mod os {
    use super::Facts;

    #[repr(C)]
    struct MemoryStatusEx {
        length: u32,
        memory_load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended_virtual: u64,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalMemoryStatusEx(buf: *mut MemoryStatusEx) -> i32;
    }
    #[link(name = "advapi32")]
    extern "system" {
        fn RegGetValueW(key: isize, sub: *const u16, value: *const u16, flags: u32, kind: *mut u32, data: *mut u8, len: *mut u32) -> i32;
    }
    const HKEY_LOCAL_MACHINE: isize = 0x8000_0002u32 as i32 as isize;
    const RRF_RT_REG_SZ: u32 = 0x2;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn cpu_name() -> Option<String> {
        let sub = wide("HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0");
        let val = wide("ProcessorNameString");
        let mut buf = [0u16; 256];
        let mut len = (buf.len() * 2) as u32;
        let r = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, sub.as_ptr(), val.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr() as *mut u8, &mut len) };
        if r != 0 {
            return None;
        }
        let n = (len as usize / 2).min(buf.len());
        let s = String::from_utf16_lossy(&buf[..n]).trim_end_matches('\0').trim().to_string();
        (!s.is_empty()).then_some(s)
    }

    pub fn gather() -> Facts {
        let mut m = MemoryStatusEx {
            length: std::mem::size_of::<MemoryStatusEx>() as u32,
            memory_load: 0,
            total_phys: 0,
            avail_phys: 0,
            total_page_file: 0,
            avail_page_file: 0,
            total_virtual: 0,
            avail_virtual: 0,
            avail_extended_virtual: 0,
        };
        let ram = (unsafe { GlobalMemoryStatusEx(&mut m) } != 0 && m.total_phys > 0).then_some(m.total_phys);
        // Physical cores aren't read here: left out.
        Facts { cpu: cpu_name(), logical: std::thread::available_parallelism().ok().map(|n| n.get() as u32), ram_bytes: ram, ..Default::default() }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod os {
    pub fn gather() -> super::Facts {
        super::Facts { logical: std::thread::available_parallelism().ok().map(|n| n.get() as u32), ..Default::default() }
    }
}

// ---------------------------------------------------------------- cleaning and rounding

/// The words that would name this machine or its owner (host name, user names): never in a record.
pub fn private_words() -> Vec<String> {
    let mut v = Vec::new();
    for k in ["USER", "USERNAME", "LOGNAME", "COMPUTERNAME", "HOSTNAME"] {
        if let Ok(s) = std::env::var(k) {
            v.push(s);
        }
    }
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        if unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } == 0 {
            let n = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
            v.push(String::from_utf8_lossy(&buf[..n]).to_string());
        }
    }
    let mut out = Vec::new();
    for s in v {
        // The whole name and each part of it ("gx10-0e7e" and "0e7e"), as the name is cut into words below.
        let s = s.trim().to_ascii_lowercase();
        if s.len() >= 2 {
            out.push(s.clone());
        }
        for p in s.split(|c: char| !c.is_ascii_alphanumeric()) {
            if p.len() >= 3 && !out.iter().any(|o| o == p) {
                out.push(p.to_string());
            }
        }
    }
    out
}

/// A CPU or GPU model name, brand and model only: no frequency, stepping or microcode, no "(R)" or "(TM)", and none
/// of the words that could carry something about the machine or its owner: anything with a path, address or
/// e-mail sign in it ("/", "\", ".", ":", "@", "=", "_"), long numbers, hex strings (serials) and MAC addresses, and the host and
/// user names (`private`). At most 48 characters, cut between words. None when nothing is left.
pub fn clean_name(raw: &str, private: &[String]) -> Option<String> {
    let mut s = raw.replace("(R)", " ").replace("(r)", " ").replace("(TM)", " ").replace("(tm)", " ").replace(['®', '™'], " ");
    // Everything from a frequency, a stepping, a microcode version or a serial on is left off.
    let lower = s.to_ascii_lowercase();
    let cut = [" @ ", " with ", " stepping", " microcode", " revision", " rev ", " serial", " s/n", " sn:", " sn ", " family ", " model "]
        .iter()
        .filter_map(|k| lower.find(k))
        .min();
    if let Some(c) = cut {
        s.truncate(c);
    }
    let mut words: Vec<&str> = Vec::new();
    for w in s.split_whitespace() {
        let w = w.trim_matches(|c: char| c == ',' || c == ';');
        let lw = w.to_ascii_lowercase();
        if w.is_empty() || matches!(lw.as_str(), "cpu" | "processor" | "core(s)") {
            continue;
        }
        // "16-Core", "8-core"
        if lw.strip_suffix("-core").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())) {
            continue;
        }
        if !w.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '+' | '(' | ')')) {
            continue;
        }
        // A MAC address written with dashes ("aa-bb-cc-dd-ee-ff").
        let groups: Vec<&str> = w.split('-').collect();
        if groups.len() >= 4 && groups.iter().all(|g| !g.is_empty() && g.len() <= 4 && g.bytes().all(|b| b.is_ascii_hexdigit())) {
            continue;
        }
        let digits = w.bytes().filter(|b| b.is_ascii_digit()).count();
        // A serial: a long run with digits in it, or a hex string of 8 or more.
        if digits > 0 && (w.len() >= 12 || (w.len() >= 8 && w.bytes().all(|b| b.is_ascii_hexdigit()))) {
            continue;
        }
        if private.iter().any(|p| lw == *p || (p.len() >= 4 && lw.contains(p.as_str()))) {
            continue;
        }
        words.push(w);
    }
    let mut out = String::new();
    for w in words {
        let more = if out.is_empty() { w.len() } else { out.len() + 1 + w.len() };
        if more > NAME_MAX {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    let out = out.trim_matches(|c: char| c == '-' || c == '+').trim().to_string();
    (!out.is_empty() && out.chars().any(|c| c.is_ascii_alphabetic())).then_some(out)
}

/// Bytes to the nearest GiB (RAM, VRAM).
pub fn round_gb(bytes: u64) -> u64 {
    (bytes as f64 / GIB).round() as u64
}

/// Disk bytes to GB: the nearest 10 GB, the nearest 100 GB from 1 TB up.
pub fn round_disk_gb(bytes: u64) -> u64 {
    let gb = bytes as f64 / 1e9;
    let step = if gb >= 1000.0 { 100.0 } else { 10.0 };
    ((gb / step).round() * step) as u64
}

/// Two significant figures.
pub fn sig2(x: f64) -> Option<f64> {
    if !x.is_finite() || x <= 0.0 {
        return None;
    }
    let e = x.log10().floor() as i32 - 1;
    let f = 10f64.powi(e);
    let r = (x / f).round() * f;
    // Printed short ("3200", "0.93"), not as 3200.0000000000005.
    Some(format!("{r:.*}", (-e).max(0) as usize).parse().unwrap_or(r))
}

/// This arcade's version, as records carry it.
pub fn version() -> String {
    crate::updater::version().map(String::from).unwrap_or_else(|| format!("{}-local", env!("CARGO_PKG_VERSION")))
}

/// The record (unsigned) for `facts`: rounded and cleaned, fitted under RECORD_MAX with room for the signature
/// (GPUs, then the CPU name, give way first).
pub fn record(node: &str, at: u64, f: &Facts, version: &str, seal: Option<f64>, private: &[String]) -> Value {
    let mut r = json!({"t": "si", "v": 1, "node": node, "at": at, "os": std::env::consts::OS, "arch": std::env::consts::ARCH});
    if version.len() <= 40 && version.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+')) {
        r["version"] = json!(version);
    }
    if let Some(c) = f.cpu.as_deref().and_then(|c| clean_name(c, private)) {
        r["cpu"] = json!(c);
    }
    let mut cores = Map::new();
    if let Some(p) = f.physical.filter(|n| *n > 0 && *n <= 65_536) {
        cores.insert("physical".into(), json!(p));
    }
    if let Some(l) = f.logical.filter(|n| *n > 0 && *n <= 65_536) {
        cores.insert("logical".into(), json!(l));
    }
    if !cores.is_empty() {
        r["cores"] = Value::Object(cores);
    }
    if let Some(b) = f.ram_bytes.filter(|b| *b > 0) {
        r["ram_gb"] = json!(round_gb(b));
    }
    let gpus: Vec<Value> = f
        .gpus
        .iter()
        .filter_map(|g| {
            let name = clean_name(&g.name, private)?;
            let mut v = json!({"name": name});
            if g.unified {
                v["unified"] = json!(true);
            } else if let Some(b) = g.vram_bytes.filter(|b| *b > 0) {
                v["vram_gb"] = json!(round_gb(b));
            }
            Some(v)
        })
        .take(GPUS_MAX)
        .collect();
    if !gpus.is_empty() {
        r["gpus"] = json!(gpus);
    }
    if let Some((total, free)) = f.disk.filter(|(t, _)| *t > 0) {
        r["disk_gb"] = json!({"total": round_disk_gb(total), "free": round_disk_gb(free.min(total))});
    }
    if let Some(s) = seal.and_then(sig2) {
        r["seal"] = json!(s);
    }
    // The signature adds `,"sig":"<128 hex>"`.
    let fits = |r: &Value| r.to_string().len() + 137 <= RECORD_MAX;
    while !fits(&r) {
        let n = r["gpus"].as_array().map(|a| a.len()).unwrap_or(0);
        if n > 0 {
            if n == 1 {
                r.as_object_mut().unwrap().remove("gpus");
            } else {
                r["gpus"].as_array_mut().unwrap().pop();
            }
        } else if r.get("cpu").is_some() {
            r.as_object_mut().unwrap().remove("cpu");
        } else {
            break;
        }
    }
    r
}

/// The record that says this arcade stopped sharing (unsigned).
pub fn off_record(node: &str, at: u64) -> Value {
    json!({"t": "si", "v": 1, "node": node, "at": at, "off": true})
}

fn is_hex(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn name_ok(v: &Value) -> bool {
    v.as_str().is_some_and(|s| !s.is_empty() && s.len() <= NAME_MAX && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '+' | '(' | ')')))
}

fn small(v: &Value, max: u64) -> bool {
    v.as_u64().is_some_and(|n| n <= max)
}

/// Whether a record as heard is one this arcade would have made: only its fields, each in its form and size, at
/// most RECORD_MAX. Anything else is dropped whole (a record passed on must stay as it was signed).
pub fn well_formed(r: &Value) -> Result<(), &'static str> {
    let o = r.as_object().ok_or("not an object")?;
    if serde_json::to_string(r).map(|s| s.len()).unwrap_or(usize::MAX) > RECORD_MAX {
        return Err("too big");
    }
    if r["t"] != "si" || r["v"] != 1 {
        return Err("not a v1 record");
    }
    if !r["node"].as_str().is_some_and(|n| is_hex(n, 64)) || !r["sig"].as_str().is_some_and(|s| is_hex(s, 128)) || r["at"].as_u64().is_none() {
        return Err("no node, time or signature");
    }
    if r.get("off").is_some() {
        if r["off"] != true || o.keys().any(|k| !matches!(k.as_str(), "t" | "v" | "node" | "at" | "off" | "sig")) {
            return Err("an off record carries nothing else");
        }
        return Ok(());
    }
    for (k, v) in o {
        let ok = match k.as_str() {
            "t" | "v" | "node" | "at" | "sig" => true,
            "cpu" => name_ok(v),
            "os" | "arch" => v.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 16 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')),
            "version" => v.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 40 && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))),
            "cores" => v.as_object().is_some_and(|c| !c.is_empty() && c.iter().all(|(k, n)| matches!(k.as_str(), "physical" | "logical") && small(n, 65_536))),
            "ram_gb" => small(v, 1 << 24),
            "disk_gb" => v.as_object().is_some_and(|d| d.len() == 2 && small(&d["total"], 1 << 32) && small(&d["free"], 1 << 32)),
            "gpus" => v.as_array().is_some_and(|a| {
                a.len() <= GPUS_MAX
                    && a.iter().all(|g| {
                        g.as_object().is_some_and(|m| {
                            name_ok(&m["name"])
                                && m.iter().all(|(k, x)| match k.as_str() {
                                    "name" => true,
                                    "vram_gb" => small(x, 1 << 20),
                                    "unified" => *x == true,
                                    _ => false,
                                })
                        })
                    })
            }),
            "seal" => v.as_f64().is_some_and(|s| s > 0.0 && s < 1e12),
            _ => false,
        };
        if !ok {
            return Err("a field out of form");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- what this arcade heard

struct Entry {
    rec: Value,
    at: u64,
    got: u64,
    via: String,
}

/// Every other arcade's newest record, and the off records that dropped one.
#[derive(Default)]
pub struct Heard {
    recs: BTreeMap<String, Entry>,
    offs: BTreeMap<String, u64>,
    links: HashMap<String, (u64, usize)>,
}

impl Heard {
    /// A record heard from link `from` at `now`: Ok(true) when it was taken and is to be passed on (newer than what
    /// was here), Err with why it wasn't.
    pub fn take(&mut self, me: &str, from: &str, r: &Value, now: u64) -> Result<bool, &'static str> {
        well_formed(r)?;
        let node = r["node"].as_str().unwrap_or("").to_string();
        if node == me {
            return Err("our own");
        }
        let at = r["at"].as_u64().unwrap_or(0);
        if at > now + FUTURE {
            return Err("from the future");
        }
        if at + KEEP < now {
            return Err("too old");
        }
        let seen = self.recs.get(&node).map(|e| e.at).unwrap_or(0).max(self.offs.get(&node).copied().unwrap_or(0));
        if at <= seen {
            return Err("not newer");
        }
        let off = r["off"] == true;
        if !off && self.recs.get(&node).is_some_and(|e| e.got + ORIGIN_GAP > now) {
            return Err("too soon after its last");
        }
        // Per link: so many a window, whoever they are from.
        let l = self.links.entry(from.to_string()).or_insert((now, 0));
        if l.0 + LINK_WINDOW <= now {
            *l = (now, 0);
        }
        if l.1 >= PER_LINK {
            return Err("that link brought too many");
        }
        // Checked last: the dearest step.
        if !arcade_mesh::canon::verify_json(r, &["sig"], "node", b"") {
            return Err("bad signature");
        }
        l.1 += 1;
        if self.links.len() > 4096 {
            self.links.retain(|_, (t, _)| t.saturating_add(LINK_WINDOW) > now);
        }
        if off {
            self.recs.remove(&node);
            self.offs.insert(node, at);
        } else {
            self.offs.remove(&node);
            if !self.recs.contains_key(&node) && self.recs.len() >= HEARD_MAX {
                self.expire(now);
                while self.recs.len() >= HEARD_MAX {
                    let Some(k) = self.recs.iter().min_by_key(|(_, e)| e.got).map(|(k, _)| k.clone()) else { break };
                    self.recs.remove(&k);
                }
            }
            self.recs.insert(node, Entry { rec: r.clone(), at, got: now, via: from.to_string() });
        }
        Ok(true)
    }

    /// Drop what is past KEEP.
    pub fn expire(&mut self, now: u64) {
        self.recs.retain(|_, e| e.at + KEEP >= now);
        self.offs.retain(|_, at| *at + KEEP >= now);
    }

    /// Records still standing at `now`, by node.
    pub fn live(&self, now: u64) -> Vec<(&str, &Value, &str)> {
        self.recs.iter().filter(|(_, e)| e.at + KEEP >= now).map(|(k, e)| (k.as_str(), &e.rec, e.via.as_str())).collect()
    }

    pub fn get(&self, node: &str, now: u64) -> Option<&Value> {
        self.recs.get(node).filter(|e| e.at + KEEP >= now).map(|e| &e.rec)
    }
}

fn heard() -> std::sync::MutexGuard<'static, Heard> {
    static H: OnceLock<Mutex<Heard>> = OnceLock::new();
    H.get_or_init(Default::default).lock().unwrap()
}

// ---------------------------------------------------------------- seal speed (measured)

#[derive(Default)]
struct Seals {
    /// (data bytes, nanoseconds of core time), oldest first, about SEAL_WINDOW bytes.
    runs: VecDeque<(u64, u64)>,
    /// Data bytes sealed here in all (kept across restarts).
    total: u64,
}

fn seals() -> std::sync::MutexGuard<'static, Seals> {
    static S: OnceLock<Mutex<Seals>> = OnceLock::new();
    S.get_or_init(Default::default).lock().unwrap()
}

impl Seals {
    fn note(&mut self, bytes: u64, nanos: u64) {
        if bytes == 0 {
            return;
        }
        self.total = self.total.saturating_add(bytes);
        self.runs.push_back((bytes, nanos));
        let mut sum: u64 = self.runs.iter().map(|r| r.0).sum();
        while self.runs.len() > 1 && (sum - self.runs[0].0 >= SEAL_WINDOW || self.runs.len() > 65_536) {
            sum -= self.runs.pop_front().unwrap().0;
        }
    }

    fn speed(&self) -> Option<f64> {
        let (b, ns) = self.runs.iter().fold((0u64, 0u64), |a, r| (a.0 + r.0, a.1 + r.1));
        (self.total >= SEAL_MIN_BYTES && b > 0 && ns > 0).then(|| (ns as f64 / 1e9) / (b as f64 / GIB))
    }
}

/// Sloth sealing just did `bytes` data bytes in `nanos` of one core's time (arcseal.rs, per worker chunk).
pub fn note_seal(bytes: u64, nanos: u64) {
    seals().note(bytes, nanos);
}

/// Core-seconds per GiB sealed, over the last seals; None until SEAL_MIN_BYTES were sealed here.
pub fn seal_speed() -> Option<f64> {
    seals().speed()
}

// ---------------------------------------------------------------- this arcade's side

static ON: AtomicBool = AtomicBool::new(false);

/// What this arcade last sent (signed): its record while on, its off record after switching off.
fn mine() -> std::sync::MutexGuard<'static, Option<Value>> {
    static M: OnceLock<Mutex<Option<Value>>> = OnceLock::new();
    M.get_or_init(Default::default).lock().unwrap()
}

/// Facts read lately (reading runs nvidia-smi: not more than once a minute).
fn facts_cached(node: &Node) -> Facts {
    static F: OnceLock<Mutex<Option<(u64, Facts)>>> = OnceLock::new();
    let now = arcade_app::now();
    let mut c = F.get_or_init(Default::default).lock().unwrap();
    if let Some((t, f)) = c.as_ref() {
        if *t + 60 > now {
            return f.clone();
        }
    }
    let f = gather(&node.home);
    *c = Some((now, f.clone()));
    f
}

const KEY: &[u8] = b"sysinfo";

fn save(node: &Node) {
    let s = seals();
    let (b, ns) = s.runs.iter().fold((0u64, 0u64), |a, r| (a.0 + r.0, a.1 + r.1));
    let v = json!({"on": ON.load(Ordering::Relaxed), "seal": {"total": s.total, "bytes": b, "ns": ns}});
    drop(s);
    let _ = node.app.store.write(|w| w.put(arcade_app::META, KEY, v.to_string().as_bytes()));
}

/// At start: the switch and the seal measurements, as they were.
pub fn load(node: &Node) {
    let v = node.app.store.read(|r| r.get(arcade_app::META, KEY)).ok().flatten().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).unwrap_or(Value::Null);
    ON.store(v["on"] == true, Ordering::Relaxed);
    let mut s = seals();
    s.total = v["seal"]["total"].as_u64().unwrap_or(0);
    if let (Some(b), Some(ns)) = (v["seal"]["bytes"].as_u64().filter(|b| *b > 0), v["seal"]["ns"].as_u64()) {
        s.runs.push_back((b, ns));
    }
}

pub fn is_on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// This arcade's record as it would go now (unsigned).
fn build(node: &Node, at: u64) -> Value {
    record(&node.id_hex(), at, &facts_cached(node), &version(), seal_speed(), &private_words())
}

fn signed(node: &Node, mut r: Value) -> Value {
    arcade_mesh::canon::sign_json(&node.identity, &mut r, &["sig"], b"");
    r
}

/// Sign `r`, keep it as ours, and send it to every link (passed on from there).
fn send(node: &Node, r: Value) {
    let r = signed(node, r);
    *mine() = Some(r.clone());
    node.mesh_outbox.lock().unwrap().push(r);
}

/// The frame a new link gets: our record while on, our off record for a day after switching off.
pub fn link_frame() -> Option<Value> {
    let m = mine().clone()?;
    (m["at"].as_u64().unwrap_or(0) + KEEP >= arcade_app::now()).then_some(m)
}

/// A "si" frame from link `from`.
pub fn on_frame(node: &Node, from: &str, h: &Value) {
    let took = heard().take(&node.id_hex(), from, h, arcade_app::now());
    if took == Ok(true) {
        node.mesh_outbox.lock().unwrap().push(h.clone());
    }
}

/// `sysinfo.set {on}`.
pub fn set(node: &Node, req: &Value) -> Result<Value, String> {
    let on = match &req["on"] {
        Value::Bool(b) => *b,
        Value::String(s) if s == "true" || s == "1" => true,
        Value::String(s) if s == "false" || s == "0" => false,
        _ => return Err("on required (true or false)".into()),
    };
    let was = ON.swap(on, Ordering::Relaxed);
    save(node);
    let now = arcade_app::now();
    if on {
        // The operator just chose to: it goes now, not after the start wait.
        send(node, build(node, now));
    } else if was || mine().as_ref().is_some_and(|m| m["off"] != true) {
        send(node, off_record(&node.id_hex(), now));
    }
    Ok(status(node))
}

/// `sysinfo.status`: the switch, and the exact record (field by field) this arcade sends or would send.
pub fn status(node: &Node) -> Value {
    let now = arcade_app::now();
    let on = is_on();
    let last = mine().clone();
    let preview = match &last {
        Some(m) if on && m["off"] != true => m.clone(),
        _ => build(node, now),
    };
    let sealed = seals().total;
    json!({
        "on": on,
        "record": preview,
        "sent_at": last.as_ref().filter(|m| on && m["off"] != true).and_then(|m| m["at"].as_u64()),
        "bytes": preview.to_string().len(),
        "seal": {"sealed_bytes": sealed, "needs_bytes": SEAL_MIN_BYTES, "measured": seal_speed().is_some()},
        "reported": ["cpu", "cores", "ram_gb", "gpus", "disk_gb", "os", "arch", "version"],
        "measured": ["seal"],
        "never": ["host name", "user names", "paths", "serial numbers", "MAC addresses", "IP addresses", "other software"],
    })
}

/// Background: while on, send at start (once a seal speed is known, or START_WAIT after start), on a change of a
/// rounded value, and every EVERY.
pub fn run(node: std::sync::Arc<Node>) {
    let started = arcade_app::now();
    let mut last_body: Option<Value> = None;
    loop {
        std::thread::sleep(Duration::from_secs(60));
        let now = arcade_app::now();
        heard().expire(now);
        if !is_on() {
            last_body = None;
            continue;
        }
        let sent = mine().clone().filter(|m| m["off"] != true);
        if sent.is_none() && seal_speed().is_none() && now < started + START_WAIT {
            continue;
        }
        let r = build(&node, now);
        let body = |v: &Value| {
            let mut v = v.clone();
            if let Some(o) = v.as_object_mut() {
                o.remove("at");
                o.remove("sig");
            }
            v
        };
        let changed = last_body.as_ref() != Some(&body(&r)) && sent.as_ref().map(body) != Some(body(&r));
        let sent_at = sent.as_ref().and_then(|m| m["at"].as_u64()).unwrap_or(0);
        // A figure near a rounding edge (free disk) doesn't send a record every minute: changes wait CHANGE_GAP.
        let changed = changed && sent_at + CHANGE_GAP <= now;
        let due = sent.is_none() || sent_at + EVERY <= now;
        if changed || due {
            last_body = Some(body(&r));
            send(&node, r);
            save(&node);
        }
    }
}

// ---------------------------------------------------------------- reading it

fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Some(v[v.len() / 2])
}

/// Network totals over `recs` (each arcade once).
pub fn totals(recs: &[&Value]) -> Value {
    let sum = |f: &dyn Fn(&Value) -> Option<u64>| recs.iter().filter_map(|r| f(r)).sum::<u64>();
    let gpus: Vec<&Value> = recs.iter().flat_map(|r| r["gpus"].as_array().into_iter().flatten()).collect();
    let seals: Vec<f64> = recs.iter().filter_map(|r| r["seal"].as_f64()).collect();
    json!({
        "cores": {"physical": sum(&|r| r["cores"]["physical"].as_u64()), "logical": sum(&|r| r["cores"]["logical"].as_u64())},
        "ram_gb": sum(&|r| r["ram_gb"].as_u64()),
        "disk_gb": {"total": sum(&|r| r["disk_gb"]["total"].as_u64()), "free": sum(&|r| r["disk_gb"]["free"].as_u64())},
        "gpus": {"count": gpus.len(), "vram_gb": gpus.iter().filter_map(|g| g["vram_gb"].as_u64()).sum::<u64>(),
                 "vram_known": gpus.iter().filter(|g| g["vram_gb"].is_u64()).count(), "unified": gpus.iter().filter(|g| g["unified"] == true).count()},
        "seal": {"fastest": seals.iter().cloned().reduce(f64::min), "median": median(seals.clone()), "slowest": seals.iter().cloned().reduce(f64::max), "arcades": seals.len()},
    })
}

/// `mesh.sysinfo` and `explorer.arc_machines`: the newest record of every other arcade heard in the last 24 h, the
/// counts (arcades sharing, of the arcades heard), the network totals as reported by them, and this arcade's own.
pub fn read(node: &Node) -> Value {
    let now = arcade_app::now();
    let linked: BTreeSet<String> = node.links.lock().unwrap().values().map(|l| l.node_id.clone()).collect();
    let me = node.id_hex();
    let h = heard();
    let live = h.live(now);
    let mut ids = crate::meshview::heard_ids(node);
    ids.extend(live.iter().map(|(n, _, _)| n.to_string()));
    ids.remove(&me);
    let recs: Vec<&Value> = live.iter().map(|(_, r, _)| *r).collect();
    let arcades: Vec<Value> = live
        .iter()
        .map(|(n, r, _)| {
            let mut v = (*r).clone();
            if let Some(o) = v.as_object_mut() {
                o.remove("t");
            }
            v["linked"] = json!(linked.contains(*n));
            v["tag"] = json!(crate::meshview::tag_of(n));
            v
        })
        .collect();
    let t = totals(&recs);
    drop(h);
    let own = if is_on() { mine().clone().filter(|m| m["off"] != true) } else { None };
    json!({
        "sharing": arcades.len(),
        "heard": ids.len(),
        "arcades": arcades,
        "totals": t,
        "me": {"sharing": own.is_some(), "record": own},
        "note": "Reported by the arcades themselves (one machine may run several), except seal speed, measured from their own seals. Rounded; never on a chain; nothing that pays reads it.",
    })
}

/// The short machine line for `mesh.overview`, when `node` shares.
pub fn machine(node: &str) -> Option<Value> {
    let now = arcade_app::now();
    let h = heard();
    Some(machine_line(h.get(node, now)?))
}

/// This arcade's own machine line, while it shares (None when off).
pub fn own_machine() -> Option<Value> {
    let m = if is_on() { mine().clone().filter(|m| m["off"] != true) } else { None };
    m.as_ref().map(machine_line)
}

fn machine_line(r: &Value) -> Value {
    let gpus: Vec<Value> = r["gpus"].as_array().into_iter().flatten().map(|g| json!({"name": g["name"], "vram_gb": g["vram_gb"], "unified": g["unified"] == true})).collect();
    json!({
        "cpu": r["cpu"], "cores": r["cores"]["physical"].as_u64().or(r["cores"]["logical"].as_u64()), "threads": r["cores"]["logical"],
        "ram_gb": r["ram_gb"], "gpus": gpus, "disk_free_gb": r["disk_gb"]["free"], "disk_total_gb": r["disk_gb"]["total"],
        "seal": r["seal"], "os": r["os"], "arch": r["arch"], "version": r["version"], "at": r["at"],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[n; 32])
    }
    fn id(k: &ed25519_dalek::SigningKey) -> String {
        hex::encode(k.verifying_key().to_bytes())
    }
    fn facts() -> Facts {
        Facts {
            cpu: Some("AMD Ryzen 9 7950X 16-Core Processor".into()),
            physical: Some(16),
            logical: Some(32),
            ram_bytes: Some(64 * (1 << 30) - 300_000_000),
            gpus: vec![Gpu { name: "NVIDIA GeForce RTX 4090".into(), vram_bytes: Some(24564 << 20), unified: false }],
            disk: Some((2_134_000_000_000, 1_049_000_000_000)),
        }
    }
    fn signed_rec(k: &ed25519_dalek::SigningKey, at: u64) -> Value {
        let mut r = record(&id(k), at, &facts(), "v0.1.140", Some(3456.0), &[]);
        arcade_mesh::canon::sign_json(k, &mut r, &["sig"], b"");
        r
    }
    fn signed_off(k: &ed25519_dalek::SigningKey, at: u64) -> Value {
        let mut r = off_record(&id(k), at);
        arcade_mesh::canon::sign_json(k, &mut r, &["sig"], b"");
        r
    }

    #[test]
    fn rounding() {
        assert_eq!(round_gb(127_535_020 * 1024), 122);
        assert_eq!(round_gb(24564 << 20), 24);
        assert_eq!(round_gb(0), 0);
        assert_eq!(round_disk_gb(4_000_000_000), 0);
        assert_eq!(round_disk_gb(5_000_000_000), 10, "halves round up");
        assert_eq!(round_disk_gb(476_300_000_000), 480);
        assert_eq!(round_disk_gb(994_000_000_000), 990);
        assert_eq!(round_disk_gb(1_049_000_000_000), 1000, "nearest 100 GB from 1 TB up");
        assert_eq!(round_disk_gb(2_134_000_000_000), 2100);
        assert_eq!(sig2(3456.0), Some(3500.0));
        assert_eq!(sig2(0.9349), Some(0.93));
        assert_eq!(sig2(12.0), Some(12.0));
        assert_eq!(sig2(0.0), None);
        assert_eq!(sig2(f64::NAN), None);
        let r = record("ab", 1, &facts(), "v0.1.140", Some(3456.0), &[]);
        assert_eq!(r["ram_gb"], 64);
        assert_eq!(r["disk_gb"], json!({"total": 2100, "free": 1000}));
        assert_eq!(r["gpus"], json!([{"name": "NVIDIA GeForce RTX 4090", "vram_gb": 24}]));
        assert_eq!(r["cores"], json!({"physical": 16, "logical": 32}));
        assert_eq!(r["seal"], json!(3500.0));
        assert_eq!(r["cpu"], "AMD Ryzen 9 7950X");
    }

    #[test]
    fn cpu_names_are_cleaned() {
        let c = |s: &str| clean_name(s, &[]);
        assert_eq!(c("Intel(R) Core(TM) i9-13900K").as_deref(), Some("Intel Core i9-13900K"));
        assert_eq!(c("Intel(R) Xeon(R) CPU E5-2680 v4 @ 2.40GHz").as_deref(), Some("Intel Xeon E5-2680 v4"));
        assert_eq!(c("AMD Ryzen 7 5800H with Radeon Graphics").as_deref(), Some("AMD Ryzen 7 5800H"));
        assert_eq!(c("AMD EPYC 7763 64-Core Processor").as_deref(), Some("AMD EPYC 7763"));
        assert_eq!(c("Apple M2 Pro").as_deref(), Some("Apple M2 Pro"));
        assert_eq!(c("ARM Cortex-X925 + Cortex-A725").as_deref(), Some("ARM Cortex-X925 + Cortex-A725"));
        assert_eq!(c("Genuine Intel(R) CPU 0000 stepping 4 microcode 0x2b000161").as_deref(), Some("Genuine Intel 0000"));
        // A model string that carries a serial: the serial goes, the model stays.
        assert_eq!(c("Virtual CPU 1f8e3c2a9b7d4e61 QEMU").as_deref(), Some("Virtual QEMU"));
        assert_eq!(c("Custom SoC SN 4C1D00A7F3 rev 2").as_deref(), Some("Custom SoC"));
        assert_eq!(c("Board X1 serial: 00000000a1b2c3d4").as_deref(), Some("Board X1"));
        assert_eq!(c("Vendor CPU ABCDEF1234567890XYZ").as_deref(), Some("Vendor"));
        // At most 48 characters, cut between words.
        let long = c("Some Very Long Brand Name Processor Model Extreme Edition Ultra Max Plus").unwrap();
        assert!(long.len() <= 48, "{long}");
        assert!(long.starts_with("Some Very Long Brand Name"));
        assert_eq!(c("@ 3.0GHz"), None);
        assert_eq!(c(""), None);
        // The host and user names never.
        assert_eq!(clean_name("Intel Core i7 gx10-0e7e martin", &["gx10-0e7e".into(), "martin".into()]).as_deref(), Some("Intel Core i7"));
    }

    /// A tiny deterministic generator for the fuzz below.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn pick<'a>(&mut self, v: &[&'a str]) -> &'a str {
            v[(self.next() % v.len() as u64) as usize]
        }
    }

    #[test]
    fn a_record_never_holds_a_host_name_path_or_address() {
        let private = vec!["workbench-07".to_string(), "workbench".to_string(), "alice".to_string()];
        let secrets = [
            "/home/alice/.local/share/arcade", "C:\\Users\\alice\\AppData", "192.0.2.10", "10.0.0.5:8421", "fe80::1",
            "2001:db8::7", "00:1b:44:11:3a:b7", "aa-bb-cc-dd-ee-ff", "alice@example.com", "workbench-07", "alice",
            "SN=7F3A9C21", "serial:0x00001234abcd", "1f8e3c2a9b7d4e61", "file:///etc/hostname", "ws://203.0.113.5",
            "user=alice", "host_workbench", "WORKBENCH-07.local", "ALICE",
        ];
        let words = ["Intel", "AMD", "Ryzen", "Core", "i7-12700K", "RTX", "4090", "Xeon", "Processor", "CPU", "Apple", "M3", "GeForce", "(R)", "(TM)", "@", "3.2GHz", "with", "Graphics"];
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..3000 {
            let mk = |rng: &mut Rng| {
                let n = 1 + rng.next() % 7;
                (0..n).map(|_| if rng.next() % 3 == 0 { rng.pick(&secrets) } else { rng.pick(&words) }).collect::<Vec<_>>().join(" ")
            };
            let gpus = (0..(1 + rng.next() % 10)).map(|_| Gpu { name: mk(&mut rng), vram_bytes: Some(8 << 30), unified: false }).collect();
            let f = Facts {
                cpu: Some(mk(&mut rng)),
                physical: Some(4),
                logical: Some(8),
                ram_bytes: Some(1 << 34),
                gpus,
                disk: Some((1 << 40, 1 << 39)),
            };
            let r = record(&"ab".repeat(32), 1_800_000_000, &f, "v0.1.140", None, &private);
            let text = r.to_string().to_ascii_lowercase();
            for s in ["/home", "\\users", "192.0.2", "10.0.0", "8421", "fe80", "2001:db8", "00:1b", "aa-bb", "example.com", "workbench", "alice", "7f3a9c21", "1234abcd", "1f8e3c2a", "hostname", "203.0.113", ".local"] {
                assert!(!text.contains(s), "{s} in {text}");
            }
            let mut signed = r.clone();
            signed["sig"] = json!("00".repeat(64));
            assert!(signed.to_string().len() <= RECORD_MAX);
            assert!(well_formed(&signed).is_ok(), "{signed}");
        }
        // This machine's own record: no host name, user name or home path in it.
        let home = std::env::temp_dir();
        let own = record(&"ab".repeat(32), 1, &gather(&home), &version(), None, &private_words());
        let text = own.to_string().to_ascii_lowercase();
        for w in private_words().iter().filter(|w| w.len() >= 4) {
            assert!(!text.contains(w.as_str()), "{w} in {text}");
        }
        assert!(!text.contains(&home.to_string_lossy().to_ascii_lowercase()));
    }

    #[test]
    fn the_size_limit() {
        let big = Facts {
            cpu: Some("Brand ".repeat(20)),
            gpus: (0..20).map(|i| Gpu { name: format!("Vendor Accelerator Model {i} Extreme Edition Long"), vram_bytes: Some(80 << 30), unified: false }).collect(),
            ..facts()
        };
        let k = key(1);
        let mut r = record(&id(&k), 1, &big, "v0.1.140-rc1+build", Some(1.0), &[]);
        assert!(r["gpus"].as_array().unwrap().len() <= GPUS_MAX);
        arcade_mesh::canon::sign_json(&k, &mut r, &["sig"], b"");
        assert!(r.to_string().len() <= RECORD_MAX, "{}", r.to_string().len());
        assert!(well_formed(&r).is_ok());
        // One heard over the limit is dropped.
        let mut fat = signed_rec(&k, 1);
        fat["gpus"] = json!((0..8).map(|_| json!({"name": "X".repeat(48), "vram_gb": 80})).collect::<Vec<_>>());
        fat["cpu"] = json!("Y".repeat(48));
        assert!(fat.to_string().len() > RECORD_MAX);
        assert_eq!(well_formed(&fat), Err("too big"));
        // And one with a field it doesn't know.
        let mut odd = signed_rec(&k, 1);
        odd["hostname"] = json!("box");
        assert_eq!(well_formed(&odd), Err("a field out of form"));
    }

    #[test]
    fn newest_wins_expiry_and_the_future() {
        let (a, me) = (key(2), "ff".repeat(32));
        let now = 1_800_000_000;
        let mut h = Heard::default();
        assert_eq!(h.take(&me, "link1", &signed_rec(&a, now - 100), now), Ok(true));
        assert_eq!(h.take(&me, "link2", &signed_rec(&a, now - 100), now), Err("not newer"), "the same record again isn't passed on");
        assert_eq!(h.take(&me, "link2", &signed_rec(&a, now - 200), now + 120), Err("not newer"));
        assert_eq!(h.take(&me, "link2", &signed_rec(&a, now - 50), now + 10), Err("too soon after its last"));
        assert_eq!(h.take(&me, "link2", &signed_rec(&a, now + 50), now + 120), Ok(true));
        assert_eq!(h.get(&id(&a), now + 120).unwrap()["at"], now + 50);
        // More than 10 min ahead: never kept.
        let b = key(3);
        assert_eq!(h.take(&me, "link1", &signed_rec(&b, now + FUTURE + 1), now), Err("from the future"));
        assert_eq!(h.take(&me, "link1", &signed_rec(&b, now + FUTURE), now), Ok(true));
        // 24 h without a fresh one: gone.
        assert!(h.get(&id(&a), now + 50 + KEEP).is_some());
        assert!(h.get(&id(&a), now + 51 + KEEP).is_none());
        h.expire(now + 51 + KEEP);
        assert!(h.live(now + 51 + KEEP).iter().all(|(n, _, _)| *n != id(&a)));
        assert_eq!(h.take(&me, "link1", &signed_rec(&a, now - KEEP - 1), now), Err("too old"));
        // Our own record coming back.
        let mine = key(9);
        assert_eq!(h.take(&id(&mine), "link1", &signed_rec(&mine, now), now), Err("our own"));
    }

    #[test]
    fn an_off_record_drops_the_machine() {
        let (a, me) = (key(4), "ff".repeat(32));
        let now = 1_800_000_000;
        let mut h = Heard::default();
        h.take(&me, "l", &signed_rec(&a, now), now).unwrap();
        assert!(h.get(&id(&a), now).is_some());
        // At once, even within the minute.
        assert_eq!(h.take(&me, "l", &signed_off(&a, now + 1), now + 1), Ok(true));
        assert!(h.get(&id(&a), now + 1).is_none());
        // A record from before it, passed on late, doesn't bring it back; a newer one does.
        assert_eq!(h.take(&me, "l", &signed_rec(&a, now), now + 2), Err("not newer"));
        assert!(h.get(&id(&a), now + 2).is_none());
        assert_eq!(h.take(&me, "l", &signed_rec(&a, now + 5), now + 5), Ok(true));
        assert!(h.get(&id(&a), now + 5).is_some());
        // An off record carries nothing else.
        let mut odd = signed_off(&a, now + 9);
        odd["cpu"] = json!("Intel");
        assert_eq!(well_formed(&odd), Err("an off record carries nothing else"));
    }

    #[test]
    fn a_bad_signature_is_dropped() {
        let (a, me) = (key(5), "ff".repeat(32));
        let now = 1_800_000_000;
        let mut h = Heard::default();
        let mut r = signed_rec(&a, now);
        r["ram_gb"] = json!(512); // changed after signing
        assert_eq!(h.take(&me, "l", &r, now), Err("bad signature"));
        // Signed by another key than the node it names.
        let mut r = record(&id(&a), now, &facts(), "v0.1.140", None, &[]);
        arcade_mesh::canon::sign_json(&key(6), &mut r, &["sig"], b"");
        assert_eq!(h.take(&me, "l", &r, now), Err("bad signature"));
        let mut off = off_record(&id(&a), now + 1);
        arcade_mesh::canon::sign_json(&key(6), &mut off, &["sig"], b"");
        h.take(&me, "l", &signed_rec(&a, now), now).unwrap();
        assert_eq!(h.take(&me, "l", &off, now + 1), Err("bad signature"));
        assert!(h.get(&id(&a), now + 1).is_some(), "a forged off record doesn't drop it");
    }

    #[test]
    fn a_link_brings_so_many_a_window() {
        let me = "ff".repeat(32);
        let now = 1_800_000_000;
        let mut h = Heard::default();
        let keys: Vec<_> = (0..(PER_LINK + 1)).map(|i| ed25519_dalek::SigningKey::from_bytes(&[(i % 250) as u8 + 1, (i / 250) as u8 + 1, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7])).collect();
        for k in &keys[..PER_LINK] {
            assert_eq!(h.take(&me, "busy", &signed_rec(k, now), now), Ok(true));
        }
        assert_eq!(h.take(&me, "busy", &signed_rec(&keys[PER_LINK], now), now), Err("that link brought too many"));
        assert_eq!(h.take(&me, "other", &signed_rec(&keys[PER_LINK], now), now), Ok(true));
    }

    #[test]
    fn seal_speed_waits_for_64_mib() {
        let mut s = Seals::default();
        assert_eq!(s.speed(), None);
        s.note(32 << 20, 20_000_000_000);
        assert_eq!(s.speed(), None, "32 MiB isn't enough");
        s.note(32 << 20, 20_000_000_000);
        // 64 MiB in 40 core-seconds: 640 core-seconds a GiB.
        assert!((s.speed().unwrap() - 640.0).abs() < 0.001);
        // Over about the last GiB: older seals leave the window.
        for _ in 0..40 {
            s.note(32 << 20, 1_000_000_000);
        }
        assert!((s.speed().unwrap() - 32.0).abs() < 0.001, "{:?}", s.speed());
        assert_eq!(s.total, 42 * (32 << 20));
    }

    #[test]
    fn nvidia_smi_lines() {
        let g = parse_nvidia_smi("NVIDIA GeForce RTX 4090, 24564\nNVIDIA RTX A6000, 49140\n", "x86_64");
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].vram_bytes, Some(24564 << 20));
        let g = parse_nvidia_smi("NVIDIA GB10, [N/A]\n", "aarch64");
        assert_eq!(g, vec![Gpu { name: "NVIDIA GB10".into(), vram_bytes: None, unified: true }]);
        let r = record("ab", 1, &Facts { gpus: g, ..Default::default() }, "v1", None, &[]);
        assert_eq!(r["gpus"], json!([{"name": "NVIDIA GB10", "unified": true}]), "unified: no VRAM, never counted twice");
        let g = parse_nvidia_smi("Some GPU, [N/A]\n", "x86_64");
        assert!(!g[0].unified && g[0].vram_bytes.is_none(), "unknown is left out, not guessed");
    }

    #[test]
    fn totals_count_each_arcade_once() {
        let a = record("aa", 1, &facts(), "v1", Some(3000.0), &[]);
        let b = record("bb", 1, &Facts { gpus: vec![Gpu { name: "NVIDIA GB10".into(), vram_bytes: None, unified: true }], ..facts() }, "v1", Some(1000.0), &[]);
        let c = record("cc", 1, &Facts::default(), "v1", None, &[]);
        let t = totals(&[&a, &b, &c]);
        assert_eq!(t["cores"], json!({"physical": 32, "logical": 64}));
        assert_eq!(t["ram_gb"], 128);
        assert_eq!(t["disk_gb"], json!({"total": 4200, "free": 2000}));
        assert_eq!(t["gpus"], json!({"count": 2, "vram_gb": 24, "vram_known": 1, "unified": 1}));
        assert_eq!(t["seal"], json!({"fastest": 1000.0, "median": 3000.0, "slowest": 3000.0, "arcades": 2}));
    }
}
