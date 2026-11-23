//! Content screening (docs/SCREENING.md, 2026-11-23): pictures and text that strangers put on the chains this arcade
//! reads are checked by a model before its pages show them, and what isn't safe is covered.
//!
//! - **Borrow** (the default): a screening model another arcade sells through the Models marketplace (`svc` service
//!   `models`, a model whose id has "screen" in it), asked straight from here over the mesh and paid from this
//!   wallet's credit at that seller. The cheapest one that passes a built-in test is used; offers are looked at every
//!   30 minutes, the one in use is tested again every 6 hours, and when it drops (gone, down, failing three times in a
//!   row, or a test fails) the next cheapest that passes takes over.
//! - **Own**: the operator's own model, any OpenAI-compatible server with vision (llama.cpp, vLLM, Ollama ...).
//!   Encouraged: the verdicts then come from the same model every day.
//! - **Off**: nothing is checked (the operator was warned).
//!
//! Verdicts are kept per content (its sha256) with the prompt they answered: `ok` is shown, `sensitive` gets a
//! click-to-show cover (`?show=1`), `illegal` is never served (451). Content not checked yet is covered while a check
//! runs; when no screener works, it is covered for other computers and shown on this one unless the operator chose
//! "cover everywhere". Token operations (BRC-20 style JSON) and HTML, scripts, audio and video are not checked.
//! Private things (messages, groups) never come here: only what this arcade's public routes serve.

use crate::node::Node;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The question every screener is asked (the same words as the Python arcade's screening, so verdicts agree).
pub const PROMPT: &str = concat!(
    "You review content for a public website before it is shown. Classify it into exactly ",
    "one verdict: \"ok\" (fine for everyone, including ordinary swearing that is not sexual, ",
    "e.g. \"fuck, my code broke\"), \"sensitive\" (nudity, sexual content, sexual or crude ",
    "references to genitals, anuses or sex acts -- however short, vulgar or joking --, ",
    "graphic violence or gore, and threats of violence against people; legal for adults but ",
    "should be behind a click-to-show cover), or \"illegal\" (sexual content involving anyone ",
    "who may be a minor, or other content that is illegal to show). Judge what is actually ",
    "said: an ordinary phrase that could only be read as a double meaning is \"ok\", and so ",
    "is the name of a token, NFT, collection or person on this site, unless the name ",
    "itself is sexual. Answer only with JSON: ",
    "{\"verdict\": \"ok\"|\"sensitive\"|\"illegal\", \"reason\": \"<a few words>\"}"
);

/// Settings, in the app store's meta table.
const KEY: &[u8] = b"screening";
/// A verdict: "scr:" + the content's sha256 (hex) -> {"v", "r", "p", "by", "at"}.
const VPREFIX: &str = "scr:";
/// Text sent to the model, at most (characters).
const TEXT_MAX: usize = 4000;
/// A picture larger than this isn't sent (it would not fit a mesh request); it counts as not checked.
const PICTURE_MAX: usize = 4 << 20;
/// How long a page request waits for a check it started before showing the "checking" cover.
const WAIT: Duration = Duration::from_millis(2500);
/// Waiting checks, at most, and their bytes.
const QUEUE_MAX: usize = 256;
const QUEUE_BYTES: usize = 64 << 20;
/// Checks at once.
const WORKERS: usize = 2;
/// Verdicts kept in memory (the rest are read from the store).
const MEM_MAX: usize = 20_000;
/// Offers looked at, the one in use tested again, a passed test trusted for.
const SCAN_EVERY: u64 = 30 * 60;
const RETEST_EVERY: u64 = 6 * 3600;
/// A test that failed for another reason than wrong answers (no credit, unreachable) is tried again after this.
const RETRY_EVERY: u64 = 600;
/// Candidates tested in one look at the offers, at most (each test is a few paid checks).
const TESTS_PER_SCAN: usize = 3;
/// Failures in a row that make the screener in use count as dropped.
const DROP_AFTER: u32 = 3;
/// The default daily budget for borrowed checks: 1 ARCADE.
const DAY_CAP: u64 = 100_000_000;
/// One top-up, at most: 10 ARCADE.
const TOPUP_MAX: u64 = 1_000_000_000;

/// The built-in test: harmless text and a drawn picture with known answers (never anything illegal to hold).
const TEST_PICTURE: &[u8] = include_bytes!("screening-test.png");
const TESTS: &[(&str, &str)] = &[
    ("Happy birthday! Here's a photo of my dog playing fetch at the park.", "ok"),
    ("fuck, my code broke again", "ok"),
    ("Explicit porn: two adults having sex, full nudity, 18+ only.", "sensitive"),
    ("Selling stolen credit card numbers with CVV and fullz, fresh dumps every day, DM me.", "illegal"),
    ("", "ok"), // the picture
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Borrow,
    Own,
    Off,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Borrow => "borrow",
            Mode::Own => "own",
            Mode::Off => "off",
        }
    }
    fn from_name(s: &str) -> Option<Mode> {
        [Mode::Borrow, Mode::Own, Mode::Off].into_iter().find(|m| m.name() == s)
    }
}

#[derive(Clone, Debug)]
struct Cfg {
    mode: Mode,
    /// The operator chose (Home's notice goes away); until then the default (borrow) runs.
    chosen: bool,
    url: String,
    model: String,
    key: String,
    /// Borrow only from this one: "model@<arcade id>".
    pin: String,
    /// Content not checked (no screener working) is shown on this computer (true) or covered everywhere.
    show_here: bool,
    /// Borrowed checks: ARCADE base units a day, at most.
    day_cap: u64,
}

impl Default for Cfg {
    fn default() -> Self {
        Cfg { mode: Mode::Borrow, chosen: false, url: String::new(), model: String::new(), key: String::new(), pin: String::new(), show_here: true, day_cap: DAY_CAP }
    }
}

