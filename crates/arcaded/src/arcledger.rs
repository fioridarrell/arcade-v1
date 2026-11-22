//! The arcade ledger on the ARCADE chain (docs/ARCADECHAIN.md §15 step 7): the same `arcd` records as on the home chain
//! (tags, profiles, follows, tokens, orders, messages), indexed by the same engine (crate::arcsvc adds it), sent from
//! the wallet's tag key on ARCADE with ARCADE's coins, fees and pool. A record of up to 1 kB is one OP_RETURN output.

use crate::node::Node;
use arcade_chain::params::ChainParams;
use serde_json::{json, Value};

/// The ARCADE chain running here: its own copy, whose ledger tables the reads below look in.
pub fn chain() -> Result<&'static ChainParams, String> {
    if let Some(p) = crate::arcsvc::defined().filter(|p| crate::arcsvc::runs(p)) {
        return Ok(p);
    }
    // Borrowed only: this arcade has no ledger tables of its own to read, and says so rather than answer empty.
    if crate::arcsvc::usable_chain().is_some() {
        return Err("ARCADE is borrowed here, not installed: this read needs its ledger, so install ARCADE in Chains & Coins (writes work borrowed)".into());
    }
    Err("the ARCADE chain isn't running here".to_string())
}

/// The ARCADE chain records are written to: installed here or borrowed (Martin 2026-11-18: "It's just a transaction
/// that needs to be sent"). Coins, waiting change and the broadcast follow whichever copy stands in (claims::arc_coins,
/// arcsvc::waiting_outputs, ownwait::send_arc).
pub fn write_chain() -> Result<&'static ChainParams, String> {
    // Not reachable yet (arcaded starting, its borrowed copy still being set up): "starting" with a retry (2026-11-21).
    crate::arcsvc::usable_chain().ok_or_else(|| crate::arcsvc::reach_pending_error().unwrap_or_else(|| "the ARCADE chain isn't installed or borrowed here".to_string()))
}

// ---------------- reads while ARCADE is only borrowed (borrowed = installed, batch 4) ----------------
//
// A borrowing arcade has no ledger tables of its own. Each read below asks an arcade that keeps the chain's ledger
// (`explorer.arc_ledger {chain, op, args}`, forwarded like every ledger read), which runs it on its own tables and
// answers. The asking arcade first fills in everything that is about *this* wallet (its address, who it follows), so
// the keeper never answers about itself; keys never leave this arcade (the address book comes back sealed and is
// opened here).

/// None when this arcade's own copy answers; else the keeper's answer to `op`.
fn borrowed_read(node: &Node, op: &str, args: Value) -> Result<Option<Value>, String> {
    if chain().is_ok() {
        return Ok(None);
    }
    let p = write_chain()?;
    crate::explorer::handle(node, "explorer.arc_ledger", &json!({"chain": p.key, "op": op, "args": args})).map(Some)
}

/// `req` with this wallet's address filled in when it names no one (`address` or `tag`).
fn about_me(node: &Node, req: &Value) -> Result<Value, String> {
    let mut a = if req.is_object() { req.clone() } else { json!({}) };
    let named = a["address"].as_str().is_some_and(|s| !s.is_empty()) || a["tag"].as_str().is_some_and(|s| !s.is_empty());
    if !named {
        a["address"] = json!(sender(node, write_chain()?)?.0);
    }
    Ok(a)
}

/// The keeper's side of `explorer.arc_ledger`: the read `op` on this arcade's own ledger tables. Every read here is
/// about the address or tag it is given, never about this arcade's own wallet.
pub fn serve(node: &Node, req: &Value) -> Result<Value, String> {
    let p = chain()?;
    let c = p.chain_no;
    let a = &req["args"];
    let named = || -> Result<(), String> {
        let ok = a["address"].as_str().is_some_and(|s| !s.is_empty()) || a["tag"].as_str().is_some_and(|s| !s.is_empty());
        if ok { Ok(()) } else { Err("address or tag required".into()) }
    };
    let s = |k: &str| a[k].as_str().map(String::from);
    match req["op"].as_str().unwrap_or("") {
        "profile" => profile(node, a),
        "a_profile" => {
            named()?;
            a_profile_as(node, a, s("me").as_deref())
        }
        "a_follows" => {
            named()?;
            a_follows(node, a)
        }
        "a_posts" => {
            if a["following_of"].is_null() {
                named()?;
            }
            a_posts_as(node, a, s("following_of").as_deref())
        }
        "a_search" => a_search_as(node, &s("q").unwrap_or_default(), s("me").as_deref()),
        "repo" => repo(node, &s("name").ok_or("name required")?),
        "repos" => repos(node, s("owner").as_deref()),
        "items" => items(node, a),
        "balances" => balances(node, &s("address").ok_or("address required")?),
        // The sealed address book entries of an address, as stored (only its key opens them).
        "contact_rows" => {
            let me = s("address").ok_or("address required")?;
            let pre = arcade_store::KeyBuf::new().u16(c).bytes(me.as_bytes()).u8(0).done();
            let rows: Vec<Value> = scan(node, c, arcade_ledger::engine::A_CONTACT, &pre).into_iter()
                .map(|(k, v)| json!({"id": hex::encode(&k[pre.len()..]), "c": v["c"], "h": v["h"]})).collect();
            Ok(json!({"rows": rows}))
        }
        // Directory cards of key hashes (40 hex), as a.contacts shows them.
        "cards" => {
            let keys = a["keys"].as_array().cloned().unwrap_or_default();
            let cards: serde_json::Map<String, Value> = keys.iter().take(100).filter_map(|k| {
                let k = k.as_str()?;
                Some((k.to_string(), directory_card(node, &hex::decode(k).ok()?)?))
            }).collect();
            Ok(json!({"cards": cards}))
        }
        // A tag's row and its release record, and the tip: what tag_owner and reveal_window decide on.
        "tag_rel" => {
            let tag = s("tag").ok_or("tag required")?;
            let row = arcade_ledger::index::tag_row(&node.chain, c, &tag).map_err(|e| e.to_string())?;
            let rel = node.chain.store(c).read(|r| r.get(arcade_ledger::engine::TAG_REL, &arcade_store::KeyBuf::new().u16(c).bytes(tag.as_bytes()).done()))
                .ok().flatten().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
            Ok(json!({"address": row.as_ref().map(|r| r.address.clone()), "block_height": row.as_ref().map(|r| r.block_height),
                      "rel": rel, "tip": node.chain.tip(c).ok().flatten().map(|t| t.height)}))
        }
        // A repository's record and whether a tag maintains it: what pushes and forks check.
        "repo_row" => {
            let full = s("full").ok_or("full required")?;
            let maintains = s("tag").is_some_and(|t| is_maintainer(node, c, &full, &t));
            Ok(json!({"row": repo_row(node, &full), "maintainer": maintains}))
        }
        "holds_blob" => {
            let id: [u8; 32] = hex::decode(s("id").unwrap_or_default()).ok().and_then(|b| b.try_into().ok()).ok_or("id is 64 hex")?;
            Ok(json!({"held": crate::arcsvc::holds_blob(&id)}))
        }
        // An owner's wallet proofs, its newest set number, and whether it has a published A:// profile.
        "wallet_state" => {
            let owner = s("address").ok_or("address required")?;
            let last = node.chain.store(c).read(|r| r.get(arcade_ledger::engine::A_WALLET, &arcade_store::KeyBuf::new().u16(c).bytes(owner.as_bytes()).u8(1).done()))
                .ok().flatten().and_then(|v| serde_json::from_slice::<Value>(&v).ok()).and_then(|v| v["set"].as_u64()).unwrap_or(0);
            let has_profile = node.chain.store(c).read(|r| r.get(arcade_ledger::engine::A_PROFILE, &arcade_store::KeyBuf::new().u16(c).bytes(owner.as_bytes()).done()))
                .ok().flatten().is_some();
            let proven: Vec<Value> = crate::walletproofs::proven(node, c, &owner).into_iter().map(|(coin, address, h)| json!([coin, address, h])).collect();
            Ok(json!({"address": owner, "last_set": last, "has_profile": has_profile, "proven": proven, "known_from": crate::walletproofs::known_from(p.net)}))
        }
        o => Err(format!("unknown ARCADE ledger read {o}")),
    }
}

/// [`serve`]'s `wallet_state` for this wallet's `owner`, from the keeper while ARCADE is borrowed.
pub fn borrowed_wallet_state(node: &Node, owner: &str) -> Result<Option<Value>, String> {
    borrowed_read(node, "wallet_state", json!({"address": owner}))
}

/// The ARCADE chain's height as the copy standing in knows it: the borrowed copy's while it stands in, else this
/// arcade's own.
pub fn tip_height(node: &Node, p: &ChainParams) -> u64 {
    if crate::arcsvc::borrowed(p) {
        return crate::borrow::tip(node, p.key).unwrap_or(0) as u64;
    }
    node.chain.tip(p.chain_no).ok().flatten().map(|t| t.height as u64).unwrap_or(0)
}

/// The @tag `address` holds on ARCADE: this arcade's own ledger; while ARCADE is borrowed, the tag recorded for that
/// key here (`recorded`, set once its claim confirmed) or an arcade keeping the chain's word.
fn tag_here(node: &Node, p: &'static ChainParams, address: &str, recorded: Option<&str>) -> Option<String> {
    if let Ok(Some(t)) = arcade_ledger::index::tag_of(&node.chain, p.chain_no, address) {
        return Some(t);
    }
    if !crate::arcsvc::borrowed(p) {
        return None;
    }
    recorded.map(String::from).or_else(|| crate::claims::arc_tag_on_mesh(node, p, address))
}

