//! How many unconfirmed transactions may stand in a row, and which piece of a long inscription
//! to send next.
//!
//! A node keeps about [`LIMIT`] waiting transactions that spend one another (the ARCADE pool's
//! row, and a coin node's default ancestor limit, counting the new transaction). Coins already
//! waiting in the wallet count toward that row. A publish of many pieces therefore goes out in
//! batches, and a piece that was signed is never signed again.

use arcade_chain::Hash256;
use std::collections::HashSet;

/// Longest row of unconfirmed transactions that spend one another, counting the new one.
/// The ARCADE pool refuses a longer row ([`arcade_arc::pool::MAX_ROW`]); a coin node's default
/// ancestor limit is the same length.
pub const LIMIT: usize = arcade_arc::pool::MAX_ROW;

/// How many more transactions can be chained onto a coin whose unconfirmed row is already
/// `tip_row` (0 when that coin is confirmed). Never more than `remaining`, and never past [`LIMIT`].
pub fn batch_len(tip_row: usize, remaining: usize) -> usize {
    LIMIT.saturating_sub(tip_row).min(remaining)
}

/// The row a new transaction would stand in, given the pool row of each distinct waiting parent
/// (confirmed parents are left out). Matches the ARCADE pool: one plus the sum of those rows.
pub fn projected(parent_rows: &[usize]) -> usize {
    1 + parent_rows.iter().sum::<usize>()
}

/// Whether a transaction at `projected_row` (from [`projected`]) still fits.
pub fn fits(projected_row: usize) -> bool {
    projected_row <= LIMIT
}

/// What to do with piece `n` of an inscription that already broadcast `sent` and may have one
/// transaction signed but not yet known to be out (`staged`). A staged piece is broadcast again
/// as the same bytes; it is never built a second time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    Broadcast(usize),
    Build(usize),
    Done,
}

pub fn next_piece(total: usize, sent: &[usize], staged: Option<usize>) -> Next {
    if let Some(n) = staged {
        if n < total && !sent.contains(&n) {
            return Next::Broadcast(n);
        }
    }
    match (0..total).find(|n| !sent.contains(n)) {
        Some(n) => Next::Build(n),
        None => Next::Done,
    }
}

/// How many of `roots` are unconfirmed, including every unconfirmed ancestor `fetch` can name.
/// `fetch` returns that transaction's input ids when it is waiting, and `None` when it is not
/// (confirmed, or unknown). Distinct ids only, so a coin spent by two inputs is counted once.
pub fn ancestor_count(
    roots: &[Hash256],
    mut fetch: impl FnMut(&Hash256) -> Option<Vec<Hash256>>,
) -> usize {
    let mut seen = HashSet::new();
    let mut stack = roots.to_vec();
    while let Some(id) = stack.pop() {
        if seen.contains(&id) {
            continue;
        }
        match fetch(&id) {
            Some(parents) => {
                seen.insert(id);
                stack.extend(parents);
            }
            None => {}
        }
        if seen.len() > LIMIT.saturating_add(8) {
            break;
        }
    }
    seen.len()
}

/// A refusal from this arcade's pool or from a coin node that will not take a longer chain.
pub fn is_refusal(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("too many waiting transactions")
        || e.contains("too-long-mempool-chain")
        || e.contains("too long mempool chain")
}

/// What to tell the person: how many are already waiting, and that the rest goes out on its own.
pub fn note(sent: usize, total: usize, waiting: usize) -> String {
    if sent == 0 {
        format!(
            "{waiting} transactions are already waiting in a row, which is as many as the network takes at once ({LIMIT}). This continues automatically as earlier ones confirm."
        )
    } else {
        format!(
            "{sent} of {total} sent, {waiting} waiting in a row (the network takes {LIMIT} unconfirmed at once). The rest continues automatically as earlier ones confirm."
        )
    }
}

