//! What this arcade earns on the ARCADE chain, and when it is paid (docs/EARN.md): read-only views for arc.earn,
//! arc.status and /r/explorer/earn. Nothing here decides anything: it says ahead what the registry
//! (arcade_arc::registry) will do at this arcade's next turn and next registration batch.

use arcade_arc::block::{self, Key};
use arcade_arc::catalog::{self, CHALLENGE_WINDOW};
use arcade_arc::params::{self, Net};
use arcade_arc::registry::{self, Earner, Pick, Registry};
use serde_json::{json, Value};

/// Whether an audit pick can no longer be answered after block `tip`.
fn closed(p: &Pick, tip: u64) -> bool {
    !p.passed
        && match p.commit {
            None => tip >= p.at + params::ANSWER_WINDOW,
            Some((_, hc)) => tip >= hc + params::CHALLENGE_DELAY + params::ANSWER_WINDOW,
        }
}

/// Why an owner's pay won't go out at its next turn, as the registry's release decides it then: (code, words); None
/// when it will.
pub fn owner_wait(net: &Net, reg: &Registry, me: &Key, tip: u64) -> Option<(&'static str, String)> {
    let e = Earner::Owner(*me);
    let accrued = reg.accrued_of(&e);
    let next = registry::next_turn(net, &e, tip);
    if accrued == 0 {
        return Some(("nothing", "nothing accrued yet: registered replicas earn from each epoch's end (every 100 blocks)".into()));
    }
    let picks = reg.audits_of(me);
    if picks.is_empty() {
        return Some(("no_audit", format!(
            "no audit yet: this arcade's audits are drawn at its turn (block {next}); pay goes out at the turn after (block {}) if they all pass",
            next + net.audit_period
        )));
    }
    if picks.iter().any(|p| closed(p, tip)) {
        return Some(("audit_missed", format!("an audit was missed: at block {next} this period's pay goes back to the pot")));
    }
    if let Some(p) = picks.iter().filter(|p| !p.passed).min_by_key(|p| p.at) {
        return Some(("audits_open", format!("audits under way (the next picked at block {}): paid at block {next} if every one passes", p.at)));
    }
    let (ok, not) = reg.verdicts_of(me);
    if ok + not >= params::HOLD_VERDICTS && not > ok {
        let times = reg.held_of(me).1 + 1;
        return Some(("held", if times >= params::HOLD_FORFEIT {
            format!("serving checks: {not} of {} verdicts say not served, {times} turns in a row: at block {next} the pay goes back to the pot", ok + not)
        } else {
            format!("held by serving checks: {not} of {} verdicts say not served; held at block {next} ({times} in a row; {} in a row goes back to the pot)", ok + not, params::HOLD_FORFEIT)
        }));
    }
    if accrued < params::MIN_PAYOUT {
        return Some(("under_min", "under 0.01 ARCADE: waits for a later turn".into()));
    }
    None
}