thread_local! {
    /// The @tag this request signs as (`"as"` on a socket request; api::dispatch sets it): one of this wallet's own tags,
    /// or one held by a wallet imported for the ARCADE chain (wallet.import), such as a publisher's key.
    static SIGN_AS: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Run `f` signing ARCADE writes as `tag` (None: as before, the wallet's own tag key).
pub fn with_sign_as<T>(tag: Option<String>, f: impl FnOnce() -> T) -> T {
    let tag = tag.map(|t| t.trim().trim_start_matches('@').to_lowercase()).filter(|t| !t.is_empty());
    let before = SIGN_AS.with(|s| s.replace(tag));
    let out = f();
    SIGN_AS.with(|s| *s.borrow_mut() = before);
    out
}

/// The @tag this request signs as, if it names one.
pub fn sign_as() -> Option<String> {
    SIGN_AS.with(|s| s.borrow().clone())
}

/// The wallet's key on ARCADE that holds an @tag there (else its first tag key): (address, tag, secret key). A request
/// that signs as a tag (`with_sign_as`) gets the key holding exactly that tag, its own or an imported wallet's, or an error.
pub fn sender(node: &Node, p: &'static ChainParams) -> Result<(String, Option<String>, arcade_wallet::SecretKey), String> {
    if let Some(want) = sign_as() {
        for k in node.app.tag_keys(p.chain_no).map_err(|e| e.to_string())? {
            if tag_here(node, p, &k.address, k.tag.as_deref()).as_deref() == Some(want.as_str()) {
                let (k, sk) = node.app.tag_key(p, k.index).map_err(|e| e.to_string())?;
                return Ok((k.address, Some(want), sk));
            }
        }
        for (address, _, sk, kind) in crate::importsvc::wallets_on(node, p.key) {
            if kind == "legacy" && tag_here(node, p, &address, None).as_deref() == Some(want.as_str()) {
                return Ok((address, Some(want), sk));
            }
        }
        return Err(format!("no key here holds @{want} on the ARCADE chain (import it first: wallet.import with chain {})", p.key));
    }
    let keys = node.app.tag_keys(p.chain_no).map_err(|e| e.to_string())?;
    // Borrowed: a key whose tag is recorded here first (no asking), then the arcades keeping the chain.
    let borrowed = crate::arcsvc::borrowed(p);
    for pass in 0..if borrowed { 2 } else { 1 } {
        for k in &keys {
            let t = match pass {
                0 if borrowed => k.tag.clone().or_else(|| arcade_ledger::index::tag_of(&node.chain, p.chain_no, &k.address).ok().flatten()),
                0 => arcade_ledger::index::tag_of(&node.chain, p.chain_no, &k.address).ok().flatten(),
                _ => crate::claims::arc_tag_on_mesh(node, p, &k.address),
            };
            if let Some(t) = t {
                let (k, sk) = node.app.tag_key(p, k.index).map_err(|e| e.to_string())?;
                return Ok((k.address, Some(t), sk));
            }
        }
    }
    let (k, sk) = node.app.tag_key(p, 0).map_err(|e| e.to_string())?;
    Ok((k.address, None, sk))
}

/// One `arcd` record from the sender's key on ARCADE: `payload` is the whole record after the marker (version, type,
/// data; tags::claim_payload and the like make it whole). Its txid and fee.
pub fn send(node: &Node, payload: &[u8]) -> Result<(String, u64), String> {
    let p = write_chain()?;
    let (_, _, sk) = sender(node, p)?;
    let pk = arcade_wallet::secp256k1::PublicKey::from_secret_key(arcade_wallet::secp256k1::SECP256K1, &sk);
    let script = arcade_wallet::keys::p2pkh_script_for(&pk);
    // Confirmed coins first; then this wallet's change still waiting in the pool (a record right after its text's
    // blob spends the blob's change, so one coin is enough for both: @strawberry/arcade-v1 #7).
    let mut coins = crate::claims::arc_coins(node, p, &script);
    coins.extend(crate::arcsvc::waiting_outputs(&script).into_iter().map(|(op, value)| arcade_wallet::tx::Utxo { outpoint: op, value, script: script.clone() }));
    let fee_per_kb = 2 * arcade_arc::params::MIN_FEE_PER_KB;
    let tx = arcade_wallet::tx::build_arcade_with(p, payload, vec![], &sk, &coins, fee_per_kb, arcade_arc::params::DUST,
        arcade_app::now() as u32, arcade_wallet::tx::ARCADE_CLASS_C_MAX)
        .map_err(|e| format!("could not build the record's transaction: {e}"))?;
    let (txid, fee) = crate::ownwait::send_arc(node, &tx)?;
    Ok((txid.to_hex(), fee))
}

/// [`send`], also paying `value` to `to` (an ARCADE address) in the same transaction: an A:// tip (crate::asocial).
pub fn send_paying(node: &Node, payload: &[u8], to: &str, value: u64) -> Result<(String, u64), String> {
    let p = write_chain()?;
    let to_script = arcade_ledger::address::script_for_address(p, to).ok_or_else(|| format!("{to} isn't an ARCADE address"))?;
    let (_, _, sk) = sender(node, p)?;
    let pk = arcade_wallet::secp256k1::PublicKey::from_secret_key(arcade_wallet::secp256k1::SECP256K1, &sk);
    let script = arcade_wallet::keys::p2pkh_script_for(&pk);
    let mut coins = crate::claims::arc_coins(node, p, &script);
    coins.extend(crate::arcsvc::waiting_outputs(&script).into_iter().map(|(op, value)| arcade_wallet::tx::Utxo { outpoint: op, value, script: script.clone() }));
    let tx = arcade_wallet::tx::build_arcade_with(p, payload, vec![arcade_wallet::tx::TxOutSpec { value, script: to_script }], &sk, &coins,
        2 * arcade_arc::params::MIN_FEE_PER_KB, arcade_arc::params::DUST, arcade_app::now() as u32, arcade_wallet::tx::ARCADE_CLASS_C_MAX)
        .map_err(|e| format!("could not build the transaction: {e}"))?;
    let (txid, fee) = crate::ownwait::send_arc(node, &tx)?;
    Ok((txid.to_hex(), fee))
}

/// This wallet's sending address on ARCADE (its @tag's, else its first): where a relay asks to be paid (relaysvc).
pub fn own_address(node: &Node) -> Result<String, String> {
    Ok(sender(node, write_chain()?)?.0)
}

/// Pay `value` to `to` on ARCADE with `memo` in the same transaction (an AnyData record, so the payee can tell whose
/// payment it is): spends confirmed coins and this wallet's change still waiting. Its txid and fee.
pub fn pay_with_memo(node: &Node, to: &str, value: u64, memo: &[u8]) -> Result<(String, u64), String> {
    let p = write_chain()?;
    let to_script = arcade_ledger::address::script_for_address(p, to).ok_or_else(|| format!("{to} isn't an ARCADE address"))?;
    let (_, _, sk) = sender(node, p)?;
    let pk = arcade_wallet::secp256k1::PublicKey::from_secret_key(arcade_wallet::secp256k1::SECP256K1, &sk);
    let script = arcade_wallet::keys::p2pkh_script_for(&pk);
    let mut coins = crate::claims::arc_coins(node, p, &script);
    coins.extend(crate::arcsvc::waiting_outputs(&script).into_iter().map(|(op, value)| arcade_wallet::tx::Utxo { outpoint: op, value, script: script.clone() }));
    let tx = arcade_wallet::tx::build_arcade_with(p, &arcade_wallet::tx::any_data(memo), vec![arcade_wallet::tx::TxOutSpec { value, script: to_script }], &sk, &coins,
        2 * arcade_arc::params::MIN_FEE_PER_KB, arcade_arc::params::DUST, arcade_app::now() as u32, arcade_wallet::tx::ARCADE_CLASS_C_MAX)
        .map_err(|e| format!("could not build the payment: {e}"))?;
    let (txid, fee) = crate::ownwait::send_arc(node, &tx)?;
    Ok((txid.to_hex(), fee))
}

/// `ledger.announce` on ARCADE: the wallet's messaging key and profile name under its @tag there (a key announcement,
/// what `msg.announce` sends on the home chain), so every arcade reads the profile back from the ARCADE chain.
pub fn announce(node: &Node) -> Result<Value, String> {
    let p = write_chain()?;
    let (address, tag, _) = sender(node, p)?;
    let tag = tag.ok_or("claim an @tag on the ARCADE chain first (tag.claim with chain arctest)")?;
    let k = crate::msgsvc::keys(node)?;
    let h160 = arcade_ledger::address::p2pkh_hash_of(p, &address).ok_or("bad address")?;
    let a = arcade_msg::public::Announce { key: k.public_bytes(), hash160: Some(h160), name: tag.clone(), tag: Some(tag.clone()), ..Default::default() };
    let (txid, fee) = send(node, &arcade_wallet::tx::any_data(&a.encode()))?;
    let _ = node.app.event("messages", &format!("published your profile for @{tag} on {} (fee {fee})", p.name));
    Ok(json!({"txid": txid, "fee": fee, "tag": tag, "address": address, "chain": p.key}))
}

/// `ledger.profile {address | tag}` on ARCADE: the profile and tag the ARCADE chain's index holds.
pub fn profile(node: &Node, req: &Value) -> Result<Value, String> {
    if let Some(v) = borrowed_read(node, "profile", req.clone())? {
        return Ok(v);
    }
    let p = chain()?;
    let c = p.chain_no;
    let address = match (req["address"].as_str(), req["tag"].as_str()) {
        (Some(a), _) => a.to_string(),
        (None, Some(t)) => arcade_ledger::index::tag_row(&node.chain, c, &t.trim_start_matches('@').to_lowercase()).map_err(|e| e.to_string())?
            .map(|r| r.address).ok_or("no such @tag on the ARCADE chain")?,
        _ => return Err("address or tag required".into()),
    };
    let tag = arcade_ledger::index::tag_of(&node.chain, c, &address).map_err(|e| e.to_string())?;
    let prof = arcade_ledger::index::profile(&node.chain, c, &address).map_err(|e| e.to_string())?;
    Ok(json!({"chain": p.key, "address": address, "tag": tag, "profile": prof,
              "indexed_to": node.chain.indexed_to(c, "ledger.v1").ok().flatten(), "tags": arcade_ledger::index::read_tags(&node.chain, c).map(|t| t.len()).unwrap_or(0)}))
}

fn salt_key(tag: &str) -> Vec<u8> {
    format!("arc_tag_salt:{tag}").into_bytes()
}

/// The salt this wallet committed for `tag` (crate::claims sends its claim with it).
pub fn committed_salt(node: &Node, tag: &str) -> Option<Vec<u8>> {
    node.app.store.read(|r| r.get(arcade_app::META, &salt_key(tag))).ok().flatten()
}

/// `ledger.tag_commit {tag}`: commit ahead to claiming a released @tag (§15 step 8): a commit record naming
/// `commitment(tag, address, salt)` with a fresh salt kept here; the claim (tag.claim) then carries the salt.
pub fn tag_commit(node: &Node, tag: &str) -> Result<Value, String> {
    let p = write_chain()?;
    let tag = tag.trim().trim_start_matches('@').to_lowercase();
    if let Some(why) = arcade_ledger::tags::tag_problem(&tag) {
        return Err(format!("@{tag}: {why}"));
    }
    let (address, _, _) = sender(node, p)?;
    let mut salt = [0u8; 16];
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut salt);
    let c = arcade_ledger::tags::commitment(&tag, &address, &salt);
    node.app.store.write(|w| w.put(arcade_app::META, &salt_key(&tag), &salt)).map_err(|e| e.to_string())?;
    let (txid, fee) = send(node, &arcade_ledger::tags::commit_payload(&c))?;
    Ok(json!({"txid": txid, "fee": fee, "tag": tag, "address": address, "commitment": hex::encode(c)}))
}

/// `ledger.tag_checkin`: keep this wallet's @tag on ARCADE with a check-in (0.1 ARCADE into the pot).
pub fn tag_checkin(node: &Node) -> Result<Value, String> {
    let p = write_chain()?;
    let (_, tag, sk) = sender(node, p)?;
    let tag = tag.ok_or("this wallet has no @tag on the ARCADE chain")?;
    let x = arcade_ledger::tags::expiry(p.chain_no).ok_or("tags don't expire on this chain")?;
    let pk = arcade_wallet::secp256k1::PublicKey::from_secret_key(arcade_wallet::secp256k1::SECP256K1, &sk);
    let script = arcade_wallet::keys::p2pkh_script_for(&pk);
    let coins = crate::claims::arc_coins(node, p, &script);
    // The fee is the check-in: the rate raised until the transaction pays it.
    let payload = arcade_ledger::tags::checkin_payload(&tag);
    let fee_of = |tx: &arcade_chain::Tx| -> u64 {
        let ins: u64 = tx.inputs.iter().filter_map(|i| coins.iter().find(|c| c.outpoint == i.prevout).map(|c| c.value)).sum();
        ins.saturating_sub(tx.outputs.iter().map(|o| o.value).sum())
    };
    let mut rate = 2 * arcade_arc::params::MIN_FEE_PER_KB;
    let mut built = None;
    for _ in 0..8 {
        let tx = arcade_wallet::tx::build_arcade_with(p, &payload, vec![], &sk, &coins, rate, arcade_arc::params::DUST, arcade_app::now() as u32, arcade_wallet::tx::ARCADE_CLASS_C_MAX)
            .map_err(|e| format!("could not build the check-in: {e}"))?;
        let fee = fee_of(&tx);
        if fee >= x.checkin_fee {
            built = Some(tx);
            break;
        }
        rate = rate.saturating_mul(x.checkin_fee).div_ceil(fee.max(1)) + 1;
    }
    let tx = built.ok_or("could not make a check-in that pays its fee")?;
    let (txid, fee) = crate::ownwait::send_arc(node, &tx)?;
    Ok(json!({"txid": txid.to_hex(), "fee": fee, "tag": tag}))
}

/// An @tag's owner on the ARCADE chain, and a warning when a released tag changed hands lately (§15 step 8: sends
/// warn of the owner change).
pub fn tag_owner(node: &Node, tag: &str) -> Option<(String, Option<String>)> {
    let (c, address, height, rel, _) = tag_state(node, tag)?;
    let address = address?;
    let warn = arcade_ledger::tags::expiry(c).and_then(|x| {
        let rel = rel?;
        let (rel_h, rel_t) = (rel["h"].as_u64()? as u32, rel["t"].as_u64()?);
        let changed = height >= rel_h && rel["a"].as_str() != Some(address.as_str());
        (changed && arcade_app::now() < rel_t + x.warn).then(|| format!(
            "@{tag} changed owner: it was released at block {rel_h} (its owner was inactive) and claimed again at block {height} by someone else"))
    });
    Some((address, warn))
}

/// What [`tag_owner`] and [`reveal_window`] decide on: (chain, owner, claim height, release record, tip), from this
/// arcade's own tables or, while ARCADE is borrowed, from a keeper.
fn tag_state(node: &Node, tag: &str) -> Option<(u16, Option<String>, u32, Option<Value>, u32)> {
    let p = write_chain().ok()?;
    let c = p.chain_no;
    if let Some(v) = borrowed_read(node, "tag_rel", json!({"tag": tag})).ok()? {
        let rel = Some(v["rel"].clone()).filter(|r| r.is_object());
        return Some((c, v["address"].as_str().map(String::from), v["block_height"].as_u64().unwrap_or(0) as u32, rel, v["tip"].as_u64().unwrap_or(0) as u32));
    }
    let row = arcade_ledger::index::tag_row(&node.chain, c, tag).ok().flatten();
    let rel = node.chain.store(c).read(|r| r.get(arcade_ledger::engine::TAG_REL, &arcade_store::KeyBuf::new().u16(c).bytes(tag.as_bytes()).done()))
        .ok().flatten().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let tip = node.chain.tip(c).ok().flatten().map(|t| t.height).unwrap_or(0);
    Some((c, row.as_ref().map(|r| r.address.clone()), row.map_or(0, |r| r.block_height), rel, tip))
}

/// A released @tag's reveal window on ARCADE, while it is open: (released at, window ends at).
pub fn reveal_window(node: &Node, tag: &str) -> Option<(u32, u32)> {
    let (c, _, _, rel, tip) = tag_state(node, tag)?;
    let x = arcade_ledger::tags::expiry(c)?;
    let h = rel?["h"].as_u64()? as u32;
    (tip + 1 < h + x.reveal_blocks).then_some((h, h + x.reveal_blocks))
}

// ---------------- @tag repositories (§15 step 9) ----------------

fn own_full(node: &Node, name: &str) -> Result<(String, Option<String>), String> {
    let p = write_chain()?;
    let (_, tag, _) = sender(node, p)?;
    let full = arcade_ledger::repos::full_name(name, tag.as_deref()).map_err(|e| format!("{name}: {e}"))?;
    Ok((full, tag))
}

/// `ledger.repo_push {name, manifest, base?, force?}`: point `@owner/name` (or `name` under this wallet's tag) at a git
/// manifest blob. From rules version 4 (ARCADE #10) the record names `base`, the manifest the push was built on ("" for
/// a new repository; left out, the repository's manifest now), and the ARCADE ledger refuses it when the repository
/// has moved on since, unless `force`. Below version 4 `base` and `force` aren't sent and the push replaces as before.
pub fn repo_push(node: &Node, name: &str, manifest: &str, base: Option<&str>, force: bool) -> Result<Value, String> {
    let (full, tag) = own_full(node, name)?;
    let tag = tag.ok_or("claim an @tag on the ARCADE chain first")?;
    let m: [u8; 32] = hex::decode(manifest).ok().and_then(|b| b.try_into().ok()).ok_or("manifest is a blob id (64 hex)")?;
    let held = match borrowed_read(node, "holds_blob", json!({"id": manifest}))? {
        Some(v) => v["held"] == true,
        None => crate::arcsvc::holds_blob(&m),
    };
    if !held {
        return Err("that manifest isn't a blob on the ARCADE chain (yet)".into());
    }
    let (owner, _) = full.split_once('/').unwrap_or_default();
    let (row, maintains) = repo_state(node, &full, &tag)?;
    if owner != tag && !maintains {
        return Err(format!("@{tag} isn't the owner or a maintainer of @{full}"));
    }
    let current = row.and_then(|r| hex::decode(r["m"].as_str()?).ok()?.try_into().ok()).unwrap_or(arcade_ledger::repos::NO_BASE);
    if !push_base_checked(node) {
        let (txid, fee) = send(node, &arcade_ledger::repos::push_payload(&full, &m))?;
        return Ok(json!({"txid": txid, "fee": fee, "repo": format!("@{full}"), "manifest": manifest}));
    }
    let b = match base.map(str::trim) {
        None => current,
        Some("") => arcade_ledger::repos::NO_BASE,
        Some(x) => hex::decode(x).ok().and_then(|b| b.try_into().ok()).ok_or("base is a manifest blob id (64 hex), or \"\" for a new repository")?,
    };
    if !force && b != current {
        return Err(if current == arcade_ledger::repos::NO_BASE {
            format!("@{full} doesn't exist yet: its first push names no base (or push with force)")
        } else {
            format!("@{full} is at manifest {}, not the base this push was built on: fetch and push again, or force it", hex::encode(current))
        });
    }
    let (txid, fee) = send(node, &arcade_ledger::repos::push_onto_payload(&full, &m, &b, force))?;
    Ok(json!({"txid": txid, "fee": fee, "repo": format!("@{full}"), "manifest": manifest, "base": hex::encode(b), "force": force}))
}

/// Whether a push sent now lands where rules version 4 checks its base (the next block's height on).
pub fn push_base_checked(node: &Node) -> bool {
    let (Some((net, ..)), Ok(p)) = (crate::arcsvc::defined_net(), write_chain()) else { return false };
    arcade_arc::params::rules_v4(&net, tip_height(node, p) + 1)
}

/// A repository's record and whether `tag` maintains it: this arcade's own tables, or a keeper's while ARCADE is
/// borrowed here.
fn repo_state(node: &Node, full: &str, tag: &str) -> Result<(Option<Value>, bool), String> {
    if let Some(v) = borrowed_read(node, "repo_row", json!({"full": full, "tag": tag}))? {
        return Ok((Some(v["row"].clone()).filter(|r| r.is_object()), v["maintainer"] == true));
    }
    let c = chain()?.chain_no;
    Ok((repo_row(node, full), !tag.is_empty() && is_maintainer(node, c, full, tag)))
}

/// `ledger.repo_maintainer {name, tag, add}` (the owner only).
pub fn repo_maintainer(node: &Node, name: &str, tag: &str, add: bool) -> Result<Value, String> {
    let (full, _) = own_full(node, name)?;
    let t = tag.trim_start_matches('@').to_lowercase();
    let (txid, fee) = send(node, &arcade_ledger::repos::maintainer_payload(&full, add, &t))?;
    Ok(json!({"txid": txid, "fee": fee, "repo": format!("@{full}"), "maintainer": t, "add": add}))
}

/// `ledger.repo_fork {from, name}`: `@from` copied under this wallet's tag.
pub fn repo_fork(node: &Node, from: &str, name: &str) -> Result<Value, String> {
    let src = arcade_ledger::repos::full_name(from, None).map_err(|e| format!("{from}: {e}"))?;
    let (full, _) = own_full(node, name)?;
    if repo_state(node, &src, "")?.0.is_none() {
        return Err(format!("no repository @{src} on the ARCADE chain"));
    }
    let (txid, fee) = send(node, &arcade_ledger::repos::fork_payload(&full, &src))?;
    Ok(json!({"txid": txid, "fee": fee, "repo": format!("@{full}"), "from": format!("@{src}")}))
}

fn is_maintainer(node: &Node, c: u16, full: &str, tag: &str) -> bool {
    node.chain.store(c).read(|r| r.get(arcade_ledger::engine::REPO_MAINT, &arcade_store::KeyBuf::new().u16(c).bytes(full.as_bytes()).u8(0).bytes(tag.as_bytes()).done()))
        .ok().flatten().is_some()
}

/// A repository's record on the ARCADE chain (`owner/name`).
pub fn repo_row(node: &Node, full: &str) -> Option<Value> {
    let c = chain().ok()?.chain_no;
    let v = node.chain.store(c).read(|r| r.get(arcade_ledger::engine::REPO, &arcade_store::KeyBuf::new().u16(c).bytes(full.as_bytes()).done())).ok().flatten()?;
    serde_json::from_slice(&v).ok()
}

/// `ledger.repo {name}`: the record, its maintainers and its ref history; `ledger.repos {owner?}`: every repository.
pub fn repo(node: &Node, name: &str) -> Result<Value, String> {
    if let Some(v) = borrowed_read(node, "repo", json!({"name": name}))? {
        return Ok(v);
    }
    let full = arcade_ledger::repos::full_name(name, None).map_err(|e| format!("{name}: {e}"))?;
    let c = chain()?.chain_no;
    let row = repo_row(node, &full).ok_or_else(|| format!("no repository @{full} on the ARCADE chain"))?;
    let pre = arcade_store::KeyBuf::new().u16(c).bytes(full.as_bytes()).u8(0).done();
    let maint: Vec<String> = node.chain.store(c).read(|r| r.scan_prefix(arcade_ledger::engine::REPO_MAINT, &pre)).unwrap_or_default()
        .into_iter().map(|(k, _)| String::from_utf8_lossy(&k[pre.len()..]).into_owned()).collect();
    let log: Vec<Value> = node.chain.store(c).read(|r| r.scan_prefix(arcade_ledger::engine::REPO_LOG, &pre)).unwrap_or_default()
        .into_iter().filter_map(|(k, v)| {
            let mut e: Value = serde_json::from_slice(&v).ok()?;
            e["height"] = json!(u32::from_be_bytes(k.get(pre.len()..pre.len() + 4)?.try_into().ok()?));
            Some(e)
        }).collect();
    Ok(json!({"repo": format!("@{full}"), "manifest": row["m"], "fork": row["fork"], "pushes": row["n"], "by": row["by"], "height": row["h"], "maintainers": maint, "log": log}))
}

pub fn repos(node: &Node, owner: Option<&str>) -> Result<Value, String> {
    if let Some(v) = borrowed_read(node, "repos", json!({"owner": owner}))? {
        return Ok(v);
    }
    let c = chain()?.chain_no;
    let mut pre = arcade_store::KeyBuf::new().u16(c);
    if let Some(o) = owner {
        pre = pre.bytes(format!("{}/", o.trim_start_matches('@').to_lowercase()).as_bytes());
    }
    let pre = pre.done();
    let rows = node.chain.store(c).read(|r| r.scan_prefix(arcade_ledger::engine::REPO, &pre)).map_err(|e| e.to_string())?;
    let list: Vec<Value> = rows.into_iter().filter_map(|(k, v)| {
        let v: Value = serde_json::from_slice(&v).ok()?;
        Some(json!({"repo": format!("@{}", String::from_utf8_lossy(&k[2..])), "manifest": v["m"], "fork": v["fork"], "pushes": v["n"]}))
    }).collect();
    Ok(json!({"repos": list}))
}

/// The manifest blob a repository points at now (crate::arcgit: /git/@owner/name).
pub fn repo_manifest(node: &Node, full: &str) -> Option<String> {
    repo_row(node, full)?["m"].as_str().map(String::from)
}

// ---------------- issues, pull requests, comments, reviews (§15 step 10) ----------------

/// A text as a blob (Permanent, JSON): its id. The record that names it can go at once: the ledger keeps the id.
/// About what a record's transaction and the blob's own transaction cost on top of the blob bytes (fees at twice the
/// floor for a few hundred bytes each, and the dust left as change), kept back before a text is posted.
const RECORD_FEE_ROOM: u64 = 1_000_000;

/// The blob part of posting `bytes` of text (a Permanent blob): what the next block takes for it (blob::fee_at: the flat
/// rate before rules version 5, the Permanent rate since).
fn blob_cost(node: &Node, p: &ChainParams, bytes: usize) -> u64 {
    let flat = arcade_arc::blob::fee_for(bytes as u64);
    let Some(net) = crate::arcsvc::svc_net() else { return flat };
    let tip = node.chain.tip(p.chain_no).ok().flatten().map(|t| t.height).unwrap_or(0);
    arcade_arc::blob::fee_at(&net, tip as u64 + 1, &[(arcade_arc::blob::Class::Permanent, bytes as u64)])
}

fn body_blob(node: &Node, v: &Value) -> Result<[u8; 32], String> {
    let bytes = serde_json::to_vec(v).map_err(|e| e.to_string())?;
    // The text's blob and then the record are two transactions. The record spends the blob's change while both wait
    // in the pool (`send` takes waiting change), so one coin pays for both. Before, it needed two confirmed coins, and
    // a wallet funded once has one: it could never post (@strawberry/arcade-v1 #7).
    let p = chain()?;
    let (_, _, sk) = sender(node, p)?;
    let pk = arcade_wallet::secp256k1::PublicKey::from_secret_key(arcade_wallet::secp256k1::SECP256K1, &sk);
    let script = arcade_wallet::keys::p2pkh_script_for(&pk);
    let have: u64 = crate::claims::arc_coins(node, p, &script).iter().map(|u| u.value).sum::<u64>()
        + crate::arcsvc::waiting_outputs(&script).iter().map(|(_, v)| v).sum::<u64>();
    // What the text costs (blob bytes at the blob rate, rounded up per kB) and about what its record's fee is: refused
    // before anything is paid, so a retry never pays for the blob twice.
    let need = blob_cost(node, p, bytes.len()) + RECORD_FEE_ROOM;
    if have < need {
        return Err(format!(
            "not enough ARCADE here: posting this ({} bytes of text) costs about {} ARCADE, and this wallet has {}",
            bytes.len(), arcade_ledger::amount::trimmed8(need), arcade_ledger::amount::trimmed8(have)
        ));
    }
    let r = crate::coinsend::publish_blob(node, &bytes, "application/json", arcade_arc::blob::Class::Permanent, arcade_arc::blob::Encoding::Plain, true)?;
    r["blob"].as_str().and_then(|h| hex::decode(h).ok()).and_then(|b| b.try_into().ok()).ok_or_else(|| "the body's blob has no id".to_string())
}

fn repo_full(name: &str) -> Result<String, String> {
    arcade_ledger::repos::full_name(name, None).map_err(|e| format!("{name}: {e}"))
}

/// `ledger.item_open {repo, kind: issue|pull, title, body, from?, manifest?, head?, extra?}`.
pub fn item_open(node: &Node, req: &Value) -> Result<Value, String> {
    let full = repo_full(req["repo"].as_str().ok_or("repo required")?)?;
    if repo_row(node, &full).is_none() {
        return Err(format!("no repository @{full} on the ARCADE chain"));
    }
    let kind = match req["kind"].as_str().unwrap_or("issue") {
        "issue" => arcade_ledger::repos::KIND_ISSUE,
        "pull" | "pr" | "pull_request" => arcade_ledger::repos::KIND_PULL,
        k => return Err(format!("kind is issue or pull, not {k}")),
    };
    let mut body = json!({"title": req["title"].as_str().ok_or("title required")?, "body": req["body"].as_str().unwrap_or("")});
    for k in ["from", "manifest", "head", "extra"] {
        if !req[k].is_null() {
            body[k] = req[k].clone();
        }
    }
    let b = body_blob(node, &body)?;
    let (txid, fee) = send(node, &arcade_ledger::repos::open_payload(&full, kind, &b))?;
    Ok(json!({"txid": txid, "fee": fee, "repo": format!("@{full}"), "body": hex::encode(b)}))
}

/// `ledger.item_comment {repo, number, body, review?: approve|changes}`.
pub fn item_comment(node: &Node, req: &Value) -> Result<Value, String> {
    let full = repo_full(req["repo"].as_str().ok_or("repo required")?)?;
    let n = req["number"].as_u64().ok_or("number required")? as u32;
    let verdict = match req["review"].as_str() {
        None | Some("") | Some("comment") => 0,
        Some("approve") => 1,
        Some("changes") => 2,
        Some(v) => return Err(format!("review is approve or changes, not {v}")),
    };
    let mut body = json!({"body": req["body"].as_str().unwrap_or("")});
    if !req["extra"].is_null() {
        body["extra"] = req["extra"].clone();
    }
    let b = body_blob(node, &body)?;
    let (txid, fee) = send(node, &arcade_ledger::repos::comment_payload(&full, n, verdict, &b))?;
    Ok(json!({"txid": txid, "fee": fee}))
}

/// `ledger.item_state {repo, number, state: open|closed|merged, manifest?}` (a merge moves the repository to `manifest`).
pub fn item_state(node: &Node, req: &Value) -> Result<Value, String> {
    use arcade_ledger::repos::{STATE_CLOSED, STATE_MERGED, STATE_OPEN};
    let full = repo_full(req["repo"].as_str().ok_or("repo required")?)?;
    let n = req["number"].as_u64().ok_or("number required")? as u32;
    let state = match req["state"].as_str().ok_or("state required")? {
        "open" => STATE_OPEN,
        "closed" | "close" => STATE_CLOSED,
        "merged" | "merge" => STATE_MERGED,
        s => return Err(format!("state is open, closed or merged, not {s}")),
    };
    let m: [u8; 32] = match req["manifest"].as_str() {
        Some(h) => hex::decode(h).ok().and_then(|b| b.try_into().ok()).ok_or("manifest is 64 hex")?,
        None if state == STATE_MERGED => return Err("a merge names the manifest the repository moves to".into()),
        None => [0; 32],
    };
    let (txid, fee) = send(node, &arcade_ledger::repos::state_payload(&full, n, state, &m))?;
    Ok(json!({"txid": txid, "fee": fee}))
}

fn body_of(hash: &str) -> Value {
    hex::decode(hash).ok().and_then(|b| <[u8; 32]>::try_from(b).ok()).and_then(|id| crate::arcblob::view(&id))
        .and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null)
}

