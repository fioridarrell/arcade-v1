# Faster Bitcoin installs: index files, then mesh chunk bootstrap

Status: **design, partly built** (see [Status](#7-status)). Nothing here is enabled by default and
nothing has been run against real Bitcoin data end to end yet. Figures are marked
*(measured)* or *(est.)*.

## 0. In short

| | Scenario A: build it yourself, smaller | Scenario B: fetch verified chunks |
|---|---|---|
| When | Always available | Once a signed release carries a checkpoint for the chain |
| How a node gets the chain | Syncs and validates every block itself (init install), writes settled index rows as compressed immutable files instead of large tables | Fetches the same files in chunks from many holders at once, checks every chunk against checksums pinned by the signed release |
| What is checked | Every block, every rule | Full header chain, every chunk's checksum, and everything above the checkpoint as today |
| Where trust comes from | Own validation | The signed release (the same trust as running its binary), never the holders |
| Build order | First | Second, reusing A's files unchanged |

- The address and tx index section is the biggest part of a Bitcoin install. In A, settled rows
  go into compressed, sorted, column-coded files per range of heights. Measured row sizes give
  roughly **110–135 GB** of index files where the classic tables take an estimated
  **0.7–0.95 TB**.
- The files an A node writes are exactly what a B node downloads. Nothing is re-exported, so
  moving from A to B needs no flag day.
- Holder count gives **availability, never truth**. Mesh identities are cheap, so three holders
  agreeing proves only that three keys agree.

## 1. Why installs are slow today

- **One writer.** The normal sync downloads in parallel but hands every block, in height order,
  to one writer that spends coins, checks rules and updates every index in the same commit.
- **The address section.** During catch-up every address row is written three times (a log in
  the chain file, sorted run files, the final tables), uncompressed, into B-trees built for
  random inserts, although history below the tip never changes.
- Measured: a classic Bitcoin install took **about 7.5 days** on a 32-core NVMe test machine
  *(measured)*.

## 2. Scenario A: init install plus index files

### 2.1 Init install (opt-in, Bitcoin only for now)

`arcade chain add btc --init` (docs/INITSYNC.md) splits the install into passes that each do one
thing over the whole chain:

1. **Headers**, agreed by independent sources and required to reach the chain's
   `min_chain_work`.
2. **Blocks stored only**: fetched in any order, each checked against its header before it is
   kept (byte-identical rebuild, merkle root, no mutated tree, witness commitment where SegWit is
   on, no trailing bytes, AuxPoW where used, and on Bitcoin no 64-byte transactions above the
   minimum-chain-work block). No coin or script rule yet.
3. **One-pass build**: main chain, tx index, coin set, then every rule and script over all cores,
   then the tip. A block that breaks a rule is found here, and the chain is not used (no coins,
   addresses, explorer or RPC answers) until the build is done.

Chains without a `min_chain_work`, and proof-of-stake chains, are refused: without a work floor a
few agreeing sources could be one party, and stake is only checked when a block is connected.

### 2.2 Index files (docs/INDEXFILES.md)

One file per **kind** per **range** of 10,000 heights, written once and never changed; ten
ranges merge deterministically into an **era** file of 100,000 heights.

- **Kinds:** H address history (script id, height, tx position → received, spent), P spends
  (created height, position, vout → spender, filed under the spender's height so files stay
  immutable), T tx index (txid prefix → height, position). Unspent coins by address stay in the
  database: small and constantly changing.
- **Blocks of a fixed row count** per kind, each column delta/varint coded and zstd compressed,
  with a per-block filter, so every arcade cuts identically and a lookup decompresses one block.
- **Hashes over canonical rows**, not compressed bytes (compressor output may differ between
  library versions; row content cannot). The footer lists each block's first key, offset, row
  hash and filter hash; the file id covers them.
- **Heights and positions, never local block numbers**, so the same rows are the same bytes on
  every arcade.
- **Readers** use the files below a per-chain marker and the tables above it. A chain with no
  marker behaves as today.

Built so far: format, writer, reader, eras, and conversion from existing tables. Not built yet:
writing the files during catch-up instead of the address log, a sealer at the tip, and building
ranges from blocks plus spent coins in parallel.

### 2.3 Sizes

Bytes per row measured on real Bitcoin run files (zstd 19, one and two runs merged) *(measured)*,
scaled to whole-chain row counts *(est.)*:

| Kind | Bytes per row | Whole chain |
|---|---|---|
| H history | 13.5–16.4 | ~90–110 GB |
| P spends | 3.7–3.8 | ~10–13 GB |
| T tx index | 8.4–8.5 | ~10–12 GB |
| **Index files** | | **~110–135 GB** |
| Classic tables (history, spends, tx index) | | ~0.7–0.95 TB |

### 2.4 Speed

| Install | Figure |
|---|---|
| Classic, today | ~7.5 days on a 32-core NVMe test machine *(measured)* |
| Init install + index files | **10–17 h** on the same class of machine *(est.)* |

An 8-core HDD node is limited by random reads in the coin derivation and the rules pass (one
tx-index lookup per input). A sorted join of all inputs against the tx index would make that
sequential; not built.

## 3. Scenario B: mesh chunk bootstrap with checksum checkpoints

### 3.1 The idea

Once a release carries a checkpoint, a new node does not redo the work below it. It fetches the
finished pieces (blocks, coins and index files) as chunks from every holder at once and checks
each chunk against a checksum before using it.

### 3.2 Where the checksums come from

| Claim | Holds? |
|---|---|
| "Three holders announce the same manifest, so it is right" | **No.** A mesh identity costs a key. Agreeing holders prove availability and speed only |
| "The manifest id is in the signed release I run" | **Yes; this is the trust.** Whoever can sign a release can already ship any code |
| "My own machine verified the history" | Stronger, and available to any node through background verification (§3.5) |

A **checkpoint** is a record in the signed release (in the chain params, or a sidecar file signed
with the release key so it can be refreshed without a new binary):

    {net, M, hash_at_M, chain_work_at_M, manifest_id, coins_hash, fmt}

- `manifest_id` is the hash of the chain manifest at milestone `M` (the last height of a complete
  range). The manifest lists, per range, the ids of the block, spent-coin and index files, and the
  id of the coin set at `M`. One 32-byte value pins every chunk.
- **A checkpoint is made** only when at least three arcades, run independently, have each
  completed Scenario A's full validation to `M` and computed the same `manifest_id`, and the coin
  set's hash matches an independent reference implementation. "Three" is used twice, for
  different things: three **verifiers** to justify signing, three **holders** to make the
  download fast.

### 3.3 Holders, have-lists, fetching

- **Have-lists.** Each holder adds, per chain, the manifest id it holds and which ranges and
  families it has to its signed hold announcement. Old arcades ignore the field. Range
  granularity keeps this to a few dozen bytes per chain.
- **Holders give availability only.** A holder's identity or count never enters whether a chunk
  is accepted; only its checksum does.
- **Delivery-ranked.** An untested holder gets one chunk in flight, rising as it delivers;
  holders are ranked by measured rate, and a throughput floor drops slow ones. Rarity counts only
  holders that have actually delivered, so fake holders cannot make a range look common.
- **Rarest-first** at range level. Validation below the checkpoint no longer needs height order,
  so the rarest ranges are fetched first. A short endgame asks a second holder for the last few
  stuck chunks.
- **Bitcoin P2P is a holder of blocks only**; whole blocks from peers assemble into the same
  range files.
- **Seed while fetching.** A node announces a range once every chunk of it matches the manifest.

### 3.4 Verification before adoption

Every chunk is decompressed with a hard size cap, hashed, and compared with the manifest before
it is written where readers can see it. A mismatch drops that holder for the chunk and the chunk
is fetched elsewhere. A file is adopted only when all its blocks match, and the manifest's id must
equal the release's.

| Skipped below the checkpoint | Always runs |
|---|---|
| Script and signature execution | Full header chain from genesis: proof of work, difficulty, timestamps |
| Per-block rules (amounts, fees, maturity, locktimes, weight) | `min_chain_work` reached, and our own header at `M` equals `hash_at_M` |
| Deriving index rows: index files are adopted as delivered | Every chunk's hash against the manifest; the manifest's id against the release |
| | The coin set's hash against `coins_hash` |
| | **Everything above `M`**: full validation of every block, as today; reorgs, undo records, mempool rules |

### 3.5 After the bootstrap

- **State shown:** "checkpointed (release X, block M)", saying plainly that this node did not
  validate history itself; the release vouches for it.
- **Background verification** (Scenario A's validation of history, at low priority) is on by
  default and can be postponed on small machines. When it completes the state becomes
  "verified".
- A checkpointed node **seeds** what it holds, since all of it is hash-pinned, but **does not
  count as a verifier** for the next checkpoint until its own verification completes; otherwise a
  checkpoint could be confirmed by nodes that only copied the previous one.
- If background verification ever disagrees with a release checkpoint, the node stops serving
  that chain, records the event, alerts its operator and resyncs by Scenario A.

### 3.6 Speed *(est.)*

Below the checkpoint the node mostly hashes and writes. Estimated full install on a fast link:

| Holders | Install |
|---|---|
| 3 | **~5 h** (bound by the holders' upload) |
| 10 | **~3 h** (bound by the installer's own link and disk) |

With fewer than three delivering holders for a range, the node falls back to Scenario A for that
range (blocks from Bitcoin peers, its own build).

## 4. Upgrade path A → B

**A B node downloads what an A node already has on disk.**

| Produced by an A node anyway | Used in B as |
|---|---|
| Index range and era files with per-block row hashes | The index chunks; one block of rows is the unit of verification |
| Block range files | The block chunks |
| Spent-coin range files | Chunks for background verification |
| The coin set at each milestone, made at depth (never at the tip) | The coin snapshot |
| Its own chain manifest | The value independent verifiers compare and the release signs |

Versioning:

- Every file header carries its format version; the manifest lists it per family, so a changed
  encoding gives a different manifest id and the two never mix.
- The manifest has its own version; a node ignores families it does not know.
- A node may hold old and new formats side by side during a transition.

| Release has a checkpoint? | ≥3 delivering holders? | The node does |
|---|---|---|
| No | any | Scenario A |
| Yes | No | Scenario A for missing ranges, the checkpoint path where available; or waits, by operator choice |
| Yes | Yes | Scenario B |

Mixed meshes work: an A node and a B node at the same milestone hold byte-identical files. Moving
the network from A to B is one event that touches no running node: a release that contains the
first checkpoint.

What must be right in A so that B needs no rework:

1. Canonical rows use heights and positions, never local block numbers.
2. Fixed rows per block, hashes over canonical rows.
3. Deterministic range and era boundaries and sealing depth.
4. Every builder (conversion from tables, catch-up, blocks plus spent coins) gives the same file
   ids; tested between independent arcades before anything is shared.

## 5. Per-chain extension (planned)

Bitcoin comes first. Other chains follow per chain, each step gated by what that chain has:

| Step | Needs, per chain |
|---|---|
| Index files (conversion from tables) | Nothing chain-specific; works for any chain with the address tables |
| Init install | A `min_chain_work` in the chain's params (from its Core's `nMinimumChainWork`) and no proof of stake |
| Stored-block checks | Already per chain: witness commitment where SegWit is on, AuxPoW for merge-mined chains, MWEB bytes allowed only where the chain has them. The 64-byte transaction refusal is Bitcoin-only until other histories are checked |
| Checkpoints and mesh bootstrap | A per-chain checkpoint record, three independent verifiers of that chain, and a coin-set hash definition. Open: Litecoin's MWEB state is not covered by a hash of the canonical coin set; coinstake chains need the coinbase flag defined as coinbase-or-coinstake; some chains' reference nodes have no coin-set hash to compare against |

## 6. Open points

- History rows measured at about twice the first estimate in fixed blocks; era files, larger
  blocks or coding amounts against an address's earlier rows may reduce it. Any change is a new
  format version and must be settled before files are shared.
- Tx lookups while connecting blocks: if the tx index lived only in files, each input could cost
  one block decompression per era. Before table rows are removed, a per-file filter or a table of
  txids with unspent coins is needed.
- Converting history from tables needs one tx-index lookup per row (txid prefix to position); the
  build from blocks avoids it.
- Coin-set hash, spent-coin format and manifests are not built.

## 7. Status

Built, as separate draft changes (unit tests pass; **not yet tested on real Bitcoin data**, a full
real-Bitcoin init test is queued):

| Change | What |
|---|---|
| Stored-block checks | `store_blocks_only` refuses stripped witnesses, trailing bytes, bad AuxPoW, mutated merkle trees (CVE-2012-2459) and, on Bitcoin, 64-byte transactions above block 691,719 |
| Init install (`--init`) | Headers first, store-only blocks, one-pass build; Bitcoin only; docs/INITSYNC.md |
| Index files | Format, writer, reader, eras, conversion from tables, readers behind a marker; docs/INDEXFILES.md |

Not built:

- writing index files during catch-up and sealing them at the tip (no address log, runs or big
  tables for new installs);
- reclaiming table space after conversion;
- spent-coin files, the range-parallel build and verification from blocks plus spent coins, and
  the coin-set hash;
- chain manifests, checkpoints in releases, have-lists, chunk fetching and serving (all of
  Scenario B);
- background verification and the "checkpointed" state;
- any chain other than Bitcoin for init installs.

Numbers: the ~7.5-day classic install and the index row sizes are measured; the 10–17 h, ~5 h and
~3 h install times and the whole-chain index size are estimates until the queued real-Bitcoin test
and later work replace them.