/// The same stop, for a send that does not continue on its own (a message, not an inscription).
pub fn held_note(sent: usize, total: usize, waiting: usize) -> String {
    if sent == 0 {
        format!(
            "{waiting} transactions are already waiting in a row, which is as many as the network takes at once ({LIMIT}); nothing was sent"
        )
    } else {
        format!(
            "{sent} of {total} sent, {waiting} waiting in a row (the network takes {LIMIT} unconfirmed at once). The rest was not sent"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> Hash256 {
        Hash256([n; 32])
    }

    /// The report of 24 Nov 2026: 18 transactions already waiting, so only 7 of a 47-piece
    /// inscription fit before the row reaches 25. Once that row confirms, the next batch is 25.
    #[test]
    fn seven_of_forty_seven_fit_behind_eighteen_waiting() {
        assert_eq!(batch_len(18, 47), 7);
        assert_eq!(projected(&[18]), 19);
        assert!(fits(projected(&[18])));
        assert!(!fits(projected(&[25])));
        assert_eq!(batch_len(25, 40), 0, "a full row takes nothing more");
        assert_eq!(
            batch_len(0, 47),
            LIMIT,
            "a confirmed coin starts a fresh row of 25"
        );
        assert_eq!(batch_len(24, 47), 1);
        assert_eq!(batch_len(0, 3), 3);
    }

    #[test]
    fn a_batch_never_passes_the_limit() {
        for tip in 0..=40 {
            for left in 0..=60 {
                let n = batch_len(tip, left);
                assert!(n <= left);
                if n > 0 {
                    assert!(tip + n <= LIMIT, "tip {tip} plus {n} stands past {LIMIT}");
                } else {
                    assert!(tip >= LIMIT || left == 0);
                }
            }
        }
    }

    #[test]
    fn a_linear_chain_counts_every_waiting_ancestor() {
        // 1 spends nothing waiting, 2 spends 1, ... 18 spends 17.
        let parents = |h: &Hash256| -> Option<Vec<Hash256>> {
            let n = h.0[0];
            if n == 0 || n > 18 {
                return None;
            }
            if n == 1 {
                Some(vec![])
            } else {
                Some(vec![id(n - 1)])
            }
        };
        assert_eq!(ancestor_count(&[id(18)], parents), 18);
        assert_eq!(
            ancestor_count(&[id(0)], parents),
            0,
            "a confirmed coin is not waiting"
        );
        assert_eq!(1 + ancestor_count(&[id(18)], parents), projected(&[18]));
        // Two inputs of the same waiting parent count once.
        assert_eq!(ancestor_count(&[id(5), id(5)], parents), 5);
    }

    /// Restarting must broadcast a signed piece again and build only what was never signed.
    #[test]
    fn resume_broadcasts_a_staged_piece_and_never_rebuilds_a_sent_one() {
        let sent: Vec<usize> = (0..7).collect();
        assert_eq!(next_piece(47, &sent, None), Next::Build(7));
        assert_eq!(
            next_piece(47, &sent, Some(7)),
            Next::Broadcast(7),
            "the signed bytes go out again"
        );
        let sent: Vec<usize> = (0..8).collect();
        assert_eq!(
            next_piece(47, &sent, Some(7)),
            Next::Build(8),
            "7 is already out, so it is not signed again"
        );
        assert_eq!(next_piece(8, &(0..8).collect::<Vec<_>>(), None), Next::Done);
        assert_eq!(
            next_piece(47, &[0, 1, 2, 5], None),
            Next::Build(3),
            "a gap is filled before the later pieces"
        );
    }

    #[test]
    fn the_waiting_note_says_how_many_and_that_it_continues() {
        let n = note(7, 47, 25);
        assert!(n.contains("7 of 47 sent"), "{n}");
        assert!(n.contains("25 waiting"), "{n}");
        assert!(n.contains("automatically"), "{n}");
        assert!(note(0, 47, 25).contains("automatically"));
        assert!(is_refusal("too many waiting transactions in a row"));
        assert!(is_refusal(
            "too-long-mempool-chain: more than 25 waiting ancestors"
        ));
        assert!(!is_refusal("not enough coins"));
        let held = held_note(7, 47, 25);
        assert!(held.contains("was not sent"));
        assert!(!held.contains("automatically"));
    }
}