/// `ledger.items {repo, number?}`: a repository's issues and pull requests with their texts, comments and reviews.
pub fn items(node: &Node, req: &Value) -> Result<Value, String> {
    if let Some(v) = borrowed_read(node, "items", req.clone())? {
        return Ok(v);
    }
    let full = repo_full(req["repo"].as_str().ok_or("repo required")?)?;
    let c = chain()?.chain_no;
    let pre = arcade_store::KeyBuf::new().u16(c).bytes(full.as_bytes()).u8(0).done();
    let only = req["number"].as_u64().map(|n| n as u32);
    let st = node.chain.store(c);
    let rows = st.read(|r| r.scan_prefix(arcade_ledger::engine::REPO_ITEM, &pre)).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for (k, v) in rows {
        let Some(n) = k.get(pre.len()..pre.len() + 4).and_then(|b| b.try_into().ok()).map(u32::from_be_bytes) else { continue };
        if only.is_some_and(|o| o != n) {
            continue;
        }
        let v: Value = serde_json::from_slice(&v).unwrap_or(Value::Null);
        let epre = arcade_store::KeyBuf::new().u16(c).bytes(full.as_bytes()).u8(0).u32(n).done();
        let events: Vec<Value> = st.read(|r| r.scan_prefix(arcade_ledger::engine::REPO_EVENT, &epre)).unwrap_or_default().into_iter().filter_map(|(ek, ev)| {
            let mut e: Value = serde_json::from_slice(&ev).ok()?;
            e["height"] = json!(u32::from_be_bytes(ek.get(epre.len()..epre.len() + 4)?.try_into().ok()?));
            if matches!(e["w"].as_str(), Some("comment" | "review" | "open")) {
                e["text"] = body_of(e["b"].as_str().unwrap_or(""));
            }
            Some(e)
        }).collect();
        let state = ["", "open", "closed", "merged"].get(v["s"].as_u64().unwrap_or(0) as usize).copied().unwrap_or("");
        out.push(json!({"number": n, "kind": if v["k"] == 2 { "pull" } else { "issue" }, "by": v["by"], "state": state,
                        "height": v["h"], "txid": v["txid"], "text": body_of(v["b"].as_str().unwrap_or("")), "events": events}));
    }
    Ok(json!({"repo": format!("@{full}"), "items": out}))
}

