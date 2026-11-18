//! Repository writes for apps (docs/ARCADECHAIN.md §15 steps 9 and 10): an app page asks, the wallet's owner approves,
//! and this arcade's wallet signs with its @tag's key on the ARCADE chain.
//!
//! - `POST /r/repo/<op>` (an app page's pass): files a request as `/r/send` does; Arcade V1.0 shows it over the app
//!   for Approve or Refuse. Ops: `push` (blobs the page made: git packs, their indexes and a manifest; then the push
//!   record once the manifest is in a block; from rules version 4 the record names `base`, the manifest the push was
//!   built on, and the ledger refuses it if the repository moved on, unless `force`), `fork`, `maintainer`,
//!   `item_open`, `item_comment`, `item_state`.
//! - `GET /r/repo/req/<id>`: where a request stands (`repo.state`: working, done or failed, with its txids).
//! - `GET /r/repo/me`: this wallet's @tag on the ARCADE chain (the author of what it writes).
//!
//! An approved request runs in the background: a record waits for the wallet's coins to be free (confirmed) as the
//! ledger commands' own scripts do, so several in a row all go.

use crate::node::Node;
use serde_json::{json, Value};

/// A push's new blobs at most (one block's 4 MB budget, less room for the records).
const MAX_PUSH: usize = 3_800_000;
/// How long a request waits for coins or for its manifest to be in a block.
const WAIT_SECS: u64 = 45 * 60;

fn dir(node: &Node, id: &str) -> std::path::PathBuf {
    node.home.join("repo-requests").join(id)
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}

/// `GET /r/repo/me`.
pub fn me(node: &Node) -> Value {
    let Ok(p) = crate::arcledger::chain() else { return json!({"tag": null, "error": "the ARCADE chain isn't running here"}) };
    match crate::arcledger::sender(node, p) {
        Ok((address, tag, _)) => json!({"tag": tag, "address": address, "chain": p.key}),
        Err(e) => json!({"tag": null, "error": e}),
    }
}

fn full(name: &str, mine: Option<&str>) -> Result<String, String> {
    arcade_ledger::repos::full_name(name, mine).map_err(|e| format!("{name}: {e}"))
}

