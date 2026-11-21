# Init installs: store first, build in bulk

Status: **built on `team/init-sync`, opt-in, not yet run against Bitcoin mainnet.** 2026-11-20.

```
arcade chain add btc --init            # CLI
{"cmd": "chains.add", "key": "btc", "init": true}   # API
```

Without `--init` nothing changes: blocks are connected one after the other as before.

## Why

The normal sync downloads in parallel (`psync`) but hands every block, in height order, to one
writer that spends coins, checks the rules and runs every index in the same commit
(`connect_one`). On Bitcoin that writer is the whole story: the first sync took days. An init
install splits the work into passes that each do one thing over the whole chain, most of them in
parallel or as sorted bulk writes.

## Phases

STATE `init` (in the chain's own store) holds the phase. Every phase carries on where it was after
a restart. `arcade chain status <key>` shows a row per phase (`Init · headers`, `Init · blocks`,
`Init · build`) and the status JSON has `"init": "<phase>"`.

| Phase | What happens | Checked here |
|---|---|---|
| **headers** | Headers from up to 8 P2P peers (one per /16 or IPv6 /32) and the arcades offering the chain over bk. Each source is asked with a locator along our *best header* chain (the tip is still genesis). Then each is asked what follows our best header. | Every header: link, proof of work, difficulty step, median time, future time (the normal header checks). Done only when **3 independent sources** have our best header as their tip or extend it, **and** its total work is at least the chain's `min_chain_work`. A source on another tip, behind us, failing, or sending a bad header doesn't count; two sources in one network group count once. |
| **blocks** | Every block on the best header chain not stored yet, fetched from every source at once (arcades + 24 P2P peers, as psync), **in any order**. Each worker checks and stores its own job (`ChainDb::init_store`). Nothing is connected, no index is touched. Then **headers again**: the two repeat until a headers round finds nothing new and no header lacks its block. | The block is the one asked for (hash), rebuilds byte-identically, coinbase first, merkle root = header's, no mutated tree (CVE-2012-2459), and **where SegWit is on at its height, the witness commitment matches** (a copy with witnesses stripped has the same txids and merkle root, so only this catches it). A refused block goes back in the queue for another source; a source that sends three bad batches steps aside. |
| **build** | (1) the stored blocks join the main chain (HEIGHT rows); (2) the tx index in one sorted pass (`write_txs_for`); (3) the coin set derived from the blocks (`Replay` in memory up to `ARCADE_INIT_COINS_MB`, else `DiskReplay`), written with `apply_coin_diff`, **while** `rules::replay` checks every block's rules and scripts on every core; (4) the tip moves to *best − 288*. | Every input spends a coin made earlier and not spent before (else the build stops); the chain's coin and block rules (`value_rules`, `block_rules`: subsidy, fees, maturity, sizes, sigops, BIP34, lock times, versions, every script with the flags of its height) where the chain has them. |
| **done** | The normal sync connects the last 288 blocks (with undo records, so the tip can reorg as usual) and follows. The address index and the protocol indexes are built from the stored blocks by their existing backfill paths (STATE `addr_backfill` from 1, the index walks). | As any sync. |

Each build step is recorded in STATE `init_built` once done (`main`, `txs`, `coins`, `rules`); the
rules check also records how far it got (`init_rules_to`). A build stopped half way redoes only
the step it was in. A build that fails on something a retry can't fix (a rule broken, a spend of
a coin that never existed) stays in `build` with the reason in `init_note` until the arcade
restarts; nothing is half-moved, the tip stays at genesis.

## Until it is built the chain is not used

- Its tip stays at genesis: no coins, so no balances from it; `far_behind` is true, so the
  arcade borrows the chain (as any chain catching up).
- It is not lent (coreshare `lent`), not announced in bk `hi`, and bk `hq`/`bq` answer "not held".
- No copies: `chains.copy_start`, `copy_from_dir` and `share.install` refuse while it runs, and
  `v1copy::checkpoint` only runs in the normal loop after it.
- The sync watchdog counts it as held (no stall alarms while it stores).

## What is not checked, and when

- During **headers**: nothing about blocks. Agreement is about our best header only; a peer
  set controlled by one party in three network groups could agree on a chain, but it must also
  carry `min_chain_work` of real proof of work.
- During **blocks**: no coin or script rule. A block whose body is the one its header commits
  to is stored even if it breaks a rule; the build finds it.
- `min_chain_work` is empty (no floor) for every chain but Bitcoin. Bitcoin's is Bitcoin Core
  22.0's `nMinimumChainWork` (block 691,719), deliberately old: it is still far more work than
  anyone can make up, and it never holds a real install back.
- The 288 blocks under the init tip have no undo records: a reorg deeper than that stops the
  sync (ReorgTooDeep), as after a coin-set copy.
- Chains without `value_rules`/`block_rules` get no rules pass (blocks checked against their
  headers and the coin derivation only), as their normal sync.

## Speed (Bitcoin, ~925k blocks, ~720 GB of blocks, ~1.2 billion transactions, ~3 billion inputs)

What is parallel: block download and the per-block checks (every worker), the rules and script
check (`rules::replay`, all cores, each spent coin read as one transaction row, not its block).
What is serial: the header download (minutes), the store's single writer (bulk appends), the tx
index read pass (one thread hashing every transaction), the coin derivation (one thread, one
tx-index lookup per input), the address build afterwards (its own sorted passes, ADDRBUILD.md).

Rough estimates, not measured on mainnet yet:

| | 32 cores, NVMe, 64 GB+ RAM, 1 Gb/s | 4 cores, HDD, 16 GB RAM, 1 Gb/s |
|---|---|---|
| headers | ~10 min | ~10 min |
| blocks (network-bound, ~60–110 MB/s from 24 peers) | 2–4 h | 3–6 h (HDD writes) |
| tx index (read 720 GB + sort 1.2 G rows) | ~1 h | 3–5 h |
| coin set (`ARCADE_INIT_COINS_MB=24000`: in memory) | 2–3 h, alongside the rules | on disk (~350 GB scratch), random tx-index lookups: 1–3 days |
| rules + scripts (all cores) | 4–8 h (the longest step) | 2–4 days |
| **total to the tip** | **~8–14 h** | **~4–7 days** |
| today (per-block connect + indexes) | days (the first Bitcoin sync) | weeks |

Then the address index builds in the background as after any catch-up.

The HDD box is limited by random reads: one tx-index lookup per input in the coin derivation
and in the rules check. The next step to make those sequential is a sorted join of all inputs
against the tx index (positions resolved in one merge pass) instead of a lookup per input; not
built.

## Code

- `crates/arcade-chaindb/src/initsync.rs`: phases, headers round and agreement, `init_store`,
  `init_fetch_blocks` (one source at a time, for tests), `init_build`. Tests in `initsync_tests.rs`.
- `crates/arcade-chaindb/src/coreimport.rs`: `check_stored_block` (now with the witness
  commitment check), `write_stored`, `write_txs_for`, `Replay`, `DiskReplay`, `apply_coin_diff`.
- `crates/arcade-chaindb/src/rules.rs`: `replay` (spent coins read as one transaction each).
- `crates/arcaded/src/initsync.rs`: the phases in the chain thread, status rows.
- `crates/arcaded/src/psync.rs`: `run_store` (store-only, any order).
- `crates/arcade-chain/src/params.rs`: `ChainParams::min_chain_work`.
