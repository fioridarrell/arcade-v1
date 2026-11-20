# Consensus harness: V1 against a real Pepecoin Core

`regnet.py` runs Pepecoin Core in `-regtest` and gives V1 the same blocks. Core mines the good
ones. The script builds the rest by hand: bad blocks and competing branches, with real scrypt
proof of work and real AuxPoW. Every block goes to Core (`submitblock`) and to V1. V1 reads them
through `examples/regtest_follow`, which runs the real `Syncer` with every check it makes on a
live chain. After each scenario, Core's best block and V1's tip must be the same block.

## Running it

```sh
cargo build --release -p arcade-chaindb --example regtest_follow
unshare -Urn sh -c 'ip link set lo up; python3 tests/regtest/regnet.py'
```

- **What it needs:** `pepecoind` (Pepecoin Core 1.1.0) on `PATH` or in `~/.local/bin`, and
  Python 3 with nothing extra installed.
- **Other builds:** `--follow PATH` picks another `regtest_follow` build (for example, one from
  an older commit, to see what a change fixed).
- **Keeping the files:** `--keep` keeps the work directory (Core's data, V1's store and the
  block file).
- **Exit code:** nonzero when a scenario that must match doesn't.

## Scenarios

| Scenario | Core | V1 must |
|---|---|---|
| Core mines blocks, before and after merge mining starts (height 20) | follows | follow |
| A merge-mined block before height 20 | early-auxpow-block | refuse |
| A version-1 (legacy) block after height 20 | late-legacy-block | refuse |
| A merge-mined block carrying chain id 64 instead of 63 | refuses (strict chain id) | refuse |
| A hand-built merge-mined block | takes it | take it |
| Witness data on a chain without SegWit | unexpected-witness | refuse |
| A heavier branch whose 2nd block spends a coin that doesn't exist | marks it invalid, stays | mark it, stay |
| A valid heavier branch | reorgs | reorg |
| A coinbase paying 10,000,000 PEP; the subsidy plus the fee plus 1 unit | bad-cb-amount | refuse |
| The subsidy plus exactly the fee | takes it | take it |
| Two coinbases; a 1-byte coinbase script | bad-cb-multiple; bad-cb-length | refuse |
| Spending a coinbase 59 blocks later; then at 60 | premature-spend; then takes it | the same |
| Paying out 1,001 PEP from a 1,000 PEP coin | bad-txns-in-belowout | refuse |
| A mutated copy of a block, then the real block | refuses the copy, takes the block | the same |
| An unsigned spend of a matured coin that needs a signature | script check fails | refuse |
| A correctly signed spend; one signed by the wrong key | takes it; script check fails | the same |
| A transaction locked 500 blocks ahead | bad-txns-nonfinal | refuse |
| 20,001 signature operations; a block over 1,000,000 bytes | bad-blk-sigops; bad-blk-length | refuse |
| After 1,351: a version-3 block | bad-version | refuse |
| After 1,251: a signature that isn't strict DER | script check fails (DERSIG) | refuse |
| After 1,351: a CHECKLOCKTIMEVERIFY coin with no lock time; then with lock time 1,000 | fails; then takes it | the same |

The block builder's own coins pay to `OP_TRUE` (spent with no signature, to try fees, maturity
and amounts exactly) or to a key of its own (signed with Python's `cryptography` package,
legacy SIGHASH_ALL). The run mines to height 1,360, so DERSIG (1,251), CHECKLOCKTIMEVERIFY and
version 4 (1,351) are in force for the last scenarios.

A scenario can be marked `gap=True` for a rule V1 doesn't check yet. The run reports it
without failing. Since step 3 there are none. After each bad block, Core mines two more
blocks, and V1 must follow Core's chain.

On `main` before step 1, the first 16 scenarios gave 6 match, 3 gaps and 7 FAIL. With steps 1
to 3, all 38 match: no gaps.

# The SegWit-era harness: V1 against a real Litecoin Core

`ltcnet.py` does the same against Litecoin Core 0.21 in `-regtest`, for the rules Bitcoin Core
0.13 and later chains add. V1 follows with `regtest_follow ... ltcregtest`.

