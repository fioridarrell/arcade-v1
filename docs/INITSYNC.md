# Init installs: store first, build in bulk

Status: **built on `team/init-sync`, every chain with a checkpoint on `team/all-chains`; the default where the chain allows it on `team/init-default` (2026-11-23). Not yet run to the end against Bitcoin or Dogecoin mainnet.** 2026-11-22.

```
arcade chain add btc                   # init where the chain allows it (the default)
arcade chain add btc --init            # init or a refusal with the reason
arcade chain add btc --no-init         # a plain install: blocks connected one after the other
{"cmd": "chains.add", "key": "btc"}                 # API: init where allowed
{"cmd": "chains.add", "key": "btc", "init": true}   # API: init or refuse
{"cmd": "chains.add", "key": "btc", "init": false}  # API: plain
```

**The default.** A new install that doesn't say (chains.add with no `init`, the app's Install
button, the startup chooser's queue) is an init install when `check` allows the chain and mode
(the table below; mode full or chain), and a plain one otherwise. chains.add's answer says which:
`init_install` true or false, and `init_why_not` with the reason when the default couldn't use it.
An installed chain, the home chain and the ARCADE chain keep their own installs. An operator turns
the default off with `ARCADE_INIT_DEFAULT=0` in arcaded's environment.

**Which chains:** those with an **init checkpoint** in the release (`ChainParams::init_checkpoint`),
no proof of stake and no MWEB. `chains.add` with init refuses the others with the reason (by default they install plain):