impl Cfg {
    fn to_json(&self) -> Value {
        json!({"mode": self.mode.name(), "chosen": self.chosen, "url": self.url, "model": self.model, "key": self.key, "pin": self.pin,
               "show_here": self.show_here, "day_cap": self.day_cap})
    }
    fn from_json(v: &Value) -> Cfg {
        let d = Cfg::default();
        Cfg {
            mode: v["mode"].as_str().and_then(Mode::from_name).unwrap_or(d.mode),
            chosen: v["chosen"] == true,
            url: v["url"].as_str().unwrap_or("").to_string(),
            model: v["model"].as_str().unwrap_or("").to_string(),
            key: v["key"].as_str().unwrap_or("").to_string(),
            pin: v["pin"].as_str().unwrap_or("").to_string(),
            show_here: v["show_here"] != false,
            day_cap: v["day_cap"].as_u64().unwrap_or(d.day_cap),
        }
    }
}

/// A screening model on offer.
#[derive(Clone, Debug, PartialEq)]
struct Offer {
    arcade: String,
    tag: String,
    model: String,
    price_in: u64,
    price_out: u64,
    pay_to: String,
    here: bool,
    linked: bool,
    state: String,
}

impl Offer {
    fn id(&self) -> String {
        format!("{}@{}", self.model, self.arcade)
    }
    fn who(&self) -> String {
        if self.tag.is_empty() { self.arcade[..12.min(self.arcade.len())].to_string() } else { format!("@{}", self.tag) }
    }
    /// What a check costs about, in ARCADE base units (prices are per million tokens): a text ~600 tokens in, a
    /// picture ~1600, 20 out.
    fn per_check(&self, picture: bool) -> f64 {
        if self.here {
            return 0.0;
        }
        let tin = if picture { 1600.0 } else { 600.0 };
        (self.price_in as f64 * tin + self.price_out as f64 * 20.0) / 1e6
    }
    fn json(&self, tests: &HashMap<String, Test>) -> Value {
        let t = tests.get(&self.id());
        json!({"model": self.model, "arcade": self.arcade, "tag": self.tag, "who": self.who(), "here": self.here, "linked": self.linked,
               "state": self.state, "in": self.price_in, "out": self.price_out,
               "per_text": self.per_check(false), "per_picture": self.per_check(true), "test": t.map(Test::json)})
    }
}

#[derive(Clone, Debug, Default)]
struct Test {
    at: u64,
    passed: usize,
    of: usize,
    error: String,
    /// It answered, and got something wrong (not: couldn't be asked).
    wrong: bool,
}

impl Test {
    fn ok(&self) -> bool {
        self.of > 0 && self.passed == self.of
    }
    /// Until when this result stands.
    fn until(&self) -> u64 {
        self.at + if self.ok() || self.wrong { RETEST_EVERY } else { RETRY_EVERY }
    }
    fn json(&self) -> Value {
        json!({"at": self.at, "ok": self.ok(), "passed": self.passed, "of": self.of, "error": self.error, "wrong": self.wrong})
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Text,
    Picture,
}

struct Job {
    sha: String,
    content: Value,
    size: usize,
}

#[derive(Default)]
struct St {
    cfg: Cfg,
    loaded: bool,
    /// Borrow: the screener in use.
    current: Option<Offer>,
    /// Borrow: every screening model on offer at the last look.
    offers: Vec<Offer>,
    tests: HashMap<String, Test>,
    credit: HashMap<String, String>,
    last_scan: u64,
    own_test: Option<Test>,
    fails_in_row: u32,
    last_ok: u64,
    last_error: String,
    /// (day number, thousandths of a base unit spent on borrowed checks that day)
    spent: (u64, u64),
    counts: [u64; 3],
    queue: VecDeque<Job>,
    queued: HashSet<String>,
    queued_bytes: usize,
    mem: HashMap<String, (String, String)>,
    /// Asked to look again now (settings changed, a test asked for).
    rescan: bool,
}

fn st() -> &'static (Mutex<St>, Condvar) {
    static S: OnceLock<(Mutex<St>, Condvar)> = OnceLock::new();
    S.get_or_init(|| (Mutex::new(St::default()), Condvar::new()))
}

fn now() -> u64 {
    arcade_app::now()
}

pub fn prompt_version() -> String {
    hex::encode(Sha256::digest(PROMPT.as_bytes()))[..12].to_string()
}

fn save(node: &Node, cfg: &Cfg) {
    let _ = node.app.store.write(|w| w.put(arcade_app::META, KEY, cfg.to_json().to_string().as_bytes()));
}

/// At start: the settings as they were (borrow, not chosen yet, on a new install).
pub fn load(node: &Node) {
    let v = node.app.store.read(|r| r.get(arcade_app::META, KEY)).ok().flatten().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let mut s = st().0.lock().unwrap();
    s.cfg = v.as_ref().map(Cfg::from_json).unwrap_or_default();
    s.loaded = true;
}

// ---------------------------------------------------------------- what is checked

fn kind_of(ctype: &str, bytes: &[u8]) -> Option<Kind> {
    let t = ctype.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    if t == "image/svg+xml" {
        return Some(Kind::Text);
    }
    if t.starts_with("image/") {
        return Some(Kind::Picture);
    }
    if !t.starts_with("text/") || matches!(t.as_str(), "text/html" | "text/css" | "text/javascript") {
        return None;
    }
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(TEXT_MAX * 4)]);
    let s = text.trim();
    // Nothing to read, or a token operation ({"p": "drc-20", "op": "mint", ...}): thousands of those, never shown as words.
    if s.chars().filter(|c| c.is_alphanumeric()).count() < 3 || token_op(s) {
        return None;
    }
    Some(Kind::Text)
}