/// Check a request and put it in words: (what, repository, args to keep, files to keep).
fn check(node: &Node, op: &str, b: &Value) -> Result<(String, String, Value, Vec<(Vec<u8>, String)>), String> {
    let p = crate::arcledger::chain()?;
    let (_, tag, _) = crate::arcledger::sender(node, p)?;
    let tag = tag.ok_or("this wallet has no @tag on the ARCADE chain yet: claim one first")?;
    let exists = |f: &str| crate::arcledger::repo_row(node, f).is_some();
    let title = |t: &str| t.chars().take(80).collect::<String>();
    match op {
        "push" => {
            let f = full(s(b, "repo"), Some(&tag))?;
            let owner = f.split('/').next().unwrap_or("");
            if owner != tag && !crate::arcledger::repo(node, &f).ok().and_then(|r| r["maintainers"].as_array().cloned()).unwrap_or_default().iter().any(|m| m == tag.as_str()) {
                return Err(format!("@{tag} can't push to @{f}: fork it first"));
            }
            let mut files = Vec::new();
            let mut total = 0;
            for x in b["files"].as_array().ok_or("files required")? {
                use base64::Engine as _;
                let bytes = base64::engine::general_purpose::STANDARD.decode(s(x, "b64")).map_err(|_| "a file isn't base64")?;
                total += bytes.len();
                let kind = match s(x, "type") {
                    t @ ("application/x-git-packed-objects" | "application/x-git-packed-objects-toc" | "application/json") => t.to_string(),
                    t => return Err(format!("a push carries git packs, their indexes and a manifest, not {t}")),
                };
                files.push((bytes, kind));
            }
            if total > MAX_PUSH {
                return Err(format!("a push from an app is at most {} MB at once", MAX_PUSH / 1_000_000));
            }
            let mut m: Value = files.last().filter(|f| f.1 == "application/json").and_then(|f| serde_json::from_slice(&f.0).ok()).ok_or("the last file is the manifest")?;
            if m["git"] != "manifest" || !m["refs"].is_object() || !m["packs"].is_array() {
                return Err("the manifest isn't a git manifest".into());
            }
            // A page can't make ARCADE blob ids: the manifest names each new pack by its name (its checksum) with
            // empty ids, and they are filled in here from the pack and index this push carries.
            let net = crate::arcsvc::svc_net().ok_or("the ARCADE chain isn't running here")?;
            let id_of = |b: &[u8], kind: &str| hex::encode(arcade_arc::blob::Record::of(b, arcade_arc::blob::Class::Permanent, arcade_arc::blob::Encoding::Plain, kind).id(&net));
            let mut ids = std::collections::HashMap::new();
            for w in files[..files.len() - 1].chunks(2) {
                let [(pack, pk), (idx, ik)] = w else { return Err("packs come with their indexes".into()) };
                if pk != "application/x-git-packed-objects" || ik != "application/x-git-packed-objects-toc" || pack.len() < 32 || &pack[..4] != b"PACK" {
                    return Err("packs come with their indexes".into());
                }
                ids.insert(hex::encode(&pack[pack.len() - 20..]), (id_of(pack, pk), id_of(idx, ik)));
            }
            for pk in m["packs"].as_array_mut().into_iter().flatten() {
                let name = s(pk, "name").to_string();
                if let Some((p, i)) = ids.get(&name) {
                    pk["pack"] = json!(p);
                    pk["idx"] = json!(i);
                }
                for k in ["pack", "idx"] {
                    let held = hex::decode(s(pk, k)).ok().and_then(|b| <[u8; 32]>::try_from(b).ok()).is_some_and(|b| crate::arcsvc::holds_blob(&b));
                    if !held && !ids.values().any(|(p, i)| p == s(pk, k) || i == s(pk, k)) {
                        return Err(format!("the manifest names pack {name}, which isn't on the ARCADE chain"));
                    }
                }
            }
            let last = files.len() - 1;
            files[last].0 = serde_json::to_vec(&m).map_err(|e| e.to_string())?;
            let branch = s(b, "branch").to_string();
            let msg = title(s(b, "message"));
            if let Some(n) = b["merge"].as_u64() {
                let it = crate::arcledger::items(node, &json!({"repo": f, "number": n})).ok().and_then(|v| v["items"].get(0).cloned()).ok_or(format!("no pull request #{n} on @{f}"))?;
                if it["kind"] != "pull" || it["state"] != "open" {
                    return Err(format!("#{n} isn't an open pull request"));
                }
                let what = format!("merge pull request #{n} into @{f} ({} new KB)", total.div_ceil(1000));
                return Ok((what, f.clone(), json!({"repo": f, "branch": branch, "message": msg, "merge": n}), files));
            }
            // Rules version 4 (ARCADE #10): the push names the manifest it was built on: the page's `base` when it
            // gives one ("" for a new repository), else its manifest's `prev` (null for a new repository: the Git app
            // sets it to the manifest it read), else the repository's manifest now, when the request is made. The
            // ledger refuses it if the repository moves on before it lands, unless `force`.
            let base = match (b["base"].as_str(), m.get("prev")) {
                (Some(x), _) => x.trim().to_lowercase(),
                (None, Some(Value::String(p))) => p.trim().to_lowercase(),
                (None, Some(Value::Null)) => String::new(),
                _ => crate::arcledger::repo_manifest(node, &f).unwrap_or_default(),
            };
            if !base.is_empty() && !(base.len() == 64 && base.bytes().all(|c| c.is_ascii_hexdigit())) {
                return Err("base is a manifest blob id (64 hex), or \"\" for a new repository".into());
            }
            let force = b["force"] == true;
            let what = format!("{}push {}{} to @{f} ({} new KB)", if force { "force-" } else { "" }, if msg.is_empty() { "a commit".into() } else { format!("“{msg}”") },
                if branch.is_empty() { String::new() } else { format!(" on {branch}") }, total.div_ceil(1000));
            Ok((what, f.clone(), json!({"repo": f, "branch": branch, "message": msg, "new": !exists(&f), "base": base, "force": force}), files))
        }
        "fork" => {
            let src = full(s(b, "from"), None)?;
            if !exists(&src) {
                return Err(format!("no repository @{src} on the ARCADE chain"));
            }
            let name = if s(b, "name").is_empty() { src.split('/').nth(1).unwrap_or("").to_string() } else { s(b, "name").to_string() };
            let f = full(&name, Some(&tag))?;
            if exists(&f) {
                return Err(format!("@{f} already exists"));
            }
            Ok((format!("fork @{src} as @{f}"), f.clone(), json!({"from": src, "name": f}), vec![]))
        }
        "maintainer" => {
            let f = full(s(b, "repo"), Some(&tag))?;
            let add = b["add"].as_bool().unwrap_or(true);
            let t = s(b, "tag").trim_start_matches('@').to_lowercase();
            if f.split('/').next() != Some(tag.as_str()) {
                return Err("only the owner names maintainers".into());
            }
            Ok((format!("{} @{t} {} the maintainers of @{f}", if add { "add" } else { "remove" }, if add { "to" } else { "from" }), f.clone(), json!({"repo": f, "tag": t, "add": add}), vec![]))
        }
        "item_open" => {
            let f = full(s(b, "repo"), None)?;
            if !exists(&f) {
                return Err(format!("no repository @{f} on the ARCADE chain"));
            }
            let kind = if matches!(s(b, "kind"), "pull" | "pr") { "pull" } else { "issue" };
            if s(b, "title").trim().is_empty() {
                return Err("a title is needed".into());
            }
            if s(b, "body").len() > 60_000 {
                return Err("the text is at most 60 KB".into());
            }
            let mut a = json!({"repo": f, "kind": kind, "title": s(b, "title").chars().take(256).collect::<String>(), "body": s(b, "body")});
            for k in ["from", "manifest", "head"] {
                if !b[k].is_null() {
                    a[k] = b[k].clone();
                }
            }
            let what = format!("open {} “{}” on @{f}", if kind == "pull" { "a pull request" } else { "an issue" }, title(s(b, "title")));
            Ok((what, f, a, vec![]))
        }
        "item_comment" => {
            let f = full(s(b, "repo"), None)?;
            let n = b["number"].as_u64().ok_or("number required")?;
            if s(b, "body").len() > 60_000 {
                return Err("the text is at most 60 KB".into());
            }
            let review = match s(b, "review") {
                "" | "comment" => None,
                r @ ("approve" | "changes") => Some(r.to_string()),
                r => return Err(format!("review is approve or changes, not {r}")),
            };
            let what = match review.as_deref() {
                Some("approve") => format!("approve #{n} on @{f}"),
                Some(_) => format!("ask for changes on #{n} of @{f}"),
                None => format!("comment on #{n} of @{f}"),
            };
            Ok((what, f.clone(), json!({"repo": f, "number": n, "body": s(b, "body"), "review": review}), vec![]))
        }
        "item_state" => {
            let f = full(s(b, "repo"), None)?;
            let n = b["number"].as_u64().ok_or("number required")?;
            let st = s(b, "state");
            if !matches!(st, "open" | "closed" | "merged") {
                return Err("state is open, closed or merged".into());
            }
            if st == "merged" && s(b, "manifest").len() != 64 {
                return Err("a merge names the manifest the repository moves to".into());
            }
            let verb = match st { "open" => "reopen", "closed" => "close", _ => "merge" };
            Ok((format!("{verb} #{n} on @{f}"), f.clone(), json!({"repo": f, "number": n, "state": st, "manifest": b["manifest"]}), vec![]))
        }
        _ => Err(format!("no repository op called {op}")),
    }
}