/// This arcade's pay: as an owner (its registered replicas) and as each genesis earner its key holds bytes of. The
/// turn is its block in each audit period; next_payout the earliest block it can be paid in (none while it depends
/// on more pay accruing).
pub fn pay(net: &Net, reg: &Registry, me: &Key, tip: u64) -> Value {
    use arcade_arc::chain::Storage;
    let e = Earner::Owner(*me);
    let next = registry::next_turn(net, &e, tip);
    let wait = owner_wait(net, reg, me, tip);
    let next_payout = match wait.as_ref().map(|w| w.0) {
        None | Some("audits_open") => Some(next),
        Some("no_audit" | "audit_missed" | "held") => Some(next + net.audit_period),
        _ => None,
    };
    let reps: Vec<_> = reg.all_replicas().into_iter().filter(|r| r.2 == *me).collect();
    let draw_waiting = reps.iter().filter(|r| reg.draw_from(&r.0).is_none_or(|f| f > tip + 1)).count();
    let picks: Vec<Value> = reg
        .audits_of(me)
        .iter()
        .map(|p| json!({"replica": hex::encode(p.replica), "at": p.at, "committed": p.commit.map(|c| c.1), "passed": p.passed, "missed": closed(p, tip)}))
        .collect();
    let genesis: Vec<Value> = reg
        .genesis_earners()
        .into_iter()
        .filter(|g| g.2.contains(me))
        .map(|(pay, w, _, acc)| {
            let g = Earner::Genesis(pay);
            let n = registry::next_turn(net, &g, tip);
            // A genesis earner is paid at its turn without audits: only the minimum can hold it back.
            json!({"address": arcade_ledger::address::base58check(pay[0], &pay[1..]), "weight": w, "turn": registry::turn(net, &g), "next_turn": n,
                   "accrued": acc, "next_payout": (acc >= params::MIN_PAYOUT).then_some(n),
                   "waits": if acc == 0 { Some("nothing") } else if acc < params::MIN_PAYOUT { Some("under_min") } else { None }})
        })
        .collect();
    let (ok, not) = reg.verdicts_of(me);
    let (held, held_times) = reg.held_of(me);
    json!({
        "period": net.audit_period,
        "turn": registry::turn(net, &e),
        "next_turn": next,
        "next_payout": next_payout,
        "accrued": reg.accrued_of(&e),
        "waits": wait.as_ref().map(|w| w.0),
        "why": wait.map(|w| w.1),
        // Pay is shared by pay weight (every registered replica that hasn't missed its last audit); blocks are written
        // by draw weight (replicas DRAW_WEIGHT_WAIT blocks after their first passed audit; from rules version 4 on the
        // testnet, right after it).
        "pay_weight": reg.owner_pay_weight(me),
        "draw_weight": reg.owner_draw_weight(me),
        "weight": reg.weight(me),
        "total_weight": reg.total_weight().min(u64::MAX as u128) as u64,
        "replicas": reps.len(),
        "replicas_not_drawing": draw_waiting,
        "draw_weight_wait": params::draw_weight_wait(net, tip + 1),
        "silenced_until": reg.silenced_until(me),
        "verdicts": {"served": ok, "not_served": not},
        "held": held,
        "held_times": held_times,
        "audits": picks,
        "genesis": genesis,
    })
}

/// The block this arcade's next registration batch can go in: one batch an audit period, from the block that held
/// the last batch's proof (arcseg::register; a batch being proved now ends its own period later).
pub fn next_batch(net: &Net, reg: &Registry, me: &Key, tip: u64) -> u64 {
    if let Some((_, hc)) = reg.commit_of(me).filter(|(_, hc)| tip <= hc + params::CHALLENGE_DELAY + params::ANSWER_WINDOW) {
        return hc + params::CHALLENGE_DELAY + 1 + net.audit_period;
    }
    match reg.last_batch(me) {
        0 => tip + 1,
        last => (last + net.audit_period).max(tip + 1),
    }
}

