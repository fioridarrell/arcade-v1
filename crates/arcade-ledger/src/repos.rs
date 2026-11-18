//! Git repositories under @tags (docs/ARCADECHAIN.md §15 step 9; the ARCADE chain only).
//!
//! A repository is `@owner/name`: its state is a git manifest blob (crates/arcaded/src/arcgit.rs) and its ref updates
//! are records signed by the sender's key, valid when the sender's @tag is the owner or one of its maintainers.
//!
//! Wire: AnyData payload (version u16, type u16 = 200) whose data is `"REPO"` 1 op ‖ fields:
//! - op 1, **push**: name (u8 len ‖ utf-8, `owner/name` or `name` under the sender's own tag) ‖ manifest (32 bytes).
//!   The owner's first push makes the repository.
//! - op 2, **maintainer**: name ‖ add (1) or remove (0) ‖ tag (u8 len ‖ utf-8). The owner only.
//! - op 3, **fork**: name (the new one, under the sender's tag) ‖ source `owner/name`: the new repository starts at the
//!   source's manifest and remembers where it came from.
//!
//! Issues, pull requests, comments and reviews (§15 step 10) are records on a repository whose text is a blob (JSON:
//! `{"title", "body", ...}`, stored once and fetched like any blob), so a record stays a few dozen bytes:
//! - op 4, **open**: name ‖ kind (1 issue, 2 pull request) ‖ body blob (32). Numbered 1, 2, ... per repository in chain
//!   order. A pull request's body names the branch it brings (`from`: a repository, `manifest`, `head`).
//! - op 5, **comment**: name ‖ number (u32) ‖ verdict (0 a comment, 1 approve, 2 changes asked: a review) ‖ body blob.
//!   Reviews count from the owner and maintainers only; anyone with an @tag comments.
//! - op 6, **state**: name ‖ number ‖ state (1 open, 2 closed, 3 merged) ‖ manifest (32, a merge's result, else zeros).
//!   The author opens and closes their own; the owner and maintainers close, reopen and merge (a merge also moves the
//!   repository to the manifest).
//! - op 7, **push onto** (rules version 4, ARCADE issue #10): name ‖ manifest (32) ‖ base (32: the manifest the push was
//!   built on, zeros for a repository's first push) ‖ flags (u8, bit 0 = force). From the height rules version 4 starts
//!   ([`set_base_check_from`]) a push whose base isn't the repository's manifest when the record is applied is refused
//!   unless forced, so a push made from an out-of-date copy can't silently replace a newer one. Below that height op 7
//!   is unknown (refused, as before version 4). From that height an op 1 push (no base) only makes a new repository;
//!   onto an existing one it is refused. Below it op 1 pushes replace as they always did.

use crate::tags::TYPE_ANY_DATA;
use std::collections::BTreeMap;
use std::sync::RwLock;

static BASE_FROM: RwLock<BTreeMap<u16, u64>> = RwLock::new(BTreeMap::new());

/// The height from which chain `c`'s pushes are checked against their base (rules version 4,
/// `arcade_arc::params::rules_v4`); None = never. Set by the ARCADE chain's service when it starts, before any block is
/// indexed (as `tags::set_expiry`).
pub fn set_base_check_from(c: u16, from: Option<u64>) {
    let mut m = BASE_FROM.write().unwrap();
    match from {
        Some(h) => m.insert(c, h),
        None => m.remove(&c),
    };
}

/// Whether a push in chain `c`'s block at `height` is checked against its base (op 7 known).
pub fn base_checked(c: u16, height: u64) -> bool {
    BASE_FROM.read().unwrap().get(&c).is_some_and(|from| height >= *from)
}

/// The base that names "no repository yet".
pub const NO_BASE: [u8; 32] = [0; 32];

pub const MARKER: &[u8; 4] = b"REPO";
pub const OP_PUSH: u8 = 1;
pub const OP_MAINTAINER: u8 = 2;
pub const OP_FORK: u8 = 3;
pub const OP_OPEN: u8 = 4;
pub const OP_COMMENT: u8 = 5;
pub const OP_STATE: u8 = 6;
pub const OP_PUSH_ONTO: u8 = 7;