/// The @tag (else the address) that signed a waiting transaction: its first input's key (a P2PKH spend), as the
/// ledger names an item's author once a block has it.
fn pending_by(node: &Node, p: &'static ChainParams, tx: &arcade_chain::Tx) -> Value {
    let sig = tx.inputs.first().map(|i| i.script_sig.as_slice()).unwrap_or(&[]);
    // <push sig> <push pubkey>: the pubkey is the last push, 33 (or 65) bytes.
    let pk = sig.first().map(|n| *n as usize).and_then(|n| sig.get(1 + n..)).and_then(|rest| {
        let m = *rest.first()? as usize;
        rest.get(1..1 + m).filter(|_| m == 33 || m == 65)
    });
    let Some(pk) = pk.and_then(|k| arcade_wallet::secp256k1::PublicKey::from_slice(k).ok()) else { return Value::Null };
    let Some(address) = arcade_ledger::address::address_of(&arcade_wallet::keys::p2pkh_script_for(&pk), p) else { return Value::Null };
    match arcade_ledger::index::tag_of(&node.chain, p.chain_no, &address).ok().flatten() {
        Some(t) => json!(t),
        None => json!(address),
    }
}

/// Add to a `ledger.items` answer what still waits in the pool for its repository (ARCADE #25 part 2): new items
/// (no number yet), comments, reviews and state changes, each marked pending: true.
pub fn add_pending(node: &Node, v: &mut Value) {
    use arcade_ledger::repos::RepoOp;
    let Some(full) = v["repo"].as_str().map(|r| r.trim_start_matches('@').to_string()) else { return };
    let Some(p) = crate::arcsvc::defined() else { return };
    let known: std::collections::HashSet<String> = v["items"].as_array().into_iter().flatten()
        .flat_map(|it| std::iter::once(it["txid"].clone()).chain(it["events"].as_array().into_iter().flatten().map(|e| e["txid"].clone())))
        .filter_map(|t| t.as_str().map(String::from)).collect();
    for tx in crate::arcsvc::pool_txs() {
        let txid = tx.txid.to_hex();
        if known.contains(&txid) {
            continue;
        }
        let payload = arcade_ledger::extract::class_c_payload(&tx);
        let Some(Ok(op)) = arcade_ledger::repos::parse(&payload) else { continue };
        let by = pending_by(node, p, &tx);
        match op {
            RepoOp::Open { name, kind, body } if name == full => {
                let b = hex::encode(body);
                let text = body_of(&b);
                if let Some(items) = v["items"].as_array_mut() {
                    items.push(json!({"number": null, "kind": if kind == arcade_ledger::repos::KIND_PULL { "pull" } else { "issue" }, "by": by,
                                      "state": "open", "height": null, "txid": txid, "text": text,
                                      "events": [{"by": by, "w": "open", "v": 0, "b": b, "txid": txid, "height": null, "pending": true}], "pending": true}));
                }
            }
            RepoOp::Comment { name, number, verdict, body } if name == full => {
                let b = hex::encode(body);
                let e = json!({"by": by, "w": if verdict == 0 { "comment" } else { "review" }, "v": verdict, "b": b, "txid": txid, "height": null,
                               "text": body_of(&b), "pending": true});
                if let Some(it) = v["items"].as_array_mut().and_then(|a| a.iter_mut().find(|it| it["number"] == number)) {
                    if let Some(ev) = it["events"].as_array_mut() {
                        ev.push(e);
                    }
                }
            }
            RepoOp::State { name, number, state, manifest } if name == full => {
                let w = ["", "reopen", "close", "merge"].get(state as usize).copied().unwrap_or("");
                let e = json!({"by": by, "w": w, "v": 0,
                               "b": hex::encode(manifest), "txid": txid, "height": null, "pending": true});
                if let Some(it) = v["items"].as_array_mut().and_then(|a| a.iter_mut().find(|it| it["number"] == number)) {
                    if let Some(ev) = it["events"].as_array_mut() {
                        ev.push(e);
                    }
                }
            }
            _ => {}
        }
    }
}