```sh
cargo build --release -p arcade-chaindb --example regtest_follow
unshare -Urn sh -c 'ip link set lo up; python3 tests/regtest/ltcnet.py'
```

- **What it needs:** `litecoind` (0.21.x) on `PATH` or in `~/.local/bin`, and Python 3 with the
  `cryptography` package. Schnorr signatures (BIP340) are made by a small pure-Python secp256k1.
- **Options:** `--litecoind PATH`, `--follow PATH`, `--keep`, `--only a|b`.
- **Two runs:** A keeps MWEB off (`-vbparams`) and gives both hand-built good and bad blocks. B
  lets MWEB activate and has Core's wallet peg in, move coins inside MWEB and peg out. Once MWEB
  is on, every block needs a real extension block, so bad blocks are built in run A only.
- **Each scenario states Core's verdict** ("both take it", or "Core: <reason>"). If Core's
  answer differs, the scenario is marked BUILD and fails the run: the block wasn't what the
  scenario meant, so it proves nothing about V1.

| Run A scenario | Core | V1 must |
|---|---|---|
| A version-4 block once SegWit is on | bad-version (whole nVersion under 0x20000000) | refuse |
| P2WSH OP_TRUE spend; signed P2WSH spend (BIP143) | takes them | take them |
| Witness spend with no commitment; wrong commitment with and without witness data | unexpected-witness; bad-witness-merkle-match | refuse |
| Witness data on a non-witness spend | script check fails | refuse |
| P2WSH signed over the wrong amount | script check fails | refuse |
| Witness CHECKMULTISIG with a non-empty dummy; then an empty one | NULLDUMMY fails; takes it | the same |
| Taproot key-path spend with a broken signature; then a good one | fails; takes it | the same |
| 90,000 witness signature operations | bad-blk-sigops | refuse |
| A block of weight over 4,000,000 | bad-blk-weight | refuse |
| Version-2 spend with a 10-block relative lock, 5 and then 10 blocks in | bad-txns-nonfinal; takes it | the same |
| The same sequence on a version-1 transaction | takes it | take it |
| 512-second relative lock, at once and then later | bad-txns-nonfinal; takes it | the same |
| Lock time between the median time past and the block's time (BIP113) | bad-txns-nonfinal | refuse |
| `<10> CHECKSEQUENCEVERIFY` spent with sequence 5, then 10 | fails; takes it | the same |
| Coinbase one unit over a halved subsidy; then exact | bad-cb-amount; takes it | the same |

Run B: the first MWEB block (with a peg-in), a transfer inside MWEB (its fee leaves through
the HogEx), a peg-out, its spend 6 blocks later, and five more mixed blocks. V1 must follow
every one. MWEB's own rules (kernels, range proofs, the peg-in and peg-out matching) aren't
checked by V1.

Run A: 26 match. Run B: 5 match.

## Borrowed until installed (`borrownet.py`)

A closed local ARCADE chain with three arcades: A writes it, C never installs it, and D installs it but takes no
blocks for 240 s (`ARCADE_ARC_TEST_SYNC_AFTER`). C and D borrow it from A, see their ARCADE, send it, claim a @tag on
ARCADE and post; D then hands over to its own copy once that has caught up, keeping its balance and handle.

```sh
cargo build -p arcaded
unshare -Urn sh -c 'ip link set lo up; python3 tests/regtest/borrownet.py target/debug/arcaded /tmp/borrownet'
```

It prints `RESULT: PASS` (exit 0) or the failed steps. It takes about 10 minutes.

## Shared system info and ping (`sysinfonet.py`)

Three arcades on a closed network, B linked to A and C. A shares its system info (`sysinfo.set`), C doesn't. B's
`mesh.sysinfo` shows A's signed record and counts 1 of 2 sharing; C gets it too, passed on by B. B's `mesh.overview`
has a `ping_ms` for both (`ping_via: "link"`) and a `machine` for A only. The switch survives a restart of A; when A
switches off, B and C drop its machine at once (about a second).