pub const KIND_ISSUE: u8 = 1;
pub const KIND_PULL: u8 = 2;
pub const STATE_OPEN: u8 = 1;
pub const STATE_CLOSED: u8 = 2;
pub const STATE_MERGED: u8 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoOp {
    Push { name: String, manifest: [u8; 32] },
    /// A push naming the manifest it was built on (op 7, rules version 4).
    PushOnto { name: String, manifest: [u8; 32], base: [u8; 32], force: bool },
    Maintainer { name: String, add: bool, tag: String },
    Fork { name: String, from: String },
    Open { name: String, kind: u8, body: [u8; 32] },
    Comment { name: String, number: u32, verdict: u8, body: [u8; 32] },
    State { name: String, number: u32, state: u8, manifest: [u8; 32] },
}

/// Why a repository name (the part after `owner/`) isn't acceptable.
pub fn name_problem(name: &str) -> Option<&'static str> {
    if name.is_empty() || name.len() > 40 {
        return Some("a repository name is 1 to 40 characters");
    }
    if !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_' || b == b'.') {
        return Some("a repository name is a-z, 0-9, - _ and .");
    }
    if name.starts_with('.') {
        return Some("a repository name doesn't start with a dot");
    }
    None
}

/// `owner/name` from `@owner/name`, `owner/name` or (with the sender's tag) `name`.
pub fn full_name(name: &str, sender_tag: Option<&str>) -> Result<String, &'static str> {
    let n = name.trim().trim_start_matches('@').to_lowercase();
    let (owner, repo) = match n.split_once('/') {
        Some((o, r)) => (o.to_string(), r.to_string()),
        None => (sender_tag.ok_or("the sender has no @tag")?.to_string(), n),
    };
    if let Some(p) = crate::tags::tag_problem(&owner) {
        return Err(p);
    }
    if let Some(p) = name_problem(&repo) {
        return Err(p);
    }
    Ok(format!("{owner}/{repo}"))
}

fn take_str<'a>(d: &mut &'a [u8]) -> Result<&'a str, &'static str> {
    let (&n, rest) = d.split_first().ok_or("truncated")?;
    let s = rest.get(..n as usize).ok_or("truncated")?;
    *d = &rest[n as usize..];
    std::str::from_utf8(s).map_err(|_| "not utf-8")
}

/// A repository record. None = not one.
pub fn parse(payload: &[u8]) -> Option<Result<RepoOp, &'static str>> {
    if payload.len() < 4 || u16::from_be_bytes([payload[2], payload[3]]) != TYPE_ANY_DATA {
        return None;
    }
    let d = &payload[4..];
    if d.len() < 6 || &d[..4] != MARKER {
        return None;
    }
    Some((|| {
        if d[4] != 1 {
            return Err("unknown repository record version");
        }
        let mut rest = &d[6..];
        let name = take_str(&mut rest)?.to_string();
        match d[5] {
            OP_PUSH => Ok(RepoOp::Push { name, manifest: rest.get(..32).ok_or("truncated")?.try_into().map_err(|_| "truncated")? }),
            OP_PUSH_ONTO => {
                let manifest: [u8; 32] = rest.get(..32).ok_or("truncated")?.try_into().map_err(|_| "truncated")?;
                let base: [u8; 32] = rest.get(32..64).ok_or("truncated")?.try_into().map_err(|_| "truncated")?;
                let flags = *rest.get(64).ok_or("truncated")?;
                if flags > 1 {
                    return Err("unknown push flags");
                }
                Ok(RepoOp::PushOnto { name, manifest, base, force: flags & 1 == 1 })
            }
            OP_MAINTAINER => {
                let (&add, mut r2) = rest.split_first().ok_or("truncated")?;
                let tag = take_str(&mut r2)?.trim_start_matches('@').to_lowercase();
                Ok(RepoOp::Maintainer { name, add: add == 1, tag })
            }
            OP_FORK => Ok(RepoOp::Fork { name, from: take_str(&mut rest)?.to_string() }),
            OP_OPEN => {
                let (&kind, r2) = rest.split_first().ok_or("truncated")?;
                if kind != KIND_ISSUE && kind != KIND_PULL {
                    return Err("unknown item kind");
                }
                Ok(RepoOp::Open { name, kind, body: r2.get(..32).ok_or("truncated")?.try_into().map_err(|_| "truncated")? })
            }
            OP_COMMENT | OP_STATE => {
                let number = u32::from_be_bytes(rest.get(..4).ok_or("truncated")?.try_into().map_err(|_| "truncated")?);
                let b = *rest.get(4).ok_or("truncated")?;
                let h: [u8; 32] = rest.get(5..37).ok_or("truncated")?.try_into().map_err(|_| "truncated")?;
                if d[5] == OP_COMMENT {
                    if b > 2 {
                        return Err("unknown review verdict");
                    }
                    Ok(RepoOp::Comment { name, number, verdict: b, body: h })
                } else {
                    if !(STATE_OPEN..=STATE_MERGED).contains(&b) {
                        return Err("unknown state");
                    }
                    Ok(RepoOp::State { name, number, state: b, manifest: h })
                }
            }
            _ => Err("unknown repository op"),
        }
    })())
}