/// `ledger.balances {address}`: an address's arcade token balances on ARCADE, each [free, reserved, offered, accepted].
pub fn balances(node: &Node, address: &str) -> Result<Value, String> {
    if let Some(v) = borrowed_read(node, "balances", json!({"address": address}))? {
        return Ok(v);
    }
    let c = chain()?.chain_no;
    let pre = arcade_store::KeyBuf::new().u16(c).bytes(address.as_bytes()).u8(0).done();
    let rows = node.chain.store(c).read(|r| r.scan_prefix(arcade_ledger::engine::BALANCE, &pre)).map_err(|e| e.to_string())?;
    let list: Vec<Value> = rows.into_iter().filter_map(|(k, v)| {
        let pid = u32::from_be_bytes(k.get(k.len() - 4..)?.try_into().ok()?);
        let a: Vec<u64> = (0..4).map(|i| v.get(i * 8..i * 8 + 8).map_or(0, |b| u64::from_be_bytes(b.try_into().unwrap()))).collect();
        Some(json!({"id": pid, "amounts": a}))
    }).collect();
    Ok(json!({"address": address, "balances": list}))
}

// ---------------- A:// (profiles, follows, the address book, posts) ----------------

fn scan(node: &Node, c: u16, table: &str, pre: &[u8]) -> Vec<(Vec<u8>, Value)> {
    node.chain.store(c).read(|r| r.scan_prefix(table, pre)).unwrap_or_default().into_iter()
        .filter_map(|(k, v)| Some((k, serde_json::from_slice::<Value>(&v).ok()?))).collect()
}