| Chain | Init | Checkpoint (height) | Sources | `min_chain_work` |
|---|---|---|---|---|
| btc | yes | 691,719 | Core 22.0 `defaultAssumeValid`, two explorers | Core 22.0 |
| pep | yes | 1,300,000 | pepecoind on node 2 and node 3; pepeblocks.com, pepeplorer.com | Pepecoin Core (block 327,239) |
| peptest | yes | 1,570,000 | node 2's arcade; pepecoind testnet on node 2 and node 3; testnet.pepeblocks.com | Pepecoin Core testnet (87,222) |
| doge | yes | 4,800,000 | node 2's arcade (Dogecoin headers); blockchair.com, blockcypher.com | Dogecoin Core (5,050,000) |
| lky | yes | 1,180,000 | luckycoind on node 2; luckyscan.org | Luckycoin Core (620,035) |
| flop | yes | 930,000 | flopcoind on node 3; explorer.flopcoin.net | Flopcoin Core (111,006) |
| dgb | no | — | two explorers agree (24,580,000), but none of our reachable nodes holds DigiByte mainnet | Core has none (0) |
| dgbtest | no | — | only node 2's arcade; no public explorer or Core checkpoint for testnet26 | Core has none |
| ltc, ltctest, jkc | no | — | MWEB: the extension block can't be checked yet (below) | |
| kot | no | — | proof of stake after 1,000,000 (below) | |
| shic, bel, wjk | no | — | no chain data of ours to check a recent hash against (Bells' node is stopped) | |

The checkpoint is the **trust floor**. `min_chain_work` alone is weak on the small merge-mined
chains: a big Scrypt pool makes their work cheaply, so a few sources could feed a made-up chain
with enough of it. A made-up chain can't run through a block the release names. Headers at the
checkpoint's height must be that block (as with any checkpoint), no branch may start under it,
and the headers phase is done only once the best header chain runs through it. Unlike the older
`checkpoints`, it is **not** an assume-valid point: scripts under it are still checked
(`ChainParams::assume_valid` reads only `checkpoints`). Each hash comes from at least two
independent sources: our own nodes' chain data (read only) plus the coin's Core source or a
public explorer. A chain where that can't be done stays refused, with the reason given. Where the
coin's Core has an `nMinimumChainWork`, it is set too.

A proof-of-stake chain (Kotia after block 1,000,000) stays refused: its stake (kernel hash,
coin age, block signature) is checked only when a block is connected, which an init install never
does for the blocks it builds, and those checks are not small enough to add to the stored-block
checks. MWEB chains (Litecoin, its testnet, Junkcoin) stay refused: the bytes after a block's
transactions are stored and served as part of the block, but nothing checks them. Their header
could be checked against the HogEx output, but not their body: its roots are MMR roots over every
MWEB output and kernel ever made, which needs the whole MWEB state.

**AuxPoW chains** (doge, pep, peptest, flop, shic, bel, lky, jkc): the parent's proof of work is
hashed with the chain's own algorithm from its params (`params::auxpow_parent_algo`), in one
check shared by the header sync, the stored-block checks and borrowing (`check_auxpow_proof`).
AuxPoW headers are large (the parent's coinbase and two merkle branches), so a source may send at
most 16 KiB of headers per new header it brings in a round, plus one full message (32 MiB); past
that it is left out of the round.

**DigiByte** (when it gets a checkpoint): multi-algo headers are checked as in a normal sync.
Headers of the algorithms not implemented here (Groestl, Qubit, Odocrypt) count the minimum work,
so DigiByte Core's chainwork (a geometric mean over all algorithms) can't be compared with ours,
and DigiByte gets no `min_chain_work`: the checkpoint is its floor. The low-work-branch check keeps
the best chain's header at every 1,000th height instead of every height's work (32 bytes a
height, ~0.8 GB on DigiByte's 24.5 M); the list of blocks still to fetch is still one entry per
height (~0.9 GB at the start of a DigiByte install).

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
| **headers** | Headers from up to 8 P2P peers (one per /16 or IPv6 /32) and the arcades offering the chain over bk. Each source is asked with a locator along our *best header* chain (the tip is still genesis). Then each is asked what follows our best header's *parent*. | Every header: link, proof of work, difficulty step, median time, future time (the normal header checks). Done only when **3 independent sources** answer with our best header itself, **and** its total work is at least the chain's `min_chain_work`. An empty answer never counts, nor a source on another tip, behind us, failing, or sending a bad header. Sources are keyed by network group (/16, IPv6 /32), P2P peers and arcades alike (an arcade by its address, resolved): one host, or one range, counts once however many ports or arcade ids it shows. **And the best header chain runs through the release's init checkpoint.** **Header spam:** a source that sends more than 20,000 headers in a round on a branch with less work than ours at the same height (or, past our best header, less work than it), or more than 16 KiB of headers per new header it brings (plus one full message), is left out of the round; no branch may start under the init checkpoint. |
| **blocks** | Every block on the best header chain not stored yet, fetched from every source at once (arcades + 24 P2P peers, as psync), **in any order**. Each worker checks and stores its own job (`ChainDb::init_store`). Nothing is connected, no index is touched. Then **headers again**: the two repeat until a headers round finds nothing new and no header lacks its block. | The block is the one asked for (hash), rebuilds byte-identically, coinbase first, merkle root = header's, no mutated tree (CVE-2012-2459), **no bytes after its transactions** (except Litecoin's MWEB block), **its own AuxPoW proves the work** (merge-mined chains: it is the copy kept and served), and **where SegWit is on at its height, the witness commitment matches** (a copy with witnesses stripped has the same txids and merkle root, so only this catches it). A refused or missing block goes back in the queue for a source that hasn't failed it yet; a source that refuses, leaves out or fails three batches in a row, or has failed every job left, steps aside. A round with no source working for 6 minutes ends and starts again. |
| **build** | (1) the stored blocks join the main chain (HEIGHT rows); (2) the tx index in one sorted pass (`write_txs_for`); (3) the coin set derived from the blocks (`Replay` in memory up to `ARCADE_INIT_COINS_MB`, else `DiskReplay`), **while** `rules::replay` checks every block's rules and scripts on every core; (4) only once both passed, the coin set is written (`apply_coin_diff`); (5) the tip moves to *best − 288*. | Every input spends a coin made before it (an earlier block, or an earlier transaction of its own block) and not spent before (else the build stops); the chain's coin and block rules (`value_rules`, `block_rules`: subsidy, fees, maturity, sizes, sigops, BIP34, lock times, versions, scripts with the flags of their height above the newest checkpoint, or all with `ARCADE_VERIFY_ALL_SCRIPTS=1`; Bitcoin's one checkpoint is a header fence only, so all). A block the rules refuse is **dropped and fetched again once** (its body may be a source's damaged copy): its rows go, the phase goes back to blocks, and the build starts over from step 1 (STATE `init_refetched` lists them). Refused again, it is the chain's own: the build stops. |
| **done** | The normal sync connects the last 288 blocks (with undo records, so the tip can reorg as usual) and follows. The address index and the protocol indexes are built from the stored blocks by their existing backfill paths (STATE `addr_backfill` from 1, the index walks). | As any sync. |

Each build step is recorded in STATE `init_built` once done (`main`, `txs`, `coins`, `rules`); the
rules check also records how far it got (`init_rules_to`). A build stopped half way redoes only
the step it was in (scratch from an earlier run is removed at the start: sorts and the coin
derivation start over). A build that fails on something a retry can't fix (a block refused again
after it was fetched again, a spend of a coin that never existed) stays in `build` with the reason
in `init_note` until the arcade restarts; any other failure (the disk, a read) is tried again
after 10 minutes. Nothing is half-moved: no coin is written before the rules passed, and the tip
stays at genesis.

**Scratch space.** The build's scratch is a folder beside the chain's own (`<chain>.initsync`), so
on the drive the chain was put on, not the arcade's home drive. Before the build it checks there
is room for the steps left (the tx index sort ~6% of the chain file, the coin set derived on disk
~55%, unless `ARCADE_INIT_COINS_MB` holds it in memory: over 1/32 of the chain file) and waits,
saying how much it needs, until there is. Removing the chain removes the scratch; a chain in an
init install can't be moved to another drive until it is built. `ARCADE_INIT_COINS_MB` is taken
as at most three quarters of the machine's memory (MemTotal; a cgroup's MemoryMax is not read)
and at most 1 TB.

