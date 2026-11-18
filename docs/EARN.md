# Earning ARCADE: when and how a node is paid

For node operators. This page follows the code: `crates/arcade-arc/src/registry.rs` (pay, turns, audits),
`params.rs` (the numbers), `catalog.rs` (listing), `crates/arcaded/src/arcseg.rs` (sealing, offering, registration),
`arcaudit.rs` (answering audits) and `arcserve.rs` (serving checks). Where this page and the code disagree, the code
is right and this page has a bug. The design and its reasons are in [ARCADECHAIN.md](ARCADECHAIN.md).

ARCADE is paid for **keeping data sealed**: the blocks of the listed chains, the coins each block spends, and the index
journals. Holding ARCADE (or any other coin) earns nothing.

Blocks come every 150 seconds: 24 an hour, 576 a day, 1,152 (one **audit period**) every 2 days.

## In short

1. Earn ARCADE is on by default for every listed coin kept in full. The arcade seals that coin from block 0 up, a
   16 MiB segment at a time.
2. A sealed segment can register only once it is **listed** in the ARCADE chain's catalog, and the arcade registers
   **one batch an audit period**.
3. From the block that holds the batch's proof, the replicas have **pay weight**: each epoch (100 blocks) the pot is
   shared by weight and added to what this arcade has **accrued**.
4. Pay goes out **at this arcade's own turn**, one block in every 1,152, and only for a period whose **storage audits
   all passed**. The first payout comes **2 to 4 days** after registration: the
   first turn draws the audits, the next one pays.
5. Coinbase outputs can be spent **100 blocks** (about 4 hours) after the block that paid them.

## Where to see it

| What | Where |
|---|---|
| The switch, sealing progress, catalog notes, replicas waiting to register, turn and next payout | Chains & Coins, the coin's card, *Earn ARCADE* (`arc.earn {chain}`) |
| The same, read only, for the Explorer and apps | `explorer.earn {chain?}`: `/r/explorer/earn?args={"chain":"peptest"}` from an app page, `/api/v1/explorer/earn` from a program on this computer. Without a chain: this arcade's pay and every sealed replica not registered yet |
| Pay, audits, every owner's weight and accrued pay | `arc.status`: `pay` is this arcade's view, `owners` and `genesis` everyone's |
| Each sealed replica: registered, missed audits, verdicts | `arc.replicas` |
| The catalog: listed and waiting entries | `arc.catalog`; per chain `arc.sealing {chain}` |

`pay` (in `arc.status`, `arc.earn` and `explorer.earn`) holds:

- `turn`: this arcade's block in each audit period (`registry::turn`, `H(owner) mod 1,152`); `next_turn`: the next
  block with that number.
- `next_payout`: the earliest block this arcade can be paid in (empty while it depends on more pay accruing).
- `accrued`: pay not yet paid, in units (1 ARCADE = 100,000,000).
- `waits` and `why`: why the next turn won't pay, if it won't (below).
- `pay_weight`, `draw_weight`: see *Pay weight and draw weight*. `replicas_not_drawing` counts registered replicas not
  yet in the draw; `silenced_until` is set while the arcade's draw weight is taken away.
- `audits`: this period's picks (`at`, `committed`, `passed`, `missed`); `verdicts`, `held`, `held_times`: serving
  checks since the last turn.
- `genesis`: the genesis earners this arcade's key holds bytes of (on the testnet, the GX10 and the JB20B), each with
  its own turn and accrued pay.

`waits` is one of:

| `waits` | What happens at the next turn |
|---|---|
| (empty) | paid, in one output to the payout address |
| `audits_open` | paid if every pending audit passes before then |
| `no_audit` | nothing yet: the first audits are drawn at this turn, and pay goes out at the turn after, if they all pass |
| `audit_missed` | an audit wasn't answered: this period's accrued pay goes back to the pot |
| `held` | most of 5 or more serving verdicts say "not served": pay is held (the third time in a row, it goes back to the pot) |
| `under_min` | under 0.01 ARCADE: it waits for a later turn |
| `nothing` | nothing accrued yet |

## Where the pay comes from: the pot