/// The address an A:// call is about: `address`, else `tag`'s owner, else this wallet's.
pub fn who(node: &Node, req: &Value) -> Result<String, String> {
    let p = chain()?;
    if let Some(a) = req["address"].as_str().filter(|a| !a.is_empty()) {
        return Ok(a.to_string());
    }
    if let Some(t) = req["tag"].as_str().filter(|t| !t.is_empty()) {
        let t = t.trim_start_matches('@').to_lowercase();
        return arcade_ledger::index::tag_row(&node.chain, p.chain_no, &t).map_err(|e| e.to_string())?.map(|r| r.address).ok_or_else(|| format!("@{t} isn't claimed on the ARCADE chain"));
    }
    Ok(sender(node, p)?.0)
}

fn card(node: &Node, c: u16, address: &str) -> Value {
    let tag = arcade_ledger::index::tag_of(&node.chain, c, address).ok().flatten();
    let prof = node.chain.store(c).read(|r| r.get(arcade_ledger::engine::A_PROFILE, &arcade_store::KeyBuf::new().u16(c).bytes(address.as_bytes()).done())).ok().flatten()
        .and_then(|v| serde_json::from_slice::<Value>(&v).ok());
    let count = |t: &str| scan(node, c, arcade_ledger::engine::A_FOLLOWER, &arcade_store::KeyBuf::new().u16(c).bytes(t.as_bytes()).u8(0).done()).len();
    let followers = tag.as_ref().map_or(0, |t| count(t))
        + arcade_ledger::address::decode_base58check(address).filter(|(_, h)| h.len() == 20).map_or(0, |(_, h)| count(&format!("#{}", hex::encode(h))));
    let following = scan(node, c, arcade_ledger::engine::A_FOLLOW, &arcade_store::KeyBuf::new().u16(c).bytes(address.as_bytes()).u8(0).done()).len();
    json!({"address": address, "tag": tag, "profile": prof.as_ref().map(|p| p["v"].clone()), "height": prof.as_ref().map(|p| p["h"].clone()),
           "versions": prof.as_ref().map_or(json!(0), |p| p["n"].clone()), "followers": followers, "following": following,
           // Every address it has proven it holds (A:// op 7, crate::walletproofs), on every coin.
           "wallets": crate::walletproofs::cards(node, c, address)})
}

/// `a.profile {tag | address}`: a profile card as the ARCADE chain's ledger holds it (this wallet's when neither).
pub fn a_profile(node: &Node, req: &Value) -> Result<Value, String> {
    let me = sender(node, write_chain()?)?.0;
    let mut v = match borrowed_read(node, "a_profile", {
        let mut a = about_me(node, req)?;
        a["me"] = json!(me);
        a
    })? {
        Some(v) => v,
        None => a_profile_as(node, req, Some(&me))?,
    };
    if v["mine"] == true {
        let p = write_chain()?;
        v["wallet_addresses"] = json!(wallet_addresses(node));
        // What publishing costs and whether the wallet can pay it (test ARCADE, e.g. from the faucet).
        let (_, _, sk) = sender(node, p)?;
        let pk = arcade_wallet::secp256k1::PublicKey::from_secret_key(arcade_wallet::secp256k1::SECP256K1, &sk);
        let have: u64 = crate::claims::arc_coins(node, p, &arcade_wallet::keys::p2pkh_script_for(&pk)).iter().map(|u| u.value).sum();
        v["spendable"] = json!(have);
        v["can_publish"] = json!(have >= arcade_arc::params::DUST + 100_000);
    }
    Ok(v)
}

/// [`a_profile`] on this arcade's own tables, `me` being the address whose card counts as its own.
fn a_profile_as(node: &Node, req: &Value, me: Option<&str>) -> Result<Value, String> {
    let p = chain()?;
    let c = p.chain_no;
    let a = who(node, req)?;
    let mut v = card(node, c, &a);
    let mine = me == Some(a.as_str());
    v["mine"] = json!(mine);
    // No A:// profile yet: the one published on the home chain under the same key (Martin 2026-11-15: keep the name
    // people published; they republish on ARCADE once they have test ARCADE).
    if v["profile"].is_null() {
        if let Some((_, h160)) = arcade_ledger::address::decode_base58check(&a).filter(|(_, h)| h.len() == 20) {
            let home = arcade_ledger::address::base58check(node.params.p2pkh_version, &h160);
            if let Ok(Some(old)) = arcade_ledger::index::profile(&node.chain, node.params.chain_no, &home) {
                let mut pv = serde_json::Map::new();
                for (from, to) in [("name", "n"), ("bio", "b"), ("url", "u")] {
                    if let Some(x) = old[from].as_str().filter(|x| !x.is_empty()) {
                        pv.insert(to.into(), json!(x));
                    }
                }
                v["profile"] = Value::Object(pv);
                v["from_home"] = json!({"chain": node.params.name, "address": home, "height": old["height"], "picture": old["pfp"]});
            }
        }
    }
    Ok(v)
}

/// `a.profile_set {name, bio, url, picture, addresses}`: publish this wallet's profile (one A:// record).
pub fn a_profile_set(node: &Node, req: &Value) -> Result<Value, String> {
    let mut v = serde_json::Map::new();
    for (from, to) in [("name", "n"), ("bio", "b"), ("url", "u"), ("picture", "p")] {
        if let Some(s) = req[from].as_str().map(str::trim).filter(|s| !s.is_empty()) {
            v.insert(to.into(), json!(s));
        }
    }
    // No stated addresses (Martin 2026-11-21: "You should not be able to change your addresses shown, they should just
    // be automatic"): a card's addresses are only its wallet proofs (crate::walletproofs), each signed by its own key.
    let v = Value::Object(v);
    if let Some(p) = arcade_ledger::aproto::profile_problem(&v) {
        return Err(p.into());
    }
    let body = v.to_string();
    if body.len() > arcade_ledger::aproto::MAX_PROFILE {
        return Err(format!("the profile is {} bytes; 1,000 at most", body.len()));
    }
    let (txid, fee) = send(node, &arcade_ledger::aproto::payload(arcade_ledger::aproto::OP_PROFILE, body.as_bytes()))?;
    // Its wallet proofs go with it, by themselves (Martin 2026-11-18); when they can't yet, the background look sends
    // them later.
    let wallets = crate::walletproofs::publish(node, false).unwrap_or_else(|e| json!({"sent": false, "why": e}));
    Ok(json!({"txid": txid, "fee": fee, "bytes": body.len(), "wallets": wallets}))
}

/// `a.follow {tag}` / `a.unfollow {tag}`.
pub fn a_follow(node: &Node, tag: &str, on: bool) -> Result<Value, String> {
    let p = write_chain()?;
    let t = tag.trim().trim_start_matches('@').to_lowercase();
    // A key hash (someone known by a name on another chain, no ARCADE @tag yet) is followed as #<hex>.
    let t = if t.len() == 40 && t.bytes().all(|b| b.is_ascii_hexdigit()) { format!("#{t}") } else { t };
    // Tags per chain: `@alice.arctest` is ARCADE's @alice; another chain's (`@alice.doge`) is its holder's key, followed
    // as #<hex> (the follow op names ARCADE tags or key hashes).
    let t = match t.contains('.').then(|| crate::hometag::parse(node, &t)).transpose()? {
        Some((name, Some(tp))) if crate::arcborrow::is(tp) => name,
        Some((name, Some(tp))) => {
            let (holder, _) = crate::hometag::owner_on_checked(node, tp, &name)?.ok_or_else(|| format!("@{} is not claimed", crate::hometag::qualified(tp, &name)))?;
            let (_, h) = arcade_ledger::address::decode_base58check(&holder).filter(|(_, h)| h.len() == 20).ok_or("the tag's address doesn't read")?;
            format!("#{}", hex::encode(h))
        }
        _ => t,
    };
    let me = sender(node, p)?.0;
    if crate::arcsvc::borrowed(p) {
        // Borrowed: the tables are at the arcades keeping the chain, so they say whether the tag is claimed. Whether
        // this wallet follows it already isn't asked: a second follow (or an unfollow of no one) changes nothing on chain.
        if !t.starts_with('#') && crate::claims::arc_owner(node, p, &t)?.is_none() {
            return Err(format!("@{t} isn't claimed on the ARCADE chain"));
        }
    } else {
        if !t.starts_with('#') && arcade_ledger::index::tag_row(&node.chain, p.chain_no, &t).map_err(|e| e.to_string())?.is_none() {
            return Err(format!("@{t} isn't claimed on the ARCADE chain"));
        }
        let following = node.chain.store(p.chain_no).read(|r| r.get(arcade_ledger::engine::A_FOLLOW, &arcade_store::KeyBuf::new().u16(p.chain_no).bytes(me.as_bytes()).u8(0).bytes(t.as_bytes()).done())).ok().flatten().is_some();
        if on == following {
            return Err(if on { format!("you already follow @{t}") } else { format!("you don't follow @{t}") });
        }
    }
    let op = if on { arcade_ledger::aproto::OP_FOLLOW } else { arcade_ledger::aproto::OP_UNFOLLOW };
    let body = t.trim_start_matches('#');
    let (txid, fee) = send(node, &arcade_ledger::aproto::payload(op, body.as_bytes()))?;
    Ok(json!({"txid": txid, "fee": fee, "tag": t, "following": on}))
}