**Removing or stopping.** Store-only workers write nothing once their round is stopped, the build
checks for a stop between its steps and before the tip moves, and `chains.remove` waits (up to
half an hour) for the init phases to leave before it deletes the chain's file.

## Until it is built the chain is not used

- Its coins and addresses are not answered: `ChainDb::coin`, `coins_for_script`,
  `balance_for_script`, `history_for_script` and `spender` return an error while STATE `init` is
  not `done` (so balances, sends, claims, swaps and the rest refuse rather than read a coin set
  whose rules aren't checked yet); the Explorer answers nothing from the chain and the RPC
  (`call_on`, also what borrowers ask) answers -28 "in an init install".

- Its tip stays at genesis: no coins, so no balances from it; `far_behind` is true, so the
  arcade borrows the chain (as any chain catching up).
- It is not lent (coreshare `lent`), not announced in bk `hi`, and bk `hq`/`bq` answer "not held".
- No copies: `chains.copy_start`, `copy_from_dir` and `share.install` refuse while it runs, and
  `v1copy::checkpoint` only runs in the normal loop after it.
- The sync watchdog counts it as held (no stall alarms while it stores).

## Timing

Every phase writes a timing line, kept with the chain (STATE `init_timing`, the last 40, in
`chains.status` as `init_timing` and per row as `timing`) and in the journal ("init timing: ..."):

```
headers round 1: 925012 new headers to 925012 in 9m 12s · 1675.4 headers/s
blocks round 1: 925012 blocks, 721034.2 MB stored in 3h 10m 04s · 81.1 blocks/s · 63.2 MB/s
build main: 924724 heights joined in 41.2s
build txs: 1201234567 tx index rows in 58m 10s · 344123.0 rows/s
build coins: 924724 blocks derived on disk in 1d ... · 9.8 blocks/s
build rules: 924724 blocks checked (heights 1..924724) in ... · ... blocks/s
build coins written: 183000000 coins in ... · ... coins/s
build: to block 924724 in ...: ...
```

(Illustrative numbers.) Same lines on every run, so runs on different machines compare. While a
blocks round runs, a `blocks round in progress: ...` line (blocks, MB, blocks/s, MB/s so far) is
rewritten every 3 minutes, so a stop, a restart or an error never loses the measurement.

## Block numbers

Blocks arrive in any order, but each takes the block number of its height: STATE `init_b_base`
is fixed at the first block stored (the next free number, under it) and a block at height h is
stored as base + h; the block-number counter moves past every number handed out. So the main
chain's block numbers run in height order, as after a block-by-block sync (the compressed index
files need that). If the best header chain moves between rounds, a stored block on the old
branch holds its height's number: its body is dropped (header kept) and the new branch's block
takes the number.

## Memory in the blocks phase

What a round holds is bounded, however the sources behave:

- **Jobs are sized by bytes**, not a fixed count: a job is cut, when a worker takes it, to about
  48 MB of blocks at the size seen so far (the larger of the running average and the last job's;
  8 blocks while nothing is known yet, at most 250).