fn head(op: u8, name: &str) -> Vec<u8> {
    let mut p = vec![0, 0];
    p.extend_from_slice(&TYPE_ANY_DATA.to_be_bytes());
    p.extend_from_slice(MARKER);
    p.extend_from_slice(&[1, op, name.len() as u8]);
    p.extend_from_slice(name.as_bytes());
    p
}

pub fn push_payload(name: &str, manifest: &[u8; 32]) -> Vec<u8> {
    let mut p = head(OP_PUSH, name);
    p.extend_from_slice(manifest);
    p
}

/// A push built on manifest `base` ([`NO_BASE`] for a new repository), op 7: only for blocks where
/// [`base_checked`] holds (rules version 4); below that, [`push_payload`].
pub fn push_onto_payload(name: &str, manifest: &[u8; 32], base: &[u8; 32], force: bool) -> Vec<u8> {
    let mut p = head(OP_PUSH_ONTO, name);
    p.extend_from_slice(manifest);
    p.extend_from_slice(base);
    p.push(force as u8);
    p
}

pub fn maintainer_payload(name: &str, add: bool, tag: &str) -> Vec<u8> {
    let mut p = head(OP_MAINTAINER, name);
    p.push(add as u8);
    p.push(tag.len() as u8);
    p.extend_from_slice(tag.as_bytes());
    p
}

pub fn open_payload(name: &str, kind: u8, body: &[u8; 32]) -> Vec<u8> {
    let mut p = head(OP_OPEN, name);
    p.push(kind);
    p.extend_from_slice(body);
    p
}

pub fn comment_payload(name: &str, number: u32, verdict: u8, body: &[u8; 32]) -> Vec<u8> {
    let mut p = head(OP_COMMENT, name);
    p.extend_from_slice(&number.to_be_bytes());
    p.push(verdict);
    p.extend_from_slice(body);
    p
}

pub fn state_payload(name: &str, number: u32, state: u8, manifest: &[u8; 32]) -> Vec<u8> {
    let mut p = head(OP_STATE, name);
    p.extend_from_slice(&number.to_be_bytes());
    p.push(state);
    p.extend_from_slice(manifest);
    p
}

pub fn fork_payload(name: &str, from: &str) -> Vec<u8> {
    let mut p = head(OP_FORK, name);
    p.push(from.len() as u8);
    p.extend_from_slice(from.as_bytes());
    p
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub manifest: [u8; 32],
    pub fork: Option<String>,
    pub pushes: u64,
}

/// An issue or pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub kind: u8,
    pub by: String,
    pub state: u8,
    pub body: [u8; 32],
}

pub trait RepoStore {
    type E;
    /// The next item number of a repository (1 for the first), counting this one taken.
    fn next_number(&mut self, full: &str) -> Result<u32, Self::E>;
    fn item(&self, full: &str, n: u32) -> Result<Option<Item>, Self::E>;
    fn set_item(&mut self, full: &str, n: u32, it: &Item) -> Result<(), Self::E>;
    /// A comment, review or state change on item `n`, in chain order.
    fn event(&mut self, full: &str, n: u32, by: &str, what: &str, verdict: u8, hash: &[u8; 32]) -> Result<(), Self::E>;
    fn repo(&self, full: &str) -> Result<Option<Repo>, Self::E>;
    fn set_repo(&mut self, full: &str, r: &Repo, by: &str) -> Result<(), Self::E>;
    fn is_maintainer(&self, full: &str, tag: &str) -> Result<bool, Self::E>;
    fn set_maintainer(&mut self, full: &str, tag: &str, on: bool) -> Result<(), Self::E>;
}