```sh
cargo build -p arcaded
unshare -Urn sh -c 'ip link set lo up; python3 tests/regtest/sysinfonet.py target/debug/arcaded /tmp/sysinfonet'
```

It prints `RESULT: PASS` (exit 0) or the failed steps. It takes about a minute.

## Nodes and sizes for the Storage app (`arcnodesnet.py`)

Three arcades on a closed network with a closed ARCADE chain: A writes it, B links to A and shares its system info,
C links to B only. A's `explorer.arc_nodes` lists itself, B (linked, with its machine line) and C (heard through B's
passed-on hold report), each V1.0 (not beta) with Pepecoin testnet kept in full and listed; the chain row counts the
three keepers; `/r/explorer/arc_nodes` and `/r/explorer/arc_machines` answer. Sizes stay unknown (nothing to measure
offline); the hold report's `sz` part is checked by `shares::tests::a_hold_carries_its_measured_size_and_passes_it_on`.

```sh
cargo build -p arcaded
unshare -Urn sh -c 'ip link set lo up; python3 tests/regtest/arcnodesnet.py target/debug/arcaded /tmp/arcnodesnet'
```

It prints `RESULT: PASS` (exit 0) or the failed steps. It takes a few minutes.
## Keep shares by machine and a lost keeper replaced (`keepernet.py`)

Six arcades on a closed ARCADE chain (rules version 5 from height 2, copy floor capped at 2 with
`ARCADE_FOG_FLOOR_MAX`), each with its own made-up disk budget (`ARCADE_DISK_BUDGET_TEST`): A publishes, T is tiny and
borrows everything, M is medium (a 3 MB share), L is a frontier machine (25% of its free space), X and Y are in between
(50 MB). It checks each machine's suggestion and that nothing is set by itself; T works and reads the fog map through
the mesh; the segments fill from whoever has room, the large machine first, M never taking the 6 MB segment; the
short list counts only registered, audited replicas; `explorer.arc_blobs` and `explorer.arc_fog_nodes` (roles, the
large blob as one item, the per-node panel, a blob's journey). Then L is killed for good and the next with room takes
its segments once its announcement is past the stale time (about a minute); the timeline is printed.

```sh
cargo build --release -p arcaded
unshare -Urn sh -c 'ip link set lo up; python3 tests/regtest/keepernet.py target/release/arcaded /tmp/keepernet'
```

It prints `RESULT: PASS` (exit 0) or the failed steps. It takes about 8 minutes.

## Keep, pin and forget (`fogkeepnet.py`)

Six arcades on a closed ARCADE chain (copy floor 2): W publishes, A is the operator under test, B and C keep, D is
linked to A alone, T borrows ARCADE. Registration and audits never happen on a closed net, so
`ARCADE_FOG_TEST_AUDIT_SECS` stands in for them (an announced keeper is audited after 25 s holding every blob). It
checks: Keep refused with the reason on a borrowed arcade and with no keep share; A keeps P#0 and T#0 by choice (the
card first) and is counted only after its first audit; C doesn't take a segment at its floor; A forgets P#0: C steps
in, A shows `handing over (n/2)` and drops only after C is counted; A forgets one blob of T#0 (which it keeps): it keeps
the bytes, refuses its own view, leaves it out of its bloom and answers D "not here", while C fetches it from B; undo:
D gets it from A again; T forgets a blob it viewed through its lenders. Last, the Storage app's Settings on A (as an
app page with its pass: A's node key is imported so the test can make it): `GET /r/fog/settings` shows arc.keep's
choice and presets and the cache size; `POST /r/fog/keep_share {mb: 80}` files a card (50.0 MB → 80.0 MB, what it would
hold, the disk left) and changes nothing until approved, then `arc.keep` says 80 MB; a cache card refused leaves
`arc.cache` as it was, one approved sets it.

```sh
cargo build --release -p arcaded
unshare -Urn sh -c 'ip link set lo up; python3 tests/regtest/fogkeepnet.py target/release/arcaded /tmp/fogkeepnet'
```

It prints `RESULT: PASS` (exit 0) or the failed steps. It takes about 7 minutes.