/// `POST /r/repo/<op>` from app page `page`: (status, the request or {"error"}).
pub fn file(node: &Node, page: &str, op: &str, b: &Value) -> (u16, Value) {
    let (what, repo, args, files) = match check(node, op, b) {
        Ok(x) => x,
        Err(e) => return (400, json!({"error": e})),
    };
    let id = arcade_mesh::random::hex(8);
    if !files.is_empty() {
        let d = dir(node, &id);
        if let Err(e) = std::fs::create_dir_all(&d) {
            return (400, json!({"error": format!("can't keep the push: {e}")}));
        }
        for (i, (bytes, kind)) in files.iter().enumerate() {
            if std::fs::write(d.join(format!("{i}")), bytes).and_then(|_| std::fs::write(d.join(format!("{i}.type")), kind)).is_err() {
                return (400, json!({"error": "can't keep the push"}));
            }
        }
    }
    let r = json!({"id": id, "page": page, "kind": "repo", "op": op, "args": args, "files": files.len(), "what": what,
                   "to": format!("@{repo}"), "to_tag": Value::Null, "units": 0, "status": "pending", "created": arcade_app::now(),
                   "txid": Value::Null, "error": Value::Null, "repo": {"state": "asked"}});
    crate::walletsvc::file_other(node, r)
}