/// `a.follows {tag | address}`: who it follows and who follows it, each with their card.
pub fn a_follows(node: &Node, req: &Value) -> Result<Value, String> {
    if let Some(v) = borrowed_read(node, "a_follows", about_me(node, req)?)? {
        return Ok(v);
    }
    let c = chain()?.chain_no;
    let a = who(node, req)?;
    let pre = arcade_store::KeyBuf::new().u16(c).bytes(a.as_bytes()).u8(0).done();
    let following: Vec<Value> = scan(node, c, arcade_ledger::engine::A_FOLLOW, &pre).into_iter().filter_map(|(k, v)| {
        let t = String::from_utf8(k[pre.len()..].to_vec()).ok()?;
        let h = match t.strip_prefix('#') {
            Some(hx) => hex::decode(hx).ok()?,
            None => h160_of(&arcade_ledger::index::tag_row(&node.chain, c, &t).ok().flatten()?.address)?,
        };
        let mut x = directory_card(node, &h)?;
        x["since"] = v["h"].clone();
        x["follow_key"] = json!(t);
        Some(x)
    }).collect();
    // Followers: those following our @tag, and those following our key hash.
    let tag = arcade_ledger::index::tag_of(&node.chain, c, &a).ok().flatten();
    let mut targets: Vec<String> = tag.into_iter().collect();
    if let Some(h) = h160_of(&a) {
        targets.push(format!("#{}", hex::encode(h)));
    }
    let mut followers: Vec<Value> = Vec::new();
    for t in targets {
        let fpre = arcade_store::KeyBuf::new().u16(c).bytes(t.as_bytes()).u8(0).done();
        for (k, v) in scan(node, c, arcade_ledger::engine::A_FOLLOWER, &fpre) {
            let Ok(addr) = String::from_utf8(k[fpre.len()..].to_vec()) else { continue };
            if followers.iter().any(|f| f["address"] == addr.as_str()) {
                continue;
            }
            let Some(mut x) = h160_of(&addr).and_then(|h| directory_card(node, &h)) else { continue };
            x["since"] = v["h"].clone();
            x["you_follow"] = json!(following.iter().any(|g| g["address"] == addr.as_str()));
            followers.push(x);
        }
    }
    followers.sort_by_key(|x| std::cmp::Reverse(x["since"].as_u64().unwrap_or(0)));
    Ok(json!({"address": a, "following": following, "followers": followers}))
}

const CONTACT_BOUND: &[u8] = b"arcade A:// contact v1";

/// `a.contacts`: this wallet's address book, opened with its own key.
pub fn a_contacts(node: &Node) -> Result<Value, String> {
    let p = write_chain()?;
    let me = sender(node, p)?.0;
    let k = crate::msgsvc::keys(node)?;
    // The sealed rows: this arcade's own table, or (ARCADE borrowed) the keeper's copy of them, opened here.
    let rows: Vec<(String, Value)> = match borrowed_read(node, "contact_rows", json!({"address": me}))? {
        Some(v) => v["rows"].as_array().cloned().unwrap_or_default().into_iter()
            .filter_map(|r| Some((r["id"].as_str()?.to_string(), json!({"c": r["c"], "h": r["h"]})))).collect(),
        None => {
            let pre = arcade_store::KeyBuf::new().u16(p.chain_no).bytes(me.as_bytes()).u8(0).done();
            scan(node, p.chain_no, arcade_ledger::engine::A_CONTACT, &pre).into_iter().map(|(key, v)| (hex::encode(&key[pre.len()..]), v)).collect()
        }
    };
    let mut opened: Vec<Value> = rows.into_iter().filter_map(|(id, v)| {
        let sealed = hex::decode(v["c"].as_str()?).ok()?;
        let (_, plain) = arcade_msg::envelope::open(&k.secret, CONTACT_BOUND, &sealed).ok()?;
        let mut e: Value = serde_json::from_slice(&plain).ok()?;
        e["id"] = json!(id);
        e["height"] = v["h"].clone();
        Some(e)
    }).collect();
    // Someone added from their profile shows it as it is now: a new name, bio or address is seen at once.
    let borrowed = chain().is_err();
    let addr_of = |e: &Value| -> Option<String> {
        e["addresses"]["arcade"].as_str().map(String::from).or_else(|| {
            let t = e["tag"].as_str()?;
            if borrowed {
                crate::hometag::owner_on(node, p, t).map(|(a, ..)| a)
            } else {
                arcade_ledger::index::tag_row(&node.chain, p.chain_no, t).ok().flatten().map(|r| r.address)
            }
        })
    };
    let keys: Vec<String> = opened.iter().filter_map(|e| addr_of(e).as_deref().and_then(h160_of).map(hex::encode)).collect();
    let remote: serde_json::Map<String, Value> = if borrowed && !keys.is_empty() {
        borrowed_read(node, "cards", json!({"keys": keys}))?.and_then(|v| v["cards"].as_object().cloned()).unwrap_or_default()
    } else {
        Default::default()
    };
    let card_of = |h: &[u8]| -> Option<Value> {
        if borrowed { remote.get(&hex::encode(h)).cloned() } else { directory_card(node, h) }
    };
    let mut out: Vec<Value> = opened.drain(..).map(|mut e| {
        if let Some(live) = addr_of(&e).as_deref().and_then(h160_of).and_then(|h| card_of(&h)) {
            if let Some(n) = live["profile"]["n"].as_str() {
                e["name"] = json!(n);
            }
            if let Some(b) = live["profile"]["b"].as_str() {
                e["note"] = json!(b);
            }
            if let Some(m) = live["profile"]["a"].as_object() {
                for (k, x) in m {
                    e["addresses"][k] = x.clone();
                }
            }
            e["tag"] = live["tag"].clone();
            e["home"] = live["home"].clone();
            e["profile_picture"] = live["profile"]["p"].clone();
        }
        e
    }).collect();
    out.sort_by(|a, b| a["name"].as_str().unwrap_or("").to_lowercase().cmp(&b["name"].as_str().unwrap_or("").to_lowercase()));
    Ok(json!({"contacts": out}))
}

/// `a.contact_save {id?, name, tag?, addresses, note?}`: one entry, sealed to this wallet's own key.
pub fn a_contact_save(node: &Node, req: &Value) -> Result<Value, String> {
    let name = req["name"].as_str().map(str::trim).filter(|s| !s.is_empty()).ok_or("a contact needs a name")?;
    let id: [u8; 8] = match req["id"].as_str() {
        Some(h) => hex::decode(h).ok().and_then(|b| b.try_into().ok()).ok_or("id is 16 hex")?,
        None => {
            let mut b = [0u8; 8];
            rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut b);
            b
        }
    };
    let entry = json!({"name": name, "tag": req["tag"].as_str().map(|t| t.trim().trim_start_matches('@').to_lowercase()).filter(|t| !t.is_empty()),
                       "addresses": req["addresses"].as_object().cloned().unwrap_or_default(), "note": req["note"].as_str().unwrap_or(""), "home": req["home"].clone()});
    let k = crate::msgsvc::keys(node)?;
    let sealed = arcade_msg::envelope::seal(&k.secret, &k.public, CONTACT_BOUND, entry.to_string().as_bytes());
    let mut body = id.to_vec();
    body.extend_from_slice(&sealed);
    if body.len() > arcade_ledger::aproto::MAX_CONTACT {
        return Err("that entry is too long (about 850 bytes of names, addresses and note at most)".into());
    }
    let (txid, fee) = send(node, &arcade_ledger::aproto::payload(arcade_ledger::aproto::OP_CONTACT, &body))?;
    Ok(json!({"txid": txid, "fee": fee, "id": hex::encode(id)}))
}

/// `a.contact_delete {id}`.
pub fn a_contact_delete(node: &Node, id: &str) -> Result<Value, String> {
    let id: [u8; 8] = hex::decode(id).ok().and_then(|b| b.try_into().ok()).ok_or("id is 16 hex")?;
    let (txid, fee) = send(node, &arcade_ledger::aproto::payload(arcade_ledger::aproto::OP_CONTACT_DEL, &id))?;
    Ok(json!({"txid": txid, "fee": fee}))
}

/// `a.post {text}`.
pub fn a_post(node: &Node, text: &str) -> Result<Value, String> {
    let t = text.trim();
    if t.is_empty() || t.len() > arcade_ledger::aproto::MAX_POST {
        return Err("a post is 1 to 1,000 bytes".into());
    }
    let (txid, fee) = send(node, &arcade_ledger::aproto::payload(arcade_ledger::aproto::OP_POST, t.as_bytes()))?;
    Ok(json!({"txid": txid, "fee": fee}))
}

/// `a.posts {tag | address | following: true, limit?}`: posts, newest first (`following`: of everyone this wallet follows).
pub fn a_posts(node: &Node, req: &Value) -> Result<Value, String> {
    let following_of = if req["following"] == true { Some(sender(node, write_chain()?)?.0) } else { None };
    let args = match &following_of {
        Some(me) => json!({"following_of": me, "limit": req["limit"]}),
        None => about_me(node, req)?,
    };
    if let Some(v) = borrowed_read(node, "a_posts", args)? {
        return Ok(v);
    }
    a_posts_as(node, req, following_of.as_deref())
}

/// [`a_posts`] on this arcade's own tables; `following_of`: the posts of everyone that address follows.
fn a_posts_as(node: &Node, req: &Value, following_of: Option<&str>) -> Result<Value, String> {
    let c = chain()?.chain_no;
    let authors: Vec<String> = if let Some(me) = following_of {
        a_follows(node, &json!({"address": me}))?["following"].as_array().map(|v| v.iter().filter_map(|x| x["address"].as_str().map(String::from)).collect()).unwrap_or_default()
    } else {
        vec![who(node, req)?]
    };
    let mut posts = Vec::new();
    for a in authors {
        let pre = arcade_store::KeyBuf::new().u16(c).bytes(a.as_bytes()).u8(0).done();
        let tag = arcade_ledger::index::tag_of(&node.chain, c, &a).ok().flatten();
        for (k, v) in scan(node, c, arcade_ledger::engine::A_POST, &pre) {
            let h = k.get(pre.len()..pre.len() + 4).and_then(|b| b.try_into().ok()).map(u32::from_be_bytes).unwrap_or(0);
            posts.push(json!({"address": a, "tag": tag, "text": v["t"], "txid": v["txid"], "height": h}));
        }
    }
    posts.sort_by_key(|p| std::cmp::Reverse(p["height"].as_u64().unwrap_or(0)));
    posts.truncate(req["limit"].as_u64().unwrap_or(100) as usize);
    Ok(json!({"posts": posts}))
}