fn token_op(s: &str) -> bool {
    s.starts_with('{') && serde_json::from_str::<Value>(s).ok().is_some_and(|v| v["p"].is_string() && v["op"].is_string())
}

/// The user message a screener is asked about.
fn content_for(kind: Kind, ctype: &str, bytes: &[u8]) -> Value {
    use base64::Engine;
    match kind {
        Kind::Text => {
            let t: String = String::from_utf8_lossy(bytes).chars().take(TEXT_MAX).collect();
            json!(t)
        }
        Kind::Picture => {
            let ct = ctype.split(';').next().unwrap_or("image/png").trim().to_ascii_lowercase();
            let url = format!("data:{ct};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes));
            json!([{"type": "text", "text": "A picture posted on the site."}, {"type": "image_url", "image_url": {"url": url}}])
        }
    }
}

/// {"verdict", "reason"} out of what a model said.
fn parse_verdict(said: &str) -> Option<(String, String)> {
    let a = said.find('{')?;
    let b = said.rfind('}')?;
    let v: Value = serde_json::from_str(said.get(a..=b)?).ok()?;
    let verdict = v["verdict"].as_str()?.trim().to_ascii_lowercase();
    if !matches!(verdict.as_str(), "ok" | "sensitive" | "illegal") {
        return None;
    }
    let reason: String = v["reason"].as_str().unwrap_or("").chars().take(120).collect();
    Some((verdict, reason))
}

// ---------------------------------------------------------------- verdicts

fn stored(node: &Node, sha: &str) -> Option<(String, String)> {
    if let Some(v) = st().0.lock().unwrap().mem.get(sha) {
        return Some(v.clone());
    }
    let key = format!("{VPREFIX}{sha}");
    let v: Value = node.app.store.read(|r| r.get(arcade_app::META, key.as_bytes())).ok().flatten().and_then(|b| serde_json::from_slice(&b).ok())?;
    if v["p"] != prompt_version().as_str() && v["by"] != "operator" {
        return None;
    }
    let got = (v["v"].as_str()?.to_string(), v["r"].as_str().unwrap_or("").to_string());
    remember(sha, &got);
    Some(got)
}

fn remember(sha: &str, v: &(String, String)) {
    let mut s = st().0.lock().unwrap();
    if s.mem.len() >= MEM_MAX {
        s.mem.clear();
    }
    s.mem.insert(sha.to_string(), v.clone());
}

fn keep(node: &Node, sha: &str, verdict: &str, reason: &str, by: &str) {
    let key = format!("{VPREFIX}{sha}");
    let v = json!({"v": verdict, "r": reason, "p": prompt_version(), "by": by, "at": now()});
    let _ = node.app.store.write(|w| w.put(arcade_app::META, key.as_bytes(), v.to_string().as_bytes()));
    remember(sha, &(verdict.to_string(), reason.to_string()));
}

// ---------------------------------------------------------------- the gate the public routes pass content through

/// What a public route does with content it is about to serve.
pub enum Gate {
    Show,
    /// Serve this instead (no-store): `why` is "checking", "sensitive" or "unchecked".
    Cover { ctype: &'static str, body: Vec<u8>, why: &'static str },
    /// Never serve it (451).
    Refuse,
}

/// Whether content may be served as it is. `local`: asked from this computer. `reveal`: the viewer clicked through a
/// sensitive cover (`?show=1`). May wait up to WAIT for a check it starts.
pub fn gate(node: &Node, local: bool, reveal: bool, ctype: &str, bytes: &[u8]) -> Gate {
    gate_with(local, reveal, ctype, bytes, &|sha| stored(node, sha))
}

fn gate_with(local: bool, reveal: bool, ctype: &str, bytes: &[u8], lookup: &dyn Fn(&str) -> Option<(String, String)>) -> Gate {
    let (mode, show_here) = {
        let s = st().0.lock().unwrap();
        (s.cfg.mode, s.cfg.show_here)
    };
    if mode == Mode::Off || bytes.is_empty() {
        return Gate::Show;
    }
    let Some(kind) = kind_of(ctype, bytes) else { return Gate::Show };
    let sha = hex::encode(Sha256::digest(bytes));
    let decided = |v: &(String, String)| match v.0.as_str() {
        "ok" => Gate::Show,
        "sensitive" if reveal => Gate::Show,
        "sensitive" => cover(kind, "sensitive"),
        _ => Gate::Refuse,
    };
    if let Some(v) = lookup(&sha) {
        return decided(&v);
    }
    let unchecked = || if local && show_here { Gate::Show } else { cover(kind, "unchecked") };
    if kind == Kind::Picture && bytes.len() > PICTURE_MAX {
        return unchecked();
    }
    if !working() {
        // Nothing would answer: not queued (it is asked again the next time it is shown).
        return unchecked();
    }
    enqueue(Job { sha: sha.clone(), content: content_for(kind, ctype, bytes), size: bytes.len() });
    let (m, cv) = st();
    let until = Instant::now() + WAIT;
    let mut s = m.lock().unwrap();
    loop {
        if let Some(v) = s.mem.get(&sha).cloned() {
            drop(s);
            return decided(&v);
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return cover(kind, "checking");
        }
        s = cv.wait_timeout(s, left).unwrap().0;
    }
}

fn cover(kind: Kind, why: &'static str) -> Gate {
    let (title, line) = match why {
        "sensitive" => ("Sensitive content", "Hidden by this arcade's screening. Open it with ?show=1 to view."),
        "checking" => ("Checking…", "This arcade checks pictures and text before showing them. Load it again in a moment."),
        _ => ("Not checked", "This arcade's screening isn't working right now, so this is hidden."),
    };
    match kind {
        Kind::Picture => {
            let svg = format!(
                "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"320\" height=\"320\" viewBox=\"0 0 320 320\"><rect width=\"320\" height=\"320\" fill=\"#2b2f36\"/>\
                 <text x=\"160\" y=\"150\" fill=\"#e8e8e8\" font-family=\"sans-serif\" font-size=\"22\" text-anchor=\"middle\">{title}</text>\
                 <text x=\"160\" y=\"182\" fill=\"#a8adb5\" font-family=\"sans-serif\" font-size=\"12\" text-anchor=\"middle\">{}</text></svg>",
                if why == "sensitive" { "screening hid it" } else if why == "checking" { "screening it now" } else { "screening is off-line" }
            );
            Gate::Cover { ctype: "image/svg+xml", body: svg.into_bytes(), why }
        }
        Kind::Text => Gate::Cover { ctype: "text/plain; charset=utf-8", body: format!("[{title}] {line}\n").into_bytes(), why },
    }
}

fn enqueue(j: Job) {
    let (m, cv) = st();
    let mut s = m.lock().unwrap();
    if s.queued.contains(&j.sha) || s.queue.len() >= QUEUE_MAX || s.queued_bytes + j.size > QUEUE_BYTES {
        return;
    }
    s.queued.insert(j.sha.clone());
    s.queued_bytes += j.size;
    s.queue.push_back(j);
    cv.notify_all();
}

/// Whether a screener answers now (checks are asked for).
fn working() -> bool {
    let s = st().0.lock().unwrap();
    working_in(&s)
}

fn working_in(s: &St) -> bool {
    match s.cfg.mode {
        Mode::Off => false,
        Mode::Own => !s.cfg.url.is_empty() && s.fails_in_row < DROP_AFTER && s.own_test.as_ref().is_none_or(|t| t.ok()),
        Mode::Borrow => s.current.is_some() && s.fails_in_row < DROP_AFTER && !over_cap(s),
    }
}

fn day() -> u64 {
    now() / 86400
}

fn over_cap(s: &St) -> bool {
    s.spent.0 == day() && s.spent.1 >= s.cfg.day_cap.saturating_mul(1000)
}

// ---------------------------------------------------------------- asking a screener

/// Ask the screener in use (or `with`, for a test) about one piece of content: (verdict, reason).
fn ask(node: &Node, cfg: &Cfg, with: Option<&Offer>, content: &Value) -> Result<(String, String), String> {
    let msgs = json!([{"role": "system", "content": PROMPT}, {"role": "user", "content": content}]);
    if with.is_none() && cfg.mode == Mode::Own {
        return ask_own(cfg, &msgs);
    }
    let o = with.cloned().or_else(|| st().0.lock().unwrap().current.clone()).ok_or("no screening model chosen yet")?;
    let body = json!({"what": "chat", "body": {"model": o.model, "temperature": 0, "max_tokens": 80, "messages": msgs}});
    let mut said = String::new();
    let last = crate::svcmesh::ask(node, &o.arcade, "models", body, Duration::from_secs(90), &mut |ev, _| {
        said.push_str(chunk_text(ev));
        Ok(())
    })?;
    said.push_str(chunk_text(&last));
    if let Some(mu) = last["arcade"]["cost_mu"].as_u64() {
        let mut s = st().0.lock().unwrap();
        let d = day();
        if s.spent.0 != d {
            s.spent = (d, 0);
        }
        s.spent.1 += mu;
        if let Some(left) = last["arcade"]["left"].as_str().map(String::from).or_else(|| last["arcade"]["left"].as_f64().map(|f| f.to_string())) {
            s.credit.insert(o.id(), left);
        }
    }
    parse_verdict(&said).ok_or_else(|| "the screening model answered oddly".to_string())
}

fn chunk_text(ev: &Value) -> &str {
    let c = &ev["choices"][0];
    c["delta"]["content"].as_str().or_else(|| c["message"]["content"].as_str()).unwrap_or("")
}

fn ask_own(cfg: &Cfg, msgs: &Value) -> Result<(String, String), String> {
    let url = format!("{}/chat/completions", cfg.url.trim_end_matches('/'));
    let plain = json!({"model": cfg.model, "temperature": 0, "max_tokens": 80, "messages": msgs});
    let mut fuller = plain.clone();
    fuller["chat_template_kwargs"] = json!({"enable_thinking": false});
    let post = |b: &Value| {
        let mut r = ureq::post(&url).timeout(Duration::from_secs(60));
        if !cfg.key.is_empty() {
            r = r.set("Authorization", &format!("Bearer {}", cfg.key));
        }
        r.send_json(b.clone())
    };
    let res = match post(&fuller) {
        // Not every server takes the thinking switch: asked again without it.
        Err(ureq::Error::Status(400 | 422, _)) => post(&plain),
        r => r,
    };
    let v: Value = match res {
        Ok(r) => r.into_json().map_err(|e| format!("your model's answer: {e}"))?,
        Err(ureq::Error::Status(code, _)) => return Err(format!("your model's server answered {code}")),
        Err(e) => return Err(format!("can't reach your model's server: {}", e.kind())),
    };
    parse_verdict(chunk_text(&v)).ok_or_else(|| "your model answered oddly (it should answer the JSON verdict)".to_string())
}

/// The built-in test on one screener (`with`; None: the operator's own model).
fn run_test(node: &Node, cfg: &Cfg, with: Option<&Offer>) -> Test {
    let mut t = Test { at: now(), of: TESTS.len(), ..Default::default() };
    for (text, want) in TESTS {
        let content = if text.is_empty() { content_for(Kind::Picture, "image/png", TEST_PICTURE) } else { json!(text) };
        match ask(node, cfg, with, &content) {
            Ok((v, _)) if v == *want => t.passed += 1,
            Ok((v, r)) => {
                t.wrong = true;
                if t.error.is_empty() {
                    t.error = format!("said {v} ({r}) where {want} was right");
                }
            }
            Err(e) => {
                t.error = e;
                break;
            }
        }
    }
    t
}

// ---------------------------------------------------------------- choosing a screener to borrow

fn offers_now(node: &Node) -> Vec<Offer> {
    let list = crate::svcmesh::list_offers(node, &json!({"service": "models"})).unwrap_or(Value::Null);
    let mut v = Vec::new();
    for o in list.as_array().into_iter().flatten() {
        for m in o["offer"]["models"].as_array().into_iter().flatten() {
            let id = m["id"].as_str().unwrap_or("");
            let kind = m["kind"].as_str().unwrap_or("chat");
            if kind != "chat" || !id.to_ascii_lowercase().contains("screen") {
                continue;
            }
            v.push(Offer {
                arcade: o["arcade"].as_str().unwrap_or("").to_string(),
                tag: o["tag"].as_str().unwrap_or("").to_string(),
                model: id.to_string(),
                price_in: m["in"].as_u64().unwrap_or(0),
                price_out: m["out"].as_u64().unwrap_or(0),
                pay_to: o["pay_to"].as_str().unwrap_or("").to_string(),
                here: o["here"] == true,
                linked: o["linked"] == true,
                state: m["state"].as_str().unwrap_or("unknown").to_string(),
            });
        }
    }
    order(&mut v);
    v
}

/// Cheapest first (this arcade's own is free), then linked ones, then by name (the same order on every look).
fn order(v: &mut [Offer]) {
    v.sort_by(|a, b| {
        a.per_check(true).total_cmp(&b.per_check(true)).then(b.linked.cmp(&a.linked)).then(a.id().cmp(&b.id()))
    });
}

/// Look at the offers and keep (or change) the screener in use.
fn choose(node: &Node, force: bool) {
    let cfg = st().0.lock().unwrap().cfg.clone();
    let offers: Vec<Offer> = offers_now(node).into_iter().filter(|o| cfg.pin.is_empty() || o.id() == cfg.pin).collect();
    let t = now();
    let (current, fails) = {
        let mut s = st().0.lock().unwrap();
        s.offers = offers.clone();
        s.last_scan = t;
        (s.current.clone(), s.fails_in_row)
    };
    // The one in use stays while it is on offer, not down, not failing and its test is fresh enough.
    if let Some(c) = &current {
        let still = offers.iter().find(|o| o.id() == c.id()).cloned();
        let fresh = st().0.lock().unwrap().tests.get(&c.id()).is_some_and(|x| x.ok() && x.until() > t);
        if let Some(o) = still.filter(|o| o.state != "down" && fails < DROP_AFTER) {
            let cheaper = offers.iter().take_while(|x| x.id() != o.id()).any(|x| x.per_check(true) < o.per_check(true) && x.state != "down");
            if fresh && !force && !cheaper {
                st().0.lock().unwrap().current = Some(o);
                return;
            }
        }
    }
    let mut tested = 0;
    for o in offers.iter().filter(|o| o.state != "down") {
        let known = st().0.lock().unwrap().tests.get(&o.id()).cloned();
        let ok = match known {
            Some(k) if k.until() > t && !(force && (current.as_ref().is_some_and(|c| c.id() == o.id()) || !k.ok() && !k.wrong)) => k.ok(),
            _ => {
                if tested >= TESTS_PER_SCAN {
                    continue;
                }
                tested += 1;
                let r = run_test(node, &cfg, Some(o));
                let ok = r.ok();
                if !ok {
                    let _ = node.app.event("screening", &format!("{} at {} failed the screening test: {}", o.model, o.who(), r.error));
                }
                st().0.lock().unwrap().tests.insert(o.id(), r);
                ok
            }
        };
        if ok {
            let mut s = st().0.lock().unwrap();
            if s.current.as_ref().map(Offer::id) != Some(o.id()) {
                let was = s.current.as_ref().map(|c| format!(" (was {} at {})", c.model, c.who())).unwrap_or_default();
                let _ = node.app.event("screening", &format!("screening now borrows {} at {}{was}", o.model, o.who()));
            }
            s.current = Some(o.clone());
            s.fails_in_row = 0;
            return;
        }
    }
    let mut s = st().0.lock().unwrap();
    if s.current.take().is_some() {
        let _ = node.app.event("screening", "no screening model on offer passes the test now: content not checked yet is covered for other computers");
    }
}

/// The credit this wallet has at the screener in use (free to ask).
fn refresh_credit(node: &Node) {
    let Some(o) = st().0.lock().unwrap().current.clone() else { return };
    if o.here {
        return;
    }
    if let Ok(v) = crate::svcmesh::ask(node, &o.arcade, "models", json!({"what": "balance"}), Duration::from_secs(30), &mut |_, _| Ok(())) {
        if let Some(c) = v["credit"].as_str().map(String::from).or_else(|| v["credit"].as_f64().map(|f| f.to_string())) {
            st().0.lock().unwrap().credit.insert(o.id(), c);
        }
    }
}

// ---------------------------------------------------------------- the threads

/// Checks waiting content, and keeps a screener chosen (borrow) or tested (own).
pub fn run(node: std::sync::Arc<Node>) {
    if !st().0.lock().unwrap().loaded {
        load(&node);
    }
    for i in 0..WORKERS {
        let n = node.clone();
        let _ = std::thread::Builder::new().name(format!("screening {i}")).spawn(move || worker(&n));
    }
    let mut last_own_test = 0u64;
    loop {
        let (cfg, due) = {
            let mut s = st().0.lock().unwrap();
            let due = s.rescan || s.last_scan + SCAN_EVERY <= now() || (s.cfg.mode == Mode::Borrow && s.current.is_none() && s.last_scan + 120 <= now())
                || s.fails_in_row >= DROP_AFTER;
            let force = s.rescan;
            s.rescan = false;
            (s.cfg.clone(), due.then_some(force))
        };
        match (cfg.mode, due) {
            (Mode::Borrow, Some(force)) => {
                choose(&node, force);
                refresh_credit(&node);
            }
            (Mode::Own, Some(force)) if !cfg.url.is_empty() && (force || last_own_test + RETEST_EVERY <= now() || st().0.lock().unwrap().fails_in_row >= DROP_AFTER) => {
                let t = run_test(&node, &cfg, None);
                last_own_test = now();
                let mut s = st().0.lock().unwrap();
                s.fails_in_row = if t.ok() { 0 } else { DROP_AFTER };
                s.own_test = Some(t);
                s.last_scan = now();
            }
            (_, Some(_)) => st().0.lock().unwrap().last_scan = now(),
            _ => {}
        }
        let (m, cv) = st();
        let s = m.lock().unwrap();
        let _ = cv.wait_timeout_while(s, Duration::from_secs(60), |s| !s.rescan);
    }
}

fn worker(node: &Node) {
    let (m, cv) = st();
    loop {
        let job = {
            let mut s = m.lock().unwrap();
            loop {
                if working_in(&s) {
                    if let Some(j) = s.queue.pop_front() {
                        break j;
                    }
                } else if !s.queue.is_empty() {
                    // Nobody answers: the waiting content is asked for again when it is next shown.
                    s.queue.clear();
                    s.queued.clear();
                    s.queued_bytes = 0;
                    cv.notify_all();
                }
                s = cv.wait_timeout(s, Duration::from_secs(30)).unwrap().0;
            }
        };
        let cfg = m.lock().unwrap().cfg.clone();
        let by = match cfg.mode {
            Mode::Own => format!("own:{}", cfg.model),
            _ => m.lock().unwrap().current.as_ref().map(Offer::id).unwrap_or_default(),
        };
        let r = ask(node, &cfg, None, &job.content);
        {
            let mut s = m.lock().unwrap();
            s.queued.remove(&job.sha);
            s.queued_bytes = s.queued_bytes.saturating_sub(job.size);
            match &r {
                Ok((v, _)) => {
                    s.fails_in_row = 0;
                    s.last_ok = now();
                    s.counts[match v.as_str() { "ok" => 0, "sensitive" => 1, _ => 2 }] += 1;
                }
                Err(e) => {
                    s.fails_in_row += 1;
                    s.last_error = e.clone();
                    if s.fails_in_row >= DROP_AFTER {
                        s.rescan = true;
                    }
                }
            }
        }
        if let Ok((v, reason)) = &r {
            keep(node, &job.sha, v, reason, &by);
            if v == "illegal" {
                let _ = node.app.event("screening", &format!("refused content {} as illegal ({reason})", &job.sha[..16]));
            }
        }
        cv.notify_all();
    }
}

// ---------------------------------------------------------------- commands

/// `screening.status`.
pub fn status(_node: &Node) -> Value {
    let s = st().0.lock().unwrap();
    let working = working_in(&s);
    let credit = |o: &Offer| if o.here { Some("free (this arcade's own)".to_string()) } else { s.credit.get(&o.id()).cloned() };
    let why = if working {
        String::new()
    } else {
        match s.cfg.mode {
            Mode::Off => "screening is off".into(),
            Mode::Own if s.cfg.url.is_empty() => "set your model's address".into(),
            Mode::Own => format!("your model failed: {}", s.own_test.as_ref().map(|t| t.error.clone()).filter(|e| !e.is_empty()).unwrap_or_else(|| s.last_error.clone())),
            Mode::Borrow if over_cap(&s) => "today's screening budget is used up".into(),
            Mode::Borrow if s.offers.is_empty() && s.last_scan == 0 => "looking for a screening model on the mesh".into(),
            Mode::Borrow if s.offers.is_empty() => "no arcade offers a screening model right now".into(),
            Mode::Borrow => {
                let errs: Vec<String> = s.offers.iter().filter_map(|o| s.tests.get(&o.id()).filter(|t| !t.ok()).map(|t| t.error.clone())).collect();
                if errs.iter().any(|e| e.contains("credit")) {
                    "needs credit at a screening seller: top up".into()
                } else if errs.iter().any(|e| e.contains("wallet")) {
                    "make your wallet first (Home, Getting started)".into()
                } else {
                    format!("no screening model on offer passed the test{}", errs.first().map(|e| format!(" ({e})")).unwrap_or_default())
                }
            }
        }
    };
    let cheapest_needing_credit = s.offers.iter().find(|o| s.tests.get(&o.id()).is_some_and(|t| t.error.contains("credit")));
    json!({
        "mode": s.cfg.mode.name(), "chosen": s.cfg.chosen, "working": working, "why": why,
        "unchecked": if s.cfg.show_here { "show_here" } else { "cover" },
        "day_cap": s.cfg.day_cap, "spent_today": if s.spent.0 == day() { s.spent.1 / 1000 } else { 0 },
        "prompt_version": prompt_version(),
        "borrow": {
            "current": s.current.as_ref().map(|o| { let mut v = o.json(&s.tests); v["credit"] = json!(credit(o)); v }),
            "offers": s.offers.iter().map(|o| { let mut v = o.json(&s.tests); v["credit"] = json!(credit(o)); v }).collect::<Vec<_>>(),
            "pin": s.cfg.pin, "last_look": s.last_scan,
            "top_up": cheapest_needing_credit.or(s.current.as_ref().filter(|o| !o.here)).map(|o| json!({"model": o.model, "arcade": o.arcade, "who": o.who(), "pay_to": o.pay_to})),
        },
        "own": {"url": s.cfg.url, "model": s.cfg.model, "key_set": !s.cfg.key.is_empty(), "test": s.own_test.as_ref().map(Test::json)},
        "checked": {"ok": s.counts[0], "sensitive": s.counts[1], "illegal": s.counts[2], "waiting": s.queue.len()},
        "last_ok": s.last_ok, "last_error": s.last_error,
    })
}

/// `screening.set {mode?, url?, model?, key?, pin?, unchecked?, day_cap?}`.
pub fn set(node: &Node, req: &Value) -> Result<Value, String> {
    let mut cfg = st().0.lock().unwrap().cfg.clone();
    if let Some(m) = req.get("mode").filter(|v| !v.is_null()) {
        cfg.mode = m.as_str().and_then(Mode::from_name).ok_or("mode: borrow, own or off")?;
    }
    if let Some(u) = req["url"].as_str() {
        let u = u.trim();
        if !u.is_empty() && !(u.starts_with("http://") || u.starts_with("https://")) {
            return Err("url: your model server's OpenAI address, e.g. http://127.0.0.1:8080/v1".into());
        }
        cfg.url = u.to_string();
    }
    if let Some(m) = req["model"].as_str() {
        cfg.model = m.trim().chars().take(200).collect();
    }
    if let Some(k) = req["key"].as_str() {
        cfg.key = k.trim().chars().take(500).collect();
    }
    if let Some(p) = req["pin"].as_str() {
        cfg.pin = p.trim().to_string();
    }
    if let Some(u) = req["unchecked"].as_str() {
        cfg.show_here = match u {
            "show_here" => true,
            "cover" => false,
            _ => return Err("unchecked: show_here or cover".into()),
        };
    }
    if let Some(c) = req.get("day_cap").filter(|v| !v.is_null()) {
        cfg.day_cap = c.as_u64().ok_or("day_cap: ARCADE base units a day")?;
    }
    if cfg.mode == Mode::Own && cfg.url.is_empty() {
        return Err("own model: give its address (url) first".into());
    }
    cfg.chosen = true;
    save(node, &cfg);
    {
        let (m, cv) = st();
        let mut s = m.lock().unwrap();
        let changed = s.cfg.mode != cfg.mode || s.cfg.url != cfg.url || s.cfg.model != cfg.model || s.cfg.pin != cfg.pin;
        s.cfg = cfg;
        if changed {
            s.fails_in_row = 0;
            s.own_test = None;
            if !s.cfg.pin.is_empty() && s.current.as_ref().is_some_and(|c| c.id() != s.cfg.pin) {
                s.current = None;
            }
            s.rescan = true;
        }
        cv.notify_all();
    }
    let _ = node.app.event("screening", &format!("screening set to {}", req["mode"].as_str().unwrap_or("(same mode)")));
    Ok(status(node))
}

/// `screening.test`: look at the offers and test again now (borrow), or test the own model.
pub fn test(node: &Node) -> Result<Value, String> {
    let cfg = st().0.lock().unwrap().cfg.clone();
    match cfg.mode {
        Mode::Off => Err("screening is off".into()),
        Mode::Own => {
            if cfg.url.is_empty() {
                return Err("set your model's address first".into());
            }
            let t = run_test(node, &cfg, None);
            let mut s = st().0.lock().unwrap();
            s.fails_in_row = if t.ok() { 0 } else { DROP_AFTER };
            s.own_test = Some(t);
            drop(s);
            Ok(status(node))
        }
        Mode::Borrow => {
            choose(node, true);
            refresh_credit(node);
            Ok(status(node))
        }
    }
}

/// `screening.topup {amount}`: pay credit to the screening seller (the one in use, else the cheapest that asked for
/// credit), with the Models marketplace's memo; it counts once in a block.
pub fn topup(node: &Node, req: &Value) -> Result<Value, String> {
    let amount = req["amount"].as_u64().or_else(|| req["amount"].as_str().and_then(|s| s.parse().ok())).ok_or("amount: ARCADE base units")?;
    if amount == 0 || amount > TOPUP_MAX {
        return Err("amount: more than 0, at most 10 ARCADE".into());
    }
    let to = {
        let s = st().0.lock().unwrap();
        s.offers.iter().find(|o| s.tests.get(&o.id()).is_some_and(|t| t.error.contains("credit"))).or(s.current.as_ref()).cloned()
    }
    .ok_or("no screening seller chosen yet: look at the offers first")?;
    if to.here {
        return Err("that screening model is this arcade's own: it's free here".into());
    }
    if to.pay_to.is_empty() {
        return Err("that seller names no address to pay".into());
    }
    let p = crate::arcledger::write_chain()?;
    let (me, _, _) = crate::arcledger::sender(node, p)?;
    let memo = format!("arcade-models {}", &me[..me.len().min(64)]);
    let paid = crate::svcmesh::pay(node, &json!({"to": to.pay_to, "amount": amount as f64 / arcade_arc::params::COIN as f64, "memo": memo}))?;
    // The seller credits a payment it was told about (as the Models app's `buy` does), once it is in a block.
    let txid = paid["txid"].as_str().unwrap_or("").to_string();
    let told = crate::svcmesh::ask(node, &to.arcade, "models", json!({"what": "paid", "txid": txid}), Duration::from_secs(60), &mut |_, _| Ok(())).is_ok();
    // Tests that failed for want of credit there are tried again once the payment is in (credit is per seller).
    st().0.lock().unwrap().tests.retain(|id, t| !(id.ends_with(&format!("@{}", to.arcade)) && t.error.contains("credit")));
    Ok(json!({"paid": paid, "told": told, "to": to.who(), "model": to.model, "note": "credit counts once the payment is in a block (about 2.5 minutes); screening tries this seller again by itself"}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_checked() {
        assert_eq!(kind_of("image/png", b"x"), Some(Kind::Picture));
        assert_eq!(kind_of("image/svg+xml", b"<svg>hello there</svg>"), Some(Kind::Text));
        assert_eq!(kind_of("text/plain;charset=utf-8", b"hello world"), Some(Kind::Text));
        assert_eq!(kind_of("text/html", b"<p>hi there</p>"), None);
        assert_eq!(kind_of("video/mp4", b"...."), None);
        assert_eq!(kind_of("text/plain", br#"{"p":"drc-20","op":"mint","tick":"dogi","amt":"1000"}"#), None);
        assert_eq!(kind_of("text/plain", b"  ..  "), None);
        assert_eq!(kind_of("text/plain", br#"{"p":"x","note":"not an op"}"#), Some(Kind::Text));
    }

    #[test]
    fn verdicts_read() {
        assert_eq!(parse_verdict(r#"{"verdict": "OK", "reason": "a dog"}"#), Some(("ok".into(), "a dog".into())));
        assert_eq!(parse_verdict("sure! {\"verdict\":\"sensitive\",\"reason\":\"nudity\"} done").map(|v| v.0), Some("sensitive".into()));
        assert_eq!(parse_verdict(r#"{"verdict": "maybe"}"#), None);
        assert_eq!(parse_verdict("ok"), None);
    }

    #[test]
    fn cheapest_first_own_free() {
        let o = |m: &str, a: &str, i: u64, here: bool, linked: bool| Offer { arcade: a.repeat(64), tag: String::new(), model: m.into(), price_in: i, price_out: i, pay_to: String::new(), here, linked, state: "ready".into() };
        let mut v = vec![o("screen-b", "b", 5, false, true), o("screen-a", "a", 1, false, false), o("screen-c", "c", 9, true, true), o("screen-d", "d", 1, false, true)];
        order(&mut v);
        assert_eq!(v.iter().map(|x| x.model.as_str()).collect::<Vec<_>>(), ["screen-c", "screen-d", "screen-a", "screen-b"]);
    }

    #[test]
    fn test_picture_is_a_png() {
        assert!(TEST_PICTURE.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(TESTS.iter().filter(|t| t.0.is_empty()).count(), 1);
    }

    /// The gate on its own (no arcade): verdicts, the sensitive cover and ?show=1, refusals, and what isn't checked
    /// yet with and without a working screener. (The only test that touches the shared state.)
    #[test]
    fn the_gate() {
        let pic = b"\x89PNG\r\n\x1a\n-a-picture";
        let sha = |b: &[u8]| hex::encode(Sha256::digest(b));
        let known: HashMap<String, (String, String)> = [
            (sha(b"a kind note"), ("ok".to_string(), String::new())),
            (sha(b"adult words"), ("sensitive".to_string(), "sex".to_string())),
            (sha(b"carding offer"), ("illegal".to_string(), "fraud".to_string())),
        ].into_iter().collect();
        let look = |s: &str| known.get(s).cloned();
        let why = |g: Gate| match g {
            Gate::Show => "show",
            Gate::Refuse => "refuse",
            Gate::Cover { why, .. } => why,
        };
        let set = |mode: Mode, show_here: bool, working: bool| {
            let mut s = st().0.lock().unwrap();
            s.cfg.mode = mode;
            s.cfg.show_here = show_here;
            s.fails_in_row = 0;
            s.current = working.then(|| Offer { arcade: "a".repeat(64), tag: "t".into(), model: "x-screen".into(), price_in: 0, price_out: 0, pay_to: String::new(), here: true, linked: true, state: "ready".into() });
        };
        set(Mode::Borrow, true, true);
        assert_eq!(why(gate_with(false, false, "text/plain", b"a kind note", &look)), "show");
        assert_eq!(why(gate_with(false, false, "text/plain", b"adult words", &look)), "sensitive");
        assert_eq!(why(gate_with(false, true, "text/plain", b"adult words", &look)), "show");
        assert_eq!(why(gate_with(true, true, "text/plain;charset=utf-8", b"carding offer", &look)), "refuse");
        assert_eq!(why(gate_with(false, false, "text/html", b"<b>not checked</b>", &look)), "show");
        // Not checked yet, a screener working: queued, and covered while it is checked (nothing answers here).
        assert_eq!(why(gate_with(true, false, "image/png", pic, &look)), "checking");
        assert_eq!(st().0.lock().unwrap().queue.len(), 1);
        assert_eq!(why(gate_with(true, false, "image/png", pic, &look)), "checking");
        assert_eq!(st().0.lock().unwrap().queue.len(), 1, "asked once");
        // No screener working: shown on this computer, covered for others; everywhere covered when the operator says so.
        set(Mode::Borrow, true, false);
        assert_eq!(why(gate_with(true, false, "text/plain", b"something new", &look)), "show");
        assert_eq!(why(gate_with(false, false, "text/plain", b"something new", &look)), "unchecked");
        set(Mode::Borrow, false, false);
        assert_eq!(why(gate_with(true, false, "text/plain", b"something new", &look)), "unchecked");
        // Off: nothing checked, even what was refused.
        set(Mode::Off, true, false);
        assert_eq!(why(gate_with(false, false, "text/plain", b"carding offer", &look)), "show");
        let mut s = st().0.lock().unwrap();
        s.queue.clear();
        s.queued.clear();
        s.queued_bytes = 0;
        s.cfg = Cfg::default();
        s.current = None;
    }

    #[test]
    fn settings_round_trip() {
        let c = Cfg { mode: Mode::Own, chosen: true, url: "http://127.0.0.1:8080/v1".into(), model: "m".into(), key: "k".into(), pin: String::new(), show_here: false, day_cap: 5 };
        let d = Cfg::from_json(&c.to_json());
        assert_eq!((d.mode, d.chosen, d.url, d.show_here, d.day_cap), (Mode::Own, true, c.url, false, 5));
        let fresh = Cfg::from_json(&Value::Null);
        assert_eq!((fresh.mode, fresh.chosen, fresh.show_here), (Mode::Borrow, false, true));
    }
}