fn files(node: &Node, id: &str, n: u64) -> Result<Vec<(Vec<u8>, String, arcade_arc::blob::Class, arcade_arc::blob::Encoding)>, String> {
    let d = dir(node, id);
    (0..n).map(|i| {
        let bytes = std::fs::read(d.join(format!("{i}"))).map_err(|_| "the push's files are gone".to_string())?;
        let kind = std::fs::read_to_string(d.join(format!("{i}.type"))).unwrap_or_default();
        Ok((bytes, kind, arcade_arc::blob::Class::Permanent, arcade_arc::blob::Encoding::Plain))
    }).collect()
}

/// What approving would cost, without sending: (fee, outputs) for the card.
pub fn preview(node: &Node, r: &Value) -> Result<(u64, Vec<Value>), String> {
    let record = arcade_arc::params::MIN_FEE_PER_KB; // a record: well under 1 kB at twice the floor
    let fee = match s(r, "op") {
        "push" => {
            let v = crate::coinsend::publish_blobs(node, &files(node, s(r, "id"), r["files"].as_u64().unwrap_or(0))?, false)?;
            v["fee"].as_u64().unwrap_or(0) + record
        }
        "item_open" | "item_comment" => {
            let text = serde_json::to_vec(&r["args"]).unwrap_or_default();
            let v = crate::coinsend::publish_blob(node, &text, "application/json", arcade_arc::blob::Class::Permanent, arcade_arc::blob::Encoding::Plain, false)?;
            v["fee"].as_u64().unwrap_or(0) + record
        }
        _ => record,
    };
    Ok((fee, vec![json!({"data": true, "value": 0})]))
}

/// The wallet's coins are tied up in transactions not yet in a block: wait and try again.
fn busy(e: &str) -> bool {
    ["not enough", "could not build", "spent by another", "doesn't exist or is spent", "no coins"].iter().any(|w| e.contains(w))
}

fn retry<T>(stop: std::time::Instant, mut f: impl FnMut() -> Result<T, String>) -> Result<T, String> {
    loop {
        match f() {
            Err(e) if busy(&e) && std::time::Instant::now() < stop => std::thread::sleep(std::time::Duration::from_secs(10)),
            r => return r,
        }
    }
}

/// The request's progress, beside its status (which only the wallet's approval sets: a note never puts a request back
/// to pending, so it can't be approved twice).
fn note(node: &Node, id: &str, state: &str, step: &str, txids: &[String], err: Option<&str>) {
    if let Some(mut r) = crate::walletsvc::get_request(node, id) {
        if r["status"] == "pending" {
            r["status"] = json!("sent");
        }
        r["repo"] = json!({"state": state, "step": step, "txids": txids, "error": err});
        crate::walletsvc::put_request(node, &r);
    }
}