- **Each job has a byte limit** (four times its expected size, 64 MB to 256 MB; 256 MB while no
  size is known). A P2P peer stops being read there and is dropped; the job goes back in halves
  (nobody blamed: the blocks may just be bigger than the ones before) and the size estimate rises.
- **One budget for every job in flight**: their limits together stay under 6 GB (one job always
  goes out). With every source padding every block, that is what fetched blocks can take.
- **P2P blocks are checked as they arrive**: a `block` message larger than the chain's largest
  possible block (`ChainParams::max_block_bytes`: Bitcoin 4 MB + 1 MB of slack) is refused
  before it is read, and a block whose header is not one asked for (or comes twice) drops the
  peer at once.
- `init_store` parses and writes in batches of 16 MB, letting each raw block go as it is parsed:
  the parsed copies a worker holds stay small.

## Room on the drive

The blocks phase holds while the chain's drive has less free than `ARCADE_INIT_FREE_GB` (default
50) plus 12 GB for the next batches, and says so in the chain's status; it carries on by itself
once there is room again (checked before each round and every 30 seconds during one).

## Smoke test: stop the store phase at a height (testing only)

`ARCADE_INIT_UPTO=<height>` in the arcade's environment: the install logs "init: storing up to
<height>" when it starts. A value that isn't a block height (a whole number over 0, so not empty
either) stops the install with that reason in its status; it never runs uncapped by mistake. The
headers phase runs as usual, the
blocks phase stores only blocks up to that height, then the install holds there ("stopped at
block N (ARCADE_INIT_UPTO, testing only)" in its status, with the last timing line); the build
never starts and the chain stays unusable. Meant to measure download and store speed on a real
chain (Bitcoin) with limited disk; remove the chain afterwards. Not for real installs.

## What is not checked, and when

- During **headers**: nothing about blocks. Agreement is about our best header only; a peer
  set controlled by one party in three network groups could agree on a chain, but it must also
  run through the release's init checkpoint and carry `min_chain_work` of real proof of work
  where the chain has one (which is why chains without a checkpoint can't use init).
- During **blocks**: no coin or script rule. A block whose body is the one its header commits
  to is stored even if it breaks a rule; the build finds it.
- `min_chain_work` is each coin's Core `nMinimumChainWork` where it has one (Bitcoin Core 22.0's,
  block 691,719, deliberately old: it is still far more work than anyone can make up, and it never
  holds a real install back). DigiByte's Core has none, and ours would not compare (above).
- The 288 blocks under the init tip have no undo records: a reorg deeper than that stops the
  sync (ReorgTooDeep), as after a coin-set copy.
- Chains without `value_rules`/`block_rules` get no rules pass (blocks checked against their
  headers and the coin derivation only), as their normal sync.
- BIP30's duplicate coinbases (Bitcoin 91,812/91,842 and 91,722/91,880) stay as two coins each,
  as in a normal sync; the tx index points such a txid at the copy stored last, which in an init
  install is arrival order, not height.

## Real-data runs (2026-11-22, the mini, throwaway arcade, `chain add <key> --init`)

| Chain | Headers | Blocks | Build | Result |
|---|---|---|---|---|
| pep (1,303,664 built) | ~22 min, 8 P2P sources, Core's work reached, through 1,300,000 | ~25 min, 23 sources | 8 min 13 s (coins 6 m 23 s, rules 5 m 29 s on every block) | tip 1,303,963 `172dff89…`: **20,432,802 coins, 105,476,635,900.49606538 PEP, the same as Pepecoin Core's `gettxoutsetinfo` on node 2 at that block** |
| lky (to best − 288) | ~15 min, through 1,180,000 | ~12 min, 16 sources | under 3 min | tip 1,191,761 `68328ddf…`: **339,201 coins, 19,990,625.40279840 LKY, the same as Luckycoin Core on node 2** |
| flop (939,354 built) | ~12 min, 6 P2P sources, through 930,000 | ~10 min, then 8 short rounds chasing one-minute blocks | 43 s | tip 939,649 `66bbe496…`: **442,220 coins, 101,832,005,807 FLOP, the same as Flopcoin Core on node 3** |

No source was left out for header volume on any of them (all three are merge-mined).
Pepecoin testnet can't be tried this way: it is every arcade's home chain, installed by its own
path (not `chains.add`), so `--init` answers that it is installed already. A new arcade's
borrowed home chain held every install in the queue (fixed on this branch).

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