/// This arcade's sealed replicas (of `chain`, or all) that aren't registered, and what each waits for: registering
/// (in the batch being proved), the next batch (and the block it can go in), a light entry's challenge window, an
/// entry offered but not confirmed, or no entry at all.
pub fn unregistered(net: &Net, reg: &Registry, me: &Key, tip: u64, sealed: &[crate::arcseal::Replica], chain: Option<&str>) -> Value {
    let batch = next_batch(net, reg, me, tip);
    let live = reg.commit_of(me).filter(|(_, hc)| tip <= hc + params::CHALLENGE_DELAY + params::ANSWER_WINDOW);
    let cat = reg.catalog();
    let mut rows = Vec::new();
    for r in sealed.iter().filter(|r| chain.is_none_or(|c| r.chain == c)) {
        if reg.registered(&block::replica_id(net, &r.segment, me)) {
            continue;
        }
        let mut v = json!({"chain": r.chain, "family": r.family, "name": crate::arcseg::family_name(r.family), "first": r.first, "last": r.last,
                           "bytes": r.bytes, "segment": hex::encode(r.segment)});
        if let Some((_, hc)) = live.as_ref().filter(|(pairs, _)| pairs.iter().any(|x| x.0 == r.segment)) {
            v["state"] = json!("registering");
            v["commit_height"] = json!(hc);
            v["proof_by"] = json!(hc + params::CHALLENGE_DELAY + params::ANSWER_WINDOW);
        } else if let Some(l) = cat.get(&r.segment) {
            // A light entry (a tenth of the weight) registers only after its challenge window.
            let settled = if l.light { l.height + CHALLENGE_WINDOW } else { l.height };
            v["state"] = json!(if settled > tip + 1 { "challenge window" } else { "next batch" });
            v["listed_at"] = json!(l.height);
            if l.light {
                v["window_ends"] = json!(settled);
            }
            v["batch_at"] = json!(batch.max(settled));
        } else if let Some(c) = cat.waiting(&r.segment) {
            v["state"] = json!("offered");
            v["behind"] = json!(c.behind.len());
            v["needs"] = json!(if catalog::light_family(net, r.family, tip + 1) { "more than a tenth of the draw weight" } else { "more than half of the draw weight" });
        } else if cat.spans(&r.chain, r.family).any(|(f, l, x)| f == r.first && l == r.last && x.is_some()) {
            v["state"] = json!("other root");
            v["why"] = json!("these heights are listed with another root: this copy can't register");
        } else {
            v["state"] = json!("not listed");
            v["why"] = json!("no entry yet: only arcades with draw weight offer and attest entries, for the chains and indexes they keep");
        }
        rows.push(v);
    }
    json!({"next_batch": batch, "last_batch": Some(reg.last_batch(me)).filter(|&h| h > 0), "replicas": rows})
}

/// How a family of a chain stands in the catalog: segments listed, entries waiting, and a note when none is listed.
pub fn listing(net: &Net, reg: &Registry, chain: &str, family: u8, tip: u64) -> Value {
    let cat = reg.catalog();
    let listed = cat.spans(chain, family).filter(|s| s.2.is_some()).count();
    let waiting = cat.all_waiting().filter(|(_, c)| c.entry.chain == chain && c.entry.family == family).count();
    let need = if catalog::light_family(net, family, tip + 1) { "a tenth" } else { "half" };
    let note = match (listed, waiting) {
        (0, 0) => format!("not listed: no arcade with draw weight has offered it yet (an entry needs more than {need} of the draw weight)"),
        (0, _) => format!("offered, not listed yet: an entry needs more than {need} of the draw weight behind it"),
        _ => String::new(),
    };
    json!({"listed": listed, "waiting": waiting, "note": note})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick(at: u64, commit: Option<u64>, passed: bool) -> Pick {
        Pick { replica: [1; 32], at, commit: commit.map(|h| ([0; 32], h)), passed }
    }

    #[test]
    fn a_pick_is_missed_once_its_window_is_over() {
        let aw = params::ANSWER_WINDOW;
        let cd = params::CHALLENGE_DELAY;
        assert!(!closed(&pick(100, None, false), 100 + aw - 1));
        assert!(closed(&pick(100, None, false), 100 + aw));
        assert!(!closed(&pick(100, Some(105), false), 105 + cd + aw - 1));
        assert!(closed(&pick(100, Some(105), false), 105 + cd + aw));
        assert!(!closed(&pick(100, Some(105), true), 10_000));
    }

    #[test]
    fn a_new_arcade_has_nothing_accrued_and_registers_at_once() {
        let net = params::TESTNET;
        let reg = Registry::new(&net, &[], params::SEGMENT_BYTES).unwrap();
        let me = [7; 32];
        assert_eq!(owner_wait(&net, &reg, &me, 50).map(|w| w.0), Some("nothing"));
        assert_eq!(next_batch(&net, &reg, &me, 50), 51);
        let v = pay(&net, &reg, &me, 50);
        assert_eq!(v["next_turn"], registry::next_turn(&net, &Earner::Owner(me), 50));
        assert!(v["next_payout"].is_null() && v["replicas"] == 0);
        assert_eq!(unregistered(&net, &reg, &me, 50, &[], None)["replicas"], json!([]));
    }
}