/// Every address this wallet has to show on a profile (Martin 2026-11-15: "automatically get all your addresses from
/// your vault including imported wallets"): its own on each coin installed here, then each imported wallet's (a second
/// one on a coin as `doge-2` ...). At most 12, as a profile holds.
pub fn wallet_addresses(node: &Node) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    if !node.app.has_wallet().unwrap_or(false) {
        return out;
    }
    // The coin chains (Pepecoin testnet first); the ARCADE address is added apart.
    let mut chains: Vec<&'static ChainParams> = vec![node.params];
    let extra: Vec<String> = node.extra.lock().unwrap().keys().map(|k| k.to_string()).collect();
    chains.extend(extra.iter().filter_map(|k| crate::chains::find(k)));
    for cp in chains {
        if crate::arcsvc::runs(cp) || out.contains_key(cp.key) {
            continue;
        }
        let key = node.app.tag_keys(cp.chain_no).ok().and_then(|ks| ks.into_iter().find(|k| k.tag.is_some()).or_else(|| node.app.tag_keys(cp.chain_no).ok()?.into_iter().next()))
            .or_else(|| node.app.tag_key(cp, 0).ok().map(|x| x.0));
        let Some(k) = key else { continue };
        let Some(pk) = hex::decode(&k.pubkey).ok().and_then(|b| arcade_wallet::secp256k1::PublicKey::from_slice(&b).ok()) else { continue };
        if let Some((addr, _)) = crate::coinsend::key_scripts(cp, &pk).into_iter().next() {
            out.insert(cp.key.to_string(), json!(addr));
        }
    }
    for e in crate::importsvc::list_raw(node) {
        let (Some(chain), Some(addr)) = (e["chain"].as_str(), e["address"].as_str()) else { continue };
        if out.values().any(|v| v == addr) {
            continue;
        }
        let mut name = chain.to_string();
        let mut n = 2;
        while out.contains_key(&name) {
            name = format!("{chain}-{n}");
            n += 1;
        }
        out.insert(name, json!(addr));
    }
    let keep: Vec<String> = out.keys().take(12).cloned().collect();
    out.retain(|k, _| keep.contains(k));
    out
}

// ---------------- the directory: everyone who published a name anywhere this arcade reads ----------------
//
// Martin 2026-11-15: ARCADE is the home chain, but the arcade ledgers of the other chains are read too, with a home
// chain marked on each card, and DogeFeed's names (D://, `.dogefeed` names on Dogecoin). One key, one person: the same
// key hash is the same wallet on every chain, so an ARCADE address follows from any of them.

fn h160_of(address: &str) -> Option<Vec<u8>> {
    arcade_ledger::address::decode_base58check(address).filter(|(_, h)| h.len() == 20).map(|(_, h)| h)
}

/// The chains whose arcade ledger is read here, besides ARCADE: the home chain and every chain installed with its
/// ledger or profiles index.
fn ledger_chains(node: &Node) -> Vec<&'static ChainParams> {
    // The old home (Pepecoin testnet) even once ARCADE is home: its ledger stays readable.
    let mut out = vec![node.params];
    let extra: Vec<String> = node.extra.lock().unwrap().keys().map(|k| k.to_string()).collect();
    for k in extra {
        if let Some(cp) = crate::chains::find(&k) {
            let ix = node.chain.indexers_for(cp.chain_no);
            if !crate::arcsvc::runs(cp) && (ix.contains(&"ledger.v1") || ix.contains(&"profiles.v1")) && !out.iter().any(|o| o.chain_no == cp.chain_no) {
                out.push(cp);
            }
        }
    }
    out
}

/// One person's card by their key hash: their ARCADE profile and @tag first; else the first chain whose arcade ledger
/// holds a profile or @tag of theirs; else their DogeFeed name. `home` says where the name was published.
fn directory_card(node: &Node, h: &[u8]) -> Option<Value> {
    let p = chain().ok()?;
    let c = p.chain_no;
    let arc_addr = arcade_ledger::address::base58check(p.p2pkh_version, h);
    let mut v = card(node, c, &arc_addr);
    v["key"] = json!(hex::encode(h));
    if !v["profile"].is_null() || !v["tag"].is_null() {
        v["home"] = json!({"chain": p.name, "key": p.key, "via": if v["profile"].is_null() { "@tag" } else { "A://" }});
        return Some(v);
    }
    for cp in ledger_chains(node) {
        let a = arcade_ledger::address::base58check(cp.p2pkh_version, h);
        let tag = arcade_ledger::index::tag_of(&node.chain, cp.chain_no, &a).ok().flatten();
        let prof = arcade_ledger::index::profile(&node.chain, cp.chain_no, &a).ok().flatten();
        if tag.is_none() && prof.is_none() {
            continue;
        }
        let mut pv = serde_json::Map::new();
        if let Some(n) = prof.as_ref().and_then(|x| x["name"].as_str()).filter(|n| !n.is_empty()).map(String::from).or_else(|| tag.as_ref().map(|t| format!("@{t}"))) {
            pv.insert("n".into(), json!(n));
        }
        for (from, to) in [("bio", "b"), ("url", "u")] {
            if let Some(x) = prof.as_ref().and_then(|x| x[from].as_str()).filter(|x| !x.is_empty()) {
                pv.insert(to.into(), json!(x));
            }
        }
        pv.insert("a".into(), json!({cp.key: a}));
        v["profile"] = Value::Object(pv);
        v["other_tag"] = json!(tag);
        v["home"] = json!({"chain": cp.name, "key": cp.key, "via": if prof.is_some() { "profile" } else { "@tag" }});
        return Some(v);
    }
    let doge = crate::chains::find("doge")?;
    let da = arcade_ledger::address::base58check(doge.p2pkh_version, h);
    let name = crate::dproto::handle_of(node, &da)?;
    v["profile"] = json!({"n": name, "a": {"doge": da}});
    v["home"] = json!({"chain": "Dogecoin", "key": "doge", "via": "DogeFeed"});
    Some(v)
}

/// `a.search {q}`: everyone whose published name or @tag holds `q`, on ARCADE, on the other chains' arcade ledgers read
/// here, and on DogeFeed, one card a person (Martin 2026-11-15: "a simple search and click to add someone").
pub fn a_search(node: &Node, q: &str) -> Result<Value, String> {
    let me = sender(node, write_chain()?)?.0;
    if let Some(v) = borrowed_read(node, "a_search", json!({"q": q, "me": me}))? {
        return Ok(v);
    }
    a_search_as(node, q, Some(&me))
}

/// [`a_search`] on this arcade's own tables, leaving out `me` (the one searching).
fn a_search_as(node: &Node, q: &str, me: Option<&str>) -> Result<Value, String> {
    let p = chain()?;
    let c = p.chain_no;
    let q = q.trim().trim_start_matches('@').to_lowercase();
    if q.len() < 2 {
        return Ok(json!({"results": []}));
    }
    let mut keys: Vec<Vec<u8>> = Vec::new();
    let mut add = |a: &str| {
        if let Some(h) = h160_of(a) {
            if !keys.contains(&h) {
                keys.push(h);
            }
        }
    };
    let pre = arcade_store::KeyBuf::new().u16(c).done();
    for (k, v) in scan(node, c, arcade_ledger::engine::A_PROFILE, &pre) {
        if v["v"]["n"].as_str().is_some_and(|n| n.to_lowercase().contains(&q)) {
            add(&String::from_utf8_lossy(&k[2..]));
        }
    }
    for cp in std::iter::once(p).chain(ledger_chains(node)) {
        for (tag, row) in arcade_ledger::index::read_tags(&node.chain, cp.chain_no).unwrap_or_default() {
            if tag.contains(&q) {
                add(&row.address);
            }
        }
        let ppre = arcade_store::KeyBuf::new().u16(cp.chain_no).done();
        for (k, v) in scan(node, cp.chain_no, arcade_ledger::index::PROFILE, &ppre) {
            if v["name"].as_str().is_some_and(|n| n.to_lowercase().contains(&q)) {
                add(&String::from_utf8_lossy(&k[2..]));
            }
        }
    }
    for (a, _) in crate::dproto::handles_matching(&q, 25) {
        add(&a);
    }
    let me = me.and_then(h160_of);
    let out: Vec<Value> = keys.iter().filter(|h| Some(*h) != me.as_ref()).take(25).filter_map(|h| directory_card(node, h)).collect();
    Ok(json!({"results": out}))
}

/// The key hash a call names: `key` (40 hex), an ARCADE or other chain's `address`, or an @`tag` on ARCADE.
fn key_of(node: &Node, req: &Value) -> Result<Vec<u8>, String> {
    if let Some(k) = req["key"].as_str() {
        return hex::decode(k).ok().filter(|h| h.len() == 20).ok_or_else(|| "key is a key hash (40 hex)".to_string());
    }
    h160_of(&who(node, req)?).ok_or_else(|| "not an address".into())
}

/// `a.contact_add {key | tag | address}`: add someone from what they published (their name, @tag, addresses and bio,
/// wherever it was published), sealed to this wallet's key like any entry; the same person again replaces it.
pub fn a_contact_add(node: &Node, req: &Value) -> Result<Value, String> {
    let h = key_of(node, req)?;
    let card = directory_card(node, &h).ok_or("they haven't published a name anywhere this arcade reads")?;
    let prof = card["profile"].clone();
    let tag = card["tag"].as_str().map(String::from);
    let name = prof["n"].as_str().map(String::from).or_else(|| tag.as_ref().map(|t| format!("@{t}"))).ok_or("they have no published name")?;
    let a = card["address"].as_str().unwrap_or("").to_string();
    let mut addrs = serde_json::Map::new();
    addrs.insert("arcade".into(), json!(a));
    if let Some(m) = prof["a"].as_object() {
        for (k, v) in m {
            addrs.insert(k.clone(), v.clone());
        }
    }
    let existing = a_contacts(node)?["contacts"].as_array().cloned().unwrap_or_default().into_iter().find(|e| e["addresses"]["arcade"] == a.as_str());
    a_contact_save(node, &json!({"id": existing.as_ref().map(|e| e["id"].clone()), "name": name, "tag": tag, "addresses": addrs,
                                 "note": prof["b"].as_str().unwrap_or(""), "home": card["home"]}))
}