- Every block mints **50 ARCADE** (`BLOCK_REWARD`). Its writer gets **1** in its coinbase (`WRITER_REWARD`, rules
  version 2 and up: the testnet from block 0 since its restart on 2026-11-15, the mainnet from block 0). The other
  **49** and **every fee** in the block go to the **pot**. Pay forfeited by a missed audit, a third hold or a key
  proved to have signed two blocks on one parent goes to the pot too.
- At each block whose height is a multiple of **100** (`EPOCH_BLOCKS`), the pot is shared by **pay weight** among
  every earner: the genesis earners (by payout address) and every owner (an arcade's key) with registered replicas. It
  is shared in whole units, the leftover units one each to the largest remainders. Each share is added to the earner's
  accrued pay and the pot starts over. (If nobody has weight, the pot carries over.)
- A replica's weight is its segment's bytes split among everyone registered for that segment: data few arcades keep
  pays more per arcade.

## When it's paid: each earner's turn

- Every earner has a **turn**, `H(earner) mod 1,152` (`registry::turn`): one block in every audit period. The coinbase
  of that block pays it everything it has accrued, in one output. Turns are spread over the period, so each coinbase
  pays about one 1,152th of the earners.
- The old "one coinbase per epoch pays everyone" design (first drafts of ARCADECHAIN.md §7) isn't what runs: on the
  testnet and on the mainnet, pay is accrued every epoch and paid at each earner's turn.
- **Genesis earners** are paid at their turn with no audit (their bytes were fixed at block 0); only the 0.01 minimum
  holds them back.
- **Owners** (arcades that registered replicas) are paid at their turn only if their audits allow it:
  - **No audit yet** (`audits_passed` None: the arcade registered after its last turn): nothing is paid; the pay
    waits for a period an audit covers. Seal, register and delete earns nothing.
  - **Every audit passed:** paid, unless serving checks hold it.
  - **Any audit not passed:** the period's accrued pay goes back to the pot.
- **Under 0.01 ARCADE** (`MIN_PAYOUT`) stays accrued for a later turn.
- A coinbase output is spendable **100 blocks** later (`COINBASE_MATURITY`).

## Storage audits

- At each owner's turn, the turn block's seed draws **4** of its replicas by bytes (`AUDIT_SAMPLE`; one drawn twice is
  audited once), each with a picking block 1 to 1,092 blocks later.
- For each pick the arcade posts a **commit** in the 20 blocks after the picking block (one read through the sealed
  copy), then an **opening** of 4 leaves in the 20 blocks after the block 10 past the commit. `arcaudit.rs` does this
  by itself while the arcade runs and the sealed copy is on its disk.
- At the next turn the period is decided: all passed releases the pay; any miss forfeits it. A replica that missed
  stops counting (no pay weight, no draw weight) until it passes again; the arcade asks for a **re-audit** at once
  (it then counts again, but that period's pay stays forfeited). **3 misses in a row** remove the replica
  (`MAX_MISSED_AUDITS`); it can register again.

## Serving checks: holds

- Arcades with draw weight that keep the same chain check each owner about **6 times a period** (`SERVE_CHECKS`): they
  fetch about 8 MiB of its plain blocks with the ordinary block fetch and post a verdict, served within 60 s or not.
  Missing verdicts cost nothing.
- At the owner's turn, with **5 or more verdicts** since its last turn (`HOLD_VERDICTS`), most "not served", its pay
  is **held**. The next turn that isn't held pays it. Held **3 turns in a row** (`HOLD_FORFEIT`), it goes back to the
  pot. The verdict count starts over at every turn.

## Pay weight and draw weight

Two weights, kept apart on purpose:

- **Pay weight** (`pay_weight`, `own_weight` in `arc.status`): every registered replica that hasn't missed its last
  audit, from the block holding its registration proof. The pot is shared by this.
- **Draw weight** (`draw_weight`): what writes blocks, offers and attests catalog entries, and makes serving checks.
  A replica counts **200 blocks after its first passed audit** (`DRAW_WEIGHT_WAIT`, longer than the 100-block reorg
  limit, so weight registered on a secret branch can't write there). Genesis bytes count from block 0. **Rules version
  4 (testnet only):** no wait: a replica counts from the block after its first passed audit (one still waiting when
  version 4 begins counts from then); `draw_weight_wait` in `arc.earn` shows 0. Mainnet keeps the 200 blocks.
- A key whose **reveal demand or transaction challenge is answered** (it challenged good data) has **no draw weight
  for one audit period** (`silenced_until`). Its pay weight is untouched. A key proved to have **signed two blocks on
  one parent** loses everything: replicas removed, accrued pay to the pot, no weight ever again.

A new arcade therefore has pay weight but **draw weight 0** until its first audit passes and 200 more blocks go by. Until
then it can't post or attest catalog entries: it depends on arcades with draw weight to list what it keeps.

## What can earn

### Chains

Only the network's **listed** chains (`params.rs`, `Net::listed_at`: `listed`, plus `listed_v4` from the rules version 4
height; adding one is a rules version):

| Network | Listed |
|---|---|
| Testnet (`arctest`) | arctest, pep, peptest, doge, dgb, dgbtest, flop, wjk, kot, ltc, ltctest, btc; **from rules version 4** also shic, bel, lky, jkc |
| Mainnet (`arc`) | arc, pep, peptest, doge, dgb, flop, wjk, kot, ltc, btc |

Shibacoin (shic), Bellscoin (bel), Luckycoin (lky) and Junkcoin (jkc) are paid on the **testnet from the rules version 4
height** (issue #6): before it their Earn ARCADE line says "not paid on this network" instead of a switch that reads on,
and at it the line flips (it reads the next block's height), segments of them are offered, sealed and can earn. On
mainnet they are not paid. The ARCADE chain itself is always sealed while it runs here; it has no switch.

### Families

A coin kept in full is cut into these families (`arcseg::families`):

| Family | Number | Notes |
|---|---|---|
| blocks | 0 | always |
| coins (what each block spends) | 9 | always, made from the blocks |
| ord, ledger, profiles, messages, names, tokens, maps, tags | 1 to 8 | only when the index's journals run here **from the index's start**: an index taken in as a copy isn't sealed or offered until it is rebuilt |
| blobs | 10 | the ARCADE chain only |
| dogefeed (D://, Dogecoin only) | 12 | **rules version 4 (testnet) only**, from its activation height; journals from the index's start, as above |

Indexes with **no journal family** are never sealed and never paid: **addresses, collections** (runes and dunes when
they come). `arc.earn` lists them under `unpaid_indexes`, and dogefeed too where version 4 doesn't apply (mainnet, or
the testnet below the version 4 height).

**Rules version 4 (testnet):** the dogefeed index journals each block as family 12 and is sealed, listed and paid like
the other index families. Its journals run from the index's start only when it was built here from block 6,300,000
with this release or later: an arcade that already had the index (it has no journals for the heights before) must
remove and rebuild it to earn from it. Collections keeps number 11 but stays unpaid: it is read by a background pass
outside the block's commit, from transaction bodies an arcade may not hold, so no two arcades are sure to have the
same journal.

### Listing

A segment is offered by an arcade with draw weight that keeps the chain (and the index) itself, and is **confirmed**:

- **blocks and coins**: by more than **a tenth** of the draw weight (rules version 3). Such a segment is in a
  **challenge window** of 100 blocks (`CHALLENGE_WINDOW`): listed, but no replica of it registers until the window
  is over.
- **every index family**: by more than **half** of the draw weight. **Rules version 4 (testnet only):** by more than
  a tenth too, with the same challenge window, so one of the two genesis writers lists an index segment alone.
  Mainnet keeps half.

On the testnet today the draw weight is the two genesis writers' (the GX10 and the JB20B), so only the chains and
families they keep can be listed, and an index family needs both of them. A sealed segment that isn't listed shows
`not listed` (or `offered`) in `registration` and can't earn until it is (issue #14). Attesting without keeping the
chain (ARCADECHAIN.md §2.4) isn't built.

## Registration

- What goes in a batch: this arcade's sealed replicas whose segments are **listed** (past any challenge window) and not
  registered, up to 4,096. The arcade waits until no new one has arrived for a minute, or the oldest has waited 30
  minutes, so segments closing one after another go together.
- **One batch an audit period**: a commit is refused until **1,152 blocks after the block that held the last batch's
  proof** (`registry::commit_item`; `arcseg::register` waits for `tip + 1 >= last_batch + 1,152`). A segment sealed
  after the first batch waits up to 2 days. `registration.next_batch` and each replica's `batch_at` give the block.
- The commit (anchored to a block at most 100 back, with a 20-bit work stamp) goes in a block `C`; the seed of block
  `C + 10` picks 16 leaves, and the proof is due in blocks `C + 11` to `C + 30`. The batch registers whole with its
  proof, or not at all; a commit whose window passed can be made again.
- Pay is sent to this arcade's wallet's first address on the ARCADE chain, as named in the commit.

## A worked example: switch-on to first coin

One arcade joining the testnet; a Pepecoin testnet node kept in full, turn **345** (its owner key hashes to 345, so
its turns are blocks 345, 1,497, 2,649, ...). Hours are at 150 s a block.

| Block | What happens | Why |
|---|---|---|
| ~400 | The operator installs Pepecoin testnet in full. Earn ARCADE is on by default; the ARCADE chain installs and syncs. | `arc.earn`, §14.17 |
| 400 to ~520 | Sealing runs from block 0 up, block by block, 100 blocks below the coin's tip; each segment closes when its 10,000-height range ends with 16 MiB in it. | `arcseg::auto_seal` |
| ~520 | The first 12 block segments are sealed. They were listed long ago (blocks 25 to 53) by the genesis writers, whose windows are over. | `catalog.rs` |
| 545 | The batch's commit goes in (a minute after the last segment closed). | `arcseg::register` |
| 556 | Its proof goes in (the seed of block 555 picked its leaves). The 12 replicas are registered; pay weight from here. Next batch: block 556 + 1,152 = **1,708**. | `registry::proof_item` |
| 600, 700, ... | Each epoch's pot (about 49 × 100 ARCADE plus fees) is shared by weight; this arcade's share accrues. | `registry::step` |
| 1,497 | First turn: no audit covers this period yet (`waits: no_audit`), so nothing is paid. The turn's seed draws 4 replicas for audit, picked in blocks 1,498 to 2,589. | `release_at`, `draw_audits` |
| e.g. 1,600 | A replica picked at block 1,600: its commit in blocks 1,601 to 1,620 (say 1,601), its opening in blocks 1,612 to 1,631 (say 1,612). Passed: it counts for the draw from block 1,812. | `arcaudit.rs`, `DRAW_WEIGHT_WAIT` |
| 1,708 | A segment sealed after block 556 (say a Flopcoin one listed at block 600, its window over at 700) can go in the second batch. | one batch an audit period |
| **2,649** | Second turn: all 4 audits passed, the verdicts don't hold it: the coinbase of block 2,649 pays everything accrued from block 600 to 2,600 in one output. | first payout |
| 2,749 | That output can be spent. | `COINBASE_MATURITY` |
| 3,801, 4,953, ... | Paid again each period, at its turn. | |

From the proof in block 556 to the first coin: 2,093 blocks, about 87 hours. In general the first payout comes
**1,152 to 2,303 blocks** (2 to 4 days) after the registration proof, depending on where the proof falls before the
arcade's turn; the time before that (installing, syncing, sealing, waiting for listing) depends on the machine and on
whether the weighted arcades list what it keeps.

## Testnet and mainnet

| | Testnet today | Mainnet (2027-01-01) |
|---|---|---|
| Pay | accrued each epoch, paid at each earner's turn, released by audits | the same |
| Registration and audits | built and running | the same |
| Proof of time | the real one (a class-group VDF, as Chia's) | the same |
| Writer's 1 ARCADE | from block 0 (rules version 3) | from block 0 (rules version 2 and up) |
| Genesis earners | the GX10 and the JB20B, 1 GiB each | the genesis list (ARCADECHAIN.md §10), audited before block 0 |
| Listed chains | 12 (above) | 10 (above) |