/// Apply one op as below rules version 4 (op 7 unknown). Ok(Err) = invalid, nothing changed.
pub fn apply<S: RepoStore>(s: &mut S, op: &RepoOp, sender_tag: Option<&str>) -> Result<Result<(), &'static str>, S::E> {
    apply_at(s, op, sender_tag, false)
}

/// Apply one op from a sender whose @tag (if any) is `sender_tag`, in a block where rules version 4 does (`v4`) or
/// doesn't apply. Ok(Err) = invalid, nothing changed.
pub fn apply_at<S: RepoStore>(s: &mut S, op: &RepoOp, sender_tag: Option<&str>, v4: bool) -> Result<Result<(), &'static str>, S::E> {
    if matches!(op, RepoOp::PushOnto { .. }) && !v4 {
        return Ok(Err("unknown repository op"));
    }
    let Some(me) = sender_tag else { return Ok(Err("repository records come from an @tag")) };
    let full = |n: &str| full_name(n, Some(me));
    match op {
        RepoOp::Push { name, manifest } | RepoOp::PushOnto { name, manifest, .. } => {
            let f = match full(name) {
                Ok(f) => f,
                Err(e) => return Ok(Err(e)),
            };
            let owner = f.split('/').next().unwrap_or("");
            let allowed = owner == me || s.is_maintainer(&f, me)?;
            if !allowed {
                return Ok(Err("only the owner and its maintainers push to a repository"));
            }
            let now = s.repo(&f)?;
            // At version 4 a push onto an existing repository names its base (op 7); op 1 only makes a new one.
            if v4 && matches!(op, RepoOp::Push { .. }) && now.is_some() {
                return Ok(Err("from rules version 4 a push to an existing repository names its base (op 7)"));
            }
            if let RepoOp::PushOnto { base, force: false, .. } = op {
                let current = now.as_ref().map_or(NO_BASE, |r| r.manifest);
                if *base != current {
                    return Ok(Err(if now.is_none() {
                        "a repository's first push names no base"
                    } else {
                        "the push wasn't built on the repository's current manifest (fetch and push again, or force it)"
                    }));
                }
            }
            let r = match now {
                Some(r) => Repo { manifest: *manifest, pushes: r.pushes + 1, ..r },
                None if owner == me => Repo { manifest: *manifest, fork: None, pushes: 1 },
                None => return Ok(Err("no such repository")),
            };
            s.set_repo(&f, &r, me)?;
            Ok(Ok(()))
        }
        RepoOp::Maintainer { name, add, tag } => {
            let f = match full(name) {
                Ok(f) => f,
                Err(e) => return Ok(Err(e)),
            };
            if f.split('/').next() != Some(me) {
                return Ok(Err("only the owner names maintainers"));
            }
            if s.repo(&f)?.is_none() {
                return Ok(Err("no such repository"));
            }
            if crate::tags::tag_problem(tag).is_some() {
                return Ok(Err("not a tag"));
            }
            s.set_maintainer(&f, tag, *add)?;
            Ok(Ok(()))
        }
        RepoOp::Fork { name, from } => {
            let (f, src) = match (full(name), full_name(from, None)) {
                (Ok(f), Ok(src)) => (f, src),
                (Err(e), _) | (_, Err(e)) => return Ok(Err(e)),
            };
            if f.split('/').next() != Some(me) {
                return Ok(Err("a fork goes under your own tag"));
            }
            if s.repo(&f)?.is_some() {
                return Ok(Err("you already have a repository of that name"));
            }
            let Some(source) = s.repo(&src)? else { return Ok(Err("no such repository to fork")) };
            s.set_repo(&f, &Repo { manifest: source.manifest, fork: Some(src), pushes: 0 }, me)?;
            Ok(Ok(()))
        }
        RepoOp::Open { name, kind, body } => {
            let Ok(f) = full_name(name, None) else { return Ok(Err("issues and pull requests name the repository as owner/name")) };
            if s.repo(&f)?.is_none() {
                return Ok(Err("no such repository"));
            }
            let n = s.next_number(&f)?;
            s.set_item(&f, n, &Item { kind: *kind, by: me.into(), state: STATE_OPEN, body: *body })?;
            s.event(&f, n, me, "open", 0, body)?;
            Ok(Ok(()))
        }
        RepoOp::Comment { name, number, verdict, body } => {
            let Ok(f) = full_name(name, None) else { return Ok(Err("name the repository as owner/name")) };
            if s.item(&f, *number)?.is_none() {
                return Ok(Err("no such issue or pull request"));
            }
            if *verdict != 0 && !(f.starts_with(&format!("{me}/")) || s.is_maintainer(&f, me)?) {
                return Ok(Err("reviews are the owner's and its maintainers'; anyone comments"));
            }
            s.event(&f, *number, me, if *verdict == 0 { "comment" } else { "review" }, *verdict, body)?;
            Ok(Ok(()))
        }
        RepoOp::State { name, number, state, manifest } => {
            let Ok(f) = full_name(name, None) else { return Ok(Err("name the repository as owner/name")) };
            let Some(mut it) = s.item(&f, *number)? else { return Ok(Err("no such issue or pull request")) };
            let keeper = f.starts_with(&format!("{me}/")) || s.is_maintainer(&f, me)?;
            match *state {
                STATE_MERGED if it.kind != KIND_PULL => return Ok(Err("only a pull request is merged")),
                STATE_MERGED if !keeper => return Ok(Err("the owner and its maintainers merge")),
                STATE_MERGED if it.state != STATE_OPEN => return Ok(Err("only an open pull request is merged")),
                _ if !keeper && it.by != me => return Ok(Err("the author, the owner and its maintainers change its state")),
                _ if it.state == STATE_MERGED => return Ok(Err("a merged pull request stays merged")),
                _ => {}
            }
            if *state == STATE_MERGED {
                let Some(r) = s.repo(&f)? else { return Ok(Err("no such repository")) };
                s.set_repo(&f, &Repo { manifest: *manifest, pushes: r.pushes + 1, ..r }, me)?;
            }
            it.state = *state;
            s.set_item(&f, *number, &it)?;
            s.event(&f, *number, me, ["", "reopen", "close", "merge"][*state as usize], 0, manifest)?;
            Ok(Ok(()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    #[derive(Default)]
    struct Mem {
        repos: BTreeMap<String, Repo>,
        maint: BTreeSet<(String, String)>,
        items: BTreeMap<(String, u32), Item>,
        events: Vec<(String, u32, String, String, u8)>,
    }
    impl RepoStore for Mem {
        type E = ();
        fn next_number(&mut self, f: &str) -> Result<u32, ()> {
            Ok(self.items.keys().filter(|k| k.0 == f).count() as u32 + 1)
        }
        fn item(&self, f: &str, n: u32) -> Result<Option<Item>, ()> {
            Ok(self.items.get(&(f.into(), n)).cloned())
        }
        fn set_item(&mut self, f: &str, n: u32, it: &Item) -> Result<(), ()> {
            self.items.insert((f.into(), n), it.clone());
            Ok(())
        }
        fn event(&mut self, f: &str, n: u32, by: &str, what: &str, v: u8, _h: &[u8; 32]) -> Result<(), ()> {
            self.events.push((f.into(), n, by.into(), what.into(), v));
            Ok(())
        }
        fn repo(&self, f: &str) -> Result<Option<Repo>, ()> {
            Ok(self.repos.get(f).cloned())
        }
        fn set_repo(&mut self, f: &str, r: &Repo, _by: &str) -> Result<(), ()> {
            self.repos.insert(f.into(), r.clone());
            Ok(())
        }
        fn is_maintainer(&self, f: &str, t: &str) -> Result<bool, ()> {
            Ok(self.maint.contains(&(f.into(), t.into())))
        }
        fn set_maintainer(&mut self, f: &str, t: &str, on: bool) -> Result<(), ()> {
            if on {
                self.maint.insert((f.into(), t.into()));
            } else {
                self.maint.remove(&(f.into(), t.into()));
            }
            Ok(())
        }
    }

    #[test]
    fn records_read_back() {
        let m = [7u8; 32];
        assert_eq!(parse(&push_payload("demo", &m)), Some(Ok(RepoOp::Push { name: "demo".into(), manifest: m })));
        assert_eq!(parse(&maintainer_payload("alice/demo", true, "bob")), Some(Ok(RepoOp::Maintainer { name: "alice/demo".into(), add: true, tag: "bob".into() })));
        assert_eq!(parse(&fork_payload("demo", "alice/demo")), Some(Ok(RepoOp::Fork { name: "demo".into(), from: "alice/demo".into() })));
        assert_eq!(parse(&crate::tags::claim_payload("bob")), None);
        assert!(matches!(parse(&push_payload("demo", &m)[..20]), Some(Err(_))));
        assert_eq!(full_name("@Alice/Demo", None), Ok("alice/demo".into()));
        assert_eq!(full_name("demo", Some("bob")), Ok("bob/demo".into()));
        assert!(full_name("demo", None).is_err() && full_name("alice/.git", None).is_err() && full_name("alice/a b", None).is_err());
    }

    #[test]
    fn push_onto_reads_back_and_old_pushes_still_parse() {
        let (m, b) = ([7u8; 32], [8u8; 32]);
        // The op 1 record is byte for byte what it was, and reads as before.
        let old = push_payload("demo", &m);
        assert_eq!(old.len(), 4 + 4 + 2 + 1 + 4 + 32);
        assert_eq!(parse(&old), Some(Ok(RepoOp::Push { name: "demo".into(), manifest: m })));
        for force in [false, true] {
            assert_eq!(parse(&push_onto_payload("alice/demo", &m, &b, force)), Some(Ok(RepoOp::PushOnto { name: "alice/demo".into(), manifest: m, base: b, force })));
        }
        let p = push_onto_payload("demo", &m, &NO_BASE, false);
        assert!(matches!(parse(&p[..p.len() - 1]), Some(Err("truncated"))));
        let mut bad = p.clone();
        *bad.last_mut().unwrap() = 2;
        assert_eq!(parse(&bad), Some(Err("unknown push flags")));
    }

    #[test]
    fn a_push_is_checked_against_its_base_from_rules_v4() {
        let mut s = Mem::default();
        let (m1, m2, m3, m4) = ([1u8; 32], [2u8; 32], [3u8; 32], [4u8; 32]);
        let onto = |n: &str, m, base, force| RepoOp::PushOnto { name: n.into(), manifest: m, base, force };
        // Below version 4 op 7 is unknown, as before; nothing changes.
        assert_eq!(apply_at(&mut s, &onto("demo", m1, NO_BASE, false), Some("alice"), false), Ok(Err("unknown repository op")));
        assert_eq!(apply(&mut s, &onto("demo", m1, NO_BASE, true), Some("alice")), Ok(Err("unknown repository op")));
        assert!(s.repos.is_empty());
        // A first push names no base.
        assert_eq!(apply_at(&mut s, &onto("demo", m1, m2, false), Some("alice"), true), Ok(Err("a repository's first push names no base")));
        assert_eq!(apply_at(&mut s, &onto("demo", m1, NO_BASE, false), Some("alice"), true), Ok(Ok(())));
        assert_eq!(s.repos["alice/demo"], Repo { manifest: m1, fork: None, pushes: 1 });
        // Built on the manifest now: taken. Then a push built on the old one (the 2026-11-16 rollback) is refused.
        assert_eq!(apply_at(&mut s, &onto("demo", m2, m1, false), Some("alice"), true), Ok(Ok(())));
        assert_eq!(apply_at(&mut s, &onto("demo", m3, m1, false), Some("alice"), true),
            Ok(Err("the push wasn't built on the repository's current manifest (fetch and push again, or force it)")));
        assert_eq!(s.repos["alice/demo"], Repo { manifest: m2, fork: None, pushes: 2 });
        // Forced, it replaces as a push always did.
        assert_eq!(apply_at(&mut s, &onto("demo", m3, m1, true), Some("alice"), true), Ok(Ok(())));
        assert_eq!(s.repos["alice/demo"], Repo { manifest: m3, fork: None, pushes: 3 });
        // The owner/maintainer check still comes first.
        assert_eq!(apply_at(&mut s, &onto("alice/demo", m4, m3, true), Some("bob"), true), Ok(Err("only the owner and its maintainers push to a repository")));
        // An op 1 push (no base) onto an existing repository is refused at version 4, taken below it.
        let plain = |n: &str, m| RepoOp::Push { name: n.into(), manifest: m };
        assert_eq!(apply_at(&mut s, &plain("demo", m4), Some("alice"), true), Ok(Err("from rules version 4 a push to an existing repository names its base (op 7)")));
        assert_eq!(s.repos["alice/demo"].manifest, m3);
        assert_eq!(apply_at(&mut s, &plain("demo", m4), Some("alice"), false), Ok(Ok(())));
        assert_eq!(s.repos["alice/demo"], Repo { manifest: m4, fork: None, pushes: 4 });
        // An op 1 push that makes a new repository stays valid at version 4.
        assert_eq!(apply_at(&mut s, &plain("fresh", m1), Some("alice"), true), Ok(Ok(())));
        assert_eq!(s.repos["alice/fresh"], Repo { manifest: m1, fork: None, pushes: 1 });
        // A fork's first push is built on the fork's manifest (its source's).
        apply_at(&mut s, &RepoOp::Fork { name: "demo".into(), from: "alice/demo".into() }, Some("carol"), true).unwrap().unwrap();
        assert!(apply_at(&mut s, &onto("demo", m1, NO_BASE, false), Some("carol"), true).unwrap().is_err());
        assert_eq!(apply_at(&mut s, &onto("demo", m1, m4, false), Some("carol"), true), Ok(Ok(())));
    }

    #[test]
    fn the_base_check_starts_at_its_height() {
        set_base_check_from(9_998, Some(700));
        assert!(!base_checked(9_998, 699) && base_checked(9_998, 700) && base_checked(9_998, 5_000));
        assert!(!base_checked(9_997, 5_000));
        set_base_check_from(9_998, None);
        assert!(!base_checked(9_998, 5_000));
    }

    #[test]
    fn issues_pull_requests_reviews_and_merges() {
        let mut s = Mem::default();
        let (m1, m2, body) = ([1u8; 32], [2u8; 32], [9u8; 32]);
        apply(&mut s, &RepoOp::Push { name: "demo".into(), manifest: m1 }, Some("alice")).unwrap().unwrap();
        for (op, p) in [
            (RepoOp::Open { name: "alice/demo".into(), kind: KIND_PULL, body }, open_payload("alice/demo", KIND_PULL, &body)),
            (RepoOp::Comment { name: "alice/demo".into(), number: 1, verdict: 1, body }, comment_payload("alice/demo", 1, 1, &body)),
            (RepoOp::State { name: "alice/demo".into(), number: 1, state: STATE_MERGED, manifest: m2 }, state_payload("alice/demo", 1, STATE_MERGED, &m2)),
        ] {
            assert_eq!(parse(&p), Some(Ok(op)));
        }
        let open = |kind| RepoOp::Open { name: "alice/demo".into(), kind, body };
        assert_eq!(apply(&mut s, &open(KIND_ISSUE), Some("carol")), Ok(Ok(())));
        assert_eq!(apply(&mut s, &open(KIND_PULL), Some("bob")), Ok(Ok(())));
        assert_eq!(s.items[&("alice/demo".into(), 2)].by, "bob");
        assert_eq!(apply(&mut s, &RepoOp::Open { name: "nobody/x".into(), kind: KIND_ISSUE, body }, Some("bob")), Ok(Err("no such repository")));
        let review = |n, v| RepoOp::Comment { name: "alice/demo".into(), number: n, verdict: v, body };
        assert_eq!(apply(&mut s, &review(2, 0), Some("carol")), Ok(Ok(())));
        assert_eq!(apply(&mut s, &review(2, 1), Some("carol")), Ok(Err("reviews are the owner's and its maintainers'; anyone comments")));
        assert_eq!(apply(&mut s, &review(2, 1), Some("alice")), Ok(Ok(())));
        assert_eq!(apply(&mut s, &review(7, 0), Some("alice")), Ok(Err("no such issue or pull request")));
        let state = |n, st, m| RepoOp::State { name: "alice/demo".into(), number: n, state: st, manifest: m };
        assert_eq!(apply(&mut s, &state(1, STATE_MERGED, m2), Some("alice")), Ok(Err("only a pull request is merged")));
        assert_eq!(apply(&mut s, &state(2, STATE_MERGED, m2), Some("bob")), Ok(Err("the owner and its maintainers merge")));
        assert_eq!(apply(&mut s, &state(1, STATE_CLOSED, [0; 32]), Some("bob")), Ok(Err("the author, the owner and its maintainers change its state")));
        assert_eq!(apply(&mut s, &state(1, STATE_CLOSED, [0; 32]), Some("carol")), Ok(Ok(())));
        assert_eq!(apply(&mut s, &state(2, STATE_MERGED, m2), Some("alice")), Ok(Ok(())));
        assert_eq!(s.repos["alice/demo"].manifest, m2);
        assert_eq!(s.items[&("alice/demo".into(), 2)].state, STATE_MERGED);
        assert_eq!(apply(&mut s, &state(2, STATE_OPEN, [0; 32]), Some("alice")), Ok(Err("a merged pull request stays merged")));
        assert_eq!(s.events.iter().map(|e| e.3.as_str()).collect::<Vec<_>>(), ["open", "open", "comment", "review", "close", "merge"]);
    }

    #[test]
    fn owners_maintainers_and_forks() {
        let mut s = Mem::default();
        let (m1, m2, m3) = ([1u8; 32], [2u8; 32], [3u8; 32]);
        let push = |n: &str, m| RepoOp::Push { name: n.into(), manifest: m };
        assert_eq!(apply(&mut s, &push("demo", m1), None), Ok(Err("repository records come from an @tag")));
        assert_eq!(apply(&mut s, &push("demo", m1), Some("alice")), Ok(Ok(())));
        // Someone else can't push to it, nor make it under another's tag.
        assert_eq!(apply(&mut s, &push("alice/demo", m2), Some("bob")), Ok(Err("only the owner and its maintainers push to a repository")));
        assert_eq!(apply(&mut s, &push("alice/other", m2), Some("bob")), Ok(Err("only the owner and its maintainers push to a repository")));
        // A maintainer can, once the owner says so; only the owner says so.
        assert_eq!(apply(&mut s, &RepoOp::Maintainer { name: "alice/demo".into(), add: true, tag: "carol".into() }, Some("bob")), Ok(Err("only the owner names maintainers")));
        assert_eq!(apply(&mut s, &RepoOp::Maintainer { name: "demo".into(), add: true, tag: "bob".into() }, Some("alice")), Ok(Ok(())));
        assert_eq!(apply(&mut s, &push("alice/demo", m2), Some("bob")), Ok(Ok(())));
        assert_eq!(s.repos["alice/demo"], Repo { manifest: m2, fork: None, pushes: 2 });
        // A fork starts at the source and goes on by itself.
        assert_eq!(apply(&mut s, &RepoOp::Fork { name: "demo".into(), from: "@alice/demo".into() }, Some("carol")), Ok(Ok(())));
        assert_eq!(s.repos["carol/demo"], Repo { manifest: m2, fork: Some("alice/demo".into()), pushes: 0 });
        assert_eq!(apply(&mut s, &RepoOp::Fork { name: "demo".into(), from: "alice/demo".into() }, Some("carol")), Ok(Err("you already have a repository of that name")));
        assert_eq!(apply(&mut s, &push("demo", m3), Some("carol")), Ok(Ok(())));
        assert_eq!(s.repos["alice/demo"].manifest, m2);
        assert_eq!(apply(&mut s, &RepoOp::Fork { name: "x".into(), from: "nobody/none".into() }, Some("carol")), Ok(Err("no such repository to fork")));
    }
}