/// Approved: the work goes on in the background (records wait for free coins, a push for its manifest's block).
pub fn start(node: &Node, r: &Value) -> Result<(String, u64, Vec<Value>), String> {
    let n = node.self_arc();
    let r = r.clone();
    std::thread::spawn(move || {
        let id = s(&r, "id").to_string();
        // The approval saves the request as sent once this returns: wait for that, so what this thread notes isn't
        // written over by it.
        for _ in 0..200 {
            if crate::walletsvc::get_request(&n, &id).is_some_and(|x| x["status"] != "pending") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let a = &r["args"];
        let stop = std::time::Instant::now() + std::time::Duration::from_secs(WAIT_SECS);
        let mut txids: Vec<String> = Vec::new();
        let tx = |v: &Value| v["txid"].as_str().unwrap_or("").to_string();
        let res: Result<(), String> = (|| {
            match s(&r, "op") {
                "push" => {
                    note(&n, &id, "working", "publishing the packs", &txids, None);
                    let items = files(&n, &id, r["files"].as_u64().unwrap_or(0))?;
                    let v = retry(stop, || crate::coinsend::publish_blobs(&n, &items, true))?;
                    txids.push(tx(&v));
                    let manifest = v["blobs"].as_array().and_then(|b| b.last()).map(|b| s(b, "blob").to_string()).ok_or("the manifest has no id")?;
                    note(&n, &id, "working", "waiting for the packs to be in a block", &txids, None);
                    let m: [u8; 32] = hex::decode(&manifest).ok().and_then(|b| b.try_into().ok()).ok_or("bad manifest id")?;
                    while !crate::arcsvc::holds_blob(&m) {
                        if std::time::Instant::now() > stop {
                            return Err("the packs weren't in a block within 45 minutes".into());
                        }
                        std::thread::sleep(std::time::Duration::from_secs(5));
                    }
                    note(&n, &id, "working", "pointing the repository at the new commit", &txids, None);
                    let v = match a["merge"].as_u64() {
                        Some(num) => retry(stop, || crate::arcledger::item_state(&n, &json!({"repo": s(a, "repo"), "number": num, "state": "merged", "manifest": manifest})))?,
                        None => retry(stop, || crate::arcledger::repo_push(&n, s(a, "repo"), &manifest, a["base"].as_str(), a["force"] == true))?,
                    };
                    txids.push(tx(&v));
                    let _ = std::fs::remove_dir_all(dir(&n, &id));
                }
                "fork" => txids.push(tx(&retry(stop, || crate::arcledger::repo_fork(&n, s(a, "from"), s(a, "name")))?)),
                "maintainer" => txids.push(tx(&retry(stop, || crate::arcledger::repo_maintainer(&n, s(a, "repo"), s(a, "tag"), a["add"].as_bool().unwrap_or(true)))?)),
                "item_open" => txids.push(tx(&retry(stop, || crate::arcledger::item_open(&n, a))?)),
                "item_comment" => txids.push(tx(&retry(stop, || crate::arcledger::item_comment(&n, a))?)),
                "item_state" => {
                    let mut a = a.clone();
                    if a["manifest"].is_null() || a["manifest"] == "" {
                        a.as_object_mut().map(|o| o.remove("manifest"));
                    }
                    txids.push(tx(&retry(stop, || crate::arcledger::item_state(&n, &a))?))
                }
                op => return Err(format!("no repository op called {op}")),
            }
            Ok(())
        })();
        match res {
            Ok(()) => note(&n, &id, "done", "sent", &txids, None),
            Err(e) => note(&n, &id, "failed", "", &txids, Some(&e)),
        }
    });
    Ok((String::new(), 0, vec![]))
}

/// `GET /r/repo/req/<id>` for the page that filed it.
pub fn status(node: &Node, page: &str, id: &str) -> (u16, Value) {
    match crate::walletsvc::get_request(node, id) {
        Some(r) if r["page"] == page && r["kind"] == "repo" => (200, json!({"id": id, "status": r["status"], "op": r["op"], "what": r["what"], "repo": r["repo"], "error": r["error"]})),
        _ => (404, json!({"error": "no such request"})),
    }
}
