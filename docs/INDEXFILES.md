# Index files: settled address and tx index rows in compressed files

Status: **part 1 built** (branch `team/index-files`): the file format, writer and reader, era merge, conversion from today's tables, and readers that use the files below a per-chain marker. Nothing changes for a chain until its operator converts it. Not built yet: see §7.

Code: `crates/arcade-chaindb/src/ixfile.rs` (format), `ixset.rs` (a chain's files, the marker, what the readers ask), `ixconv.rs` (conversion), the readers in `db.rs`, tests in `ixfile.rs` and `src/tests/ix.rs`, a hand tool in `examples/ixfiles.rs`.

## 1. Why

The address tables (`core.v2.script_hist`, `core.v2.spends`) and the tx index (`core.v1.txs`) are almost entirely history that never changes, kept uncompressed in B-trees built for random inserts. On Bitcoin they are an estimated 0.7–0.95 TB. The same rows, sorted, cut into blocks and compressed by column, take roughly a sixth of that (§6), and a file of them is the same bytes on every arcade, so it can later be shared and checked by a hash.

## 2. The format (version 2)

One file holds the rows of one **kind** for one span of heights: a **range** (10,000 heights) or an **era** (100,000 heights, ten ranges merged). A file is written once and never changed.

    header   64 bytes
             magic "ARCIXF\r\n" (8) | format u16 | kind u8 | 0 | chain number u16 |
             row size u16 | key size u16 | 0 0 | rows per block u32 |
             first height u32 | last height u32 | hash of the block at the last height (32)
    blocks   a fixed number of rows each, the last one shorter
             filter: 10 bits a row (fixed per kind), uncompressed
             per column: raw length u32 | stored length u32 | one zstd frame
             (a column of only zero bytes is stored as no bytes: stored length 0)
    footer   per block: first key | file offset u64 | sha256 of the block's canonical rows |
             sha256 of the block's filter
    trailer  rows u64 | blocks u32 | footer offset u64 | file id (32) | checksum (32) | "ARCIXEND"

All integers are big-endian. File name: `<kind>-<first height, 8 digits>-<last height, 8 digits>.ixf` in `<chain directory>/index/`.

- **Canonical rows** are fixed-size and big-endian, so their bytes sort as their keys do. They name a block by **height** and a transaction by its **position in the block**, never by the local block number, so every arcade on the same chain has the same rows.
- **Rows per block are fixed per kind**, not a size target, so every arcade cuts the same blocks.
- **Hashes are over canonical rows**, not compressed bytes. The **file id** is sha256(header | row count | every block's first key and row hash): it does not depend on the zstd version or level. The first keys are in it because the readers search them: a file with the right id cannot steer a lookup to another block (version 2; version 1 left them out). With the same zstd version and level (19; the version is pinned by `Cargo.lock`) the whole file is byte-identical, which the tests check across builders, thread counts, interrupted writes and merged-versus-direct eras.
- **Checksum**: sha256(header | footer | trailer up to the file id). It is checked when a file is opened, with the first keys rising and no block longer than a block of its kind can be (so no stated length makes a reader allocate more than that). Each block is checked against its row hash and its listed first key every time it is read, after being decompressed under a hard size cap (32 bytes a row and column).
- The footer (one entry per block) is held in memory while a file is open. For all of Bitcoin that is about 125 MB (about 1.3 M blocks of rows across the three kinds, with both hashes); the first keys alone would be about 30 MB.
- **Filters.** Each block starts with a Bloom filter (7 probes, a fixed arithmetic hash, the same on every machine) of its rows' lookup keys: the script id (H), the coin (P), the 8-byte txid prefix (T). A lookup reads and checks the filter of the one block its key could be in, and decodes the block only when the filter says the key may be there, so an address or txid that a file doesn't hold costs a small read, not a decompression. The filter's hash is in the footer and in the file id; `verify` rebuilds every filter from the rows.

### Kinds

| Kind | Row (key → value) | Row | Rows per block | Replaces |
|---|---|---|---|---|
| `H` history | script id (21) · height · position → received u64 · spent u64 | 45 B | 8,192 | `core.v2.script_hist` |
| `P` spends | created height · position · vout → spender height · position · vin | 24 B | 16,384 | `core.v2.spends` |
| `T` tx index | first 8 bytes of the txid · height · position (the whole row is the key) | 16 B | 4,096 | `core.v1.txs` |

- **H** is one row per (address, transaction), as in the table. The table's key has the first 12 bytes of the txid; the file has the position, and a reader that needs the txid reads that transaction from the stored block.
- **P** rows are filed under the **spender's** height: a spend comes into being with the spending block, so no older file ever changes. Finding who spent a coin means looking in the files from the coin's height on (§4).
- **T** keeps 8 bytes of the txid. Two transactions may share them; a lookup always confirms the full txid on the stored transaction.
- Address coins (`core.v2.by_script`) stay in MDBX: small and always changing.

### Column coding

Each block is turned into columns, and each column is compressed on its own (its own entropy table):

- `H`: a new-script flag per row · the new script ids (21 bytes each) · height as a varint (the step from the row above when the script is the same, else above the file's first height) · position · received · spent (varints).
- `P`: created height (step from the row above) · position (step while the height is the same) · vout (step while the transaction is the same) · the coin's age in blocks (spender height less created height) · spender position · vin (all varints).
- `T`: byte planes, most significant first: 8 planes of the txid prefix's step from the row above, 4 of the height above the file's first, 4 of the position. Txid prefixes are random, so their sorted steps are near-random numbers of a known size: byte planes cost about what the entropy is, where varints waste most of a byte a row.

## 3. Conversion from the tables

`ChainDb::convert_to_index_files(params, options, stop, note)`; by hand, with the arcade stopped: `cargo run --release -p arcade-chaindb --example ixfiles -- convert <chain.mdbx> <chain key>` (Ctrl-C saves a checkpoint and stops; a second Ctrl-C quits at once). Only the standard layout (ranges of 10,000, eras of ten) is accepted outside the tests.

- **What is settled.** The sealed height is the end of the last whole range that lies at least 1,000 blocks (`UNDO_DEPTH`) below both the tip and the height the coins are complete at. With a tip at 20,999, heights 0..=19,999 are sealed. Only settled rows are converted.
- **One scan per kind**, in key order, in short read transactions (200,000 rows each), so the database can recycle pages while the chain keeps syncing. Each row goes to the writer of its height's span: an era file where a whole era is settled, range files above it. Order: tx index, spends, history. Spends and history only while the Addresses index is on.
  - `txs`: a row's block number becomes its height; rows sharing 8 prefix bytes are sorted by height and position.
  - `spends`: block numbers become heights, and the row goes to the spender's span. This relies on block numbers rising with height on the main chain (true by construction: a block gets its number when it is first connected); it is checked first, and a chain where it does not hold is refused.
  - `script_hist`: the txid prefix becomes a position through one lookup in `txs` per row, then the rows of one (address, height) are sorted by position. **This lookup is random, not sequential** (see §8).
- **Checked before use.** Every finished file is read back whole (`Reader::verify`) and marked so (`<file>.ok` beside it, holding the file's id). A reader adopts a file (`KindSet::load`) only after reading it whole when it has no matching mark: a file copied in from another arcade, or written before marks existed, costs one full read the first time (Bitcoin: ~110–135 GB; `ixfiles verify` marks files ahead of that). This is the hook for files shared over the mesh: the id is not redesigned, and nothing is trusted for coming with a matching id. The marker is set in a write transaction that checks the chain still has the same block at the sealed height, and checks again that every row was in its table: nothing waits in the address log or the tx index runs, and (history, spends) the Addresses index is on and was not turned off or on since the pass began (a STATE counter, `ixf:addr_epoch`). The marker is flushed to disk (chain.mdbx does not flush every commit) before any range it no longer uses is deleted.
- **Resumable.** Every 50 M table rows, and when asked to stop, a pass saves a checkpoint (`index/convert-<kind>.ckpt`): where the scan is, and each open file's state (its block list and its open block's rows). The next run carries on from it and ends with the same bytes. A finished kind is skipped. A checkpoint of another job (another layout, level, chain, or Addresses epoch) is discarded. A pass from scratch (no marker) first deletes every file of its kind, so nothing an earlier install or an unfinished pass left is adopted.
- **Running it again later** converts the ranges that have settled since, merges ten ranges into their era (an era file already there, from a pass that stopped before switching, is read whole first and merged again if it is bad), switches the marker, then deletes the merged ranges. In this part that costs a full scan of each table again (the tables still hold everything); with the tail-only tables of part 2 it is a scan of about two ranges.
- **Room on the drive.** With `Options::free_bytes` set (the `ixfiles` tool sets it; the library makes no system calls itself), a pass first waits until the index directory's drive has `floor_bytes` (`ARCADE_IX_FREE_GB` in the tool, default 50) plus its estimated size free: the table's rows at a little over the measured bytes per row (history 18, spends 6, tx index 12), less what the kind's files already hold. It says so (`held: … GB free …`) every five minutes and goes on by itself once there is room; a stop while held writes nothing. At each checkpoint it waits again if the drive has dropped under the floor, and before each era merge it waits for the floor plus the era's ranges' bytes.
- **The tables are not changed.** `ChainDb::set_index_files_off(c)` (or `ixfiles off`) removes the markers and the tables answer alone again.
- Refused while address rows or tx index rows are still being built (`addr_pending`, `txs_pending`, tx index runs).

## 4. Readers

Per kind, STATE `ixf:<H|P|T>` = sealed height u32 | generation u32 (the last generation is also kept in `ixf:gen:<H|P|T>`, which `off` does not delete, so a marker never repeats an old value and a cached set is never mistaken for a new one; removing the chain also drops its cached sets). The sets are loaded outside the cache lock. **No marker: every reader does exactly what it did before.** With a marker `F`: heights ≤ `F` from the files, heights above from the tables. The marker is read in the same read transaction as the table rows, so one answer is one consistent view.

| Reader (`db.rs`) | With a marker |
|---|---|
| `history_for_script` | The table alone until the history table is flagged trimmed (`ixf:trim:H`; nothing sets it in this part, and the explorer must page by position first). Then each file: binary search of the in-memory first keys, read one block (the next only while the address's rows run on). Then the table's rows above `F`. The txid prefix of a file row is read from the stored block (one read per row); rows are returned in the table's order (height, then txid prefix) |
| `history_count_for_script` (new) | The same without reading any transaction. Used by the profile API, which only counts |
| A file's lookup | Binary search of the first keys for the block the key could be in (not the block before it when a block starts with exactly the key), then that block's filter, then the block |
| `tx_raw`, `tx_location`, `txids_by_t12`, `coin` | The `txs` table first; when it has no match and the table is flagged trimmed (`ixf:trim:T`), the T files: one block read per file, full txid confirmed on the stored transaction. The mempool's "in a block yet?" checks use `tx_raw_tables`, never the files |
| `spender` | `txs`/T to find the coin, then the `spends` table; when it has no row, the table is flagged trimmed (`ixf:trim:P`) and the coin is not unspent, the P files from the coin's height on, first hit wins |
| `balance_for_script`, `coins_for_script` | Unchanged (`by_script` stays in MDBX) |
| Connecting blocks (`connect_one`, `locate`, `locate_c`) | **Unchanged: tables only.** See §8 |

While the tables still hold the converted rows (all of this part), a tx or spend lookup that misses the table is a miss: no step sets the trimmed flags yet, so no reader reads a file and a damaged one breaks no lookup. The tests also delete the settled rows from the tables and check that every answer stays the same from the files alone.

### Reorgs

Index files only cover heights at least 1,000 blocks below the tip. That is the depth beyond which this database cannot disconnect a block at all (no undo record is kept deeper), so no reorg reaches a height a file covers, and files never need rewriting. A coin-cache wind-back at start goes to the height the coins were complete at, which the sealed height also stays 1,000 below. Every file's header names the block hash at its last height; a set whose file does not end on this chain's block at that height is refused when loaded.

### Errors

A missing, shortened, damaged or foreign file is an error from the reader that needs it, never a wrong or silently shorter answer. Other kinds keep working.

Turning the Addresses index off (or on) removes the history and spends markers: their tables are emptied, and files alone would give a part of the answer. They come back with the next conversion after the rows are built again.

## 5. Eras

Ten range files of an era are merged (a k-way merge of sorted rows) into one era file when the tenth is settled. A merged era is byte-identical to one written directly. The ranges are deleted after the readers have switched. When loading, at each height the file reaching furthest is used, so leftovers are harmless.

## 6. Sizes

Measured 2026-11-20 by `ixfiles bench` on real Bitcoin address run files from a 32-core NVMe test machine (heights 609,000–615,000; one run file, and two merged), zstd 19, including footers:

| Kind | Run rows | Raw in the run file | Index file, 1 run | Index file, 2 runs merged | Earlier whole-file estimate |
|---|---|---|---|---|---|
| H history | 3.70 M (3.27 M file rows) | 55 B/row | 14.5 B per run row (16.4 per file row) | 13.5 (15.5) | 8.6 |
| P spends | 1.87 M | 26 B/row | 3.8 B/row | 3.7 | 2.6 |
| T tx index | 3.78 M | 22 B/row | 8.4 B/row | 8.5 | 15.0 (12-byte prefix), ~9 est. |

**History is about twice the design's estimate, spends about 1.4 times; the tx index is as hoped.** The earlier figures compressed each column over a whole run file at once, where zstd finds repeats far apart (an amount received in one row and spent in another, for one). Cut into blocks of 8,192 rows sorted by address, most of that is out of reach, and script ids (hashes, about 21 bytes per new address) do not compress at all. Whole-chain estimates at these rates: history ~90–110 GB, spends ~10–13 GB, tx index ~10–12 GB: roughly 110–135 GB where the design said 75–100, against 0.7–0.95 TB of tables. History should gain from era files (longer runs per address), and has clear room left (bigger blocks, coding amounts against the address's earlier rows, a per-file dictionary); any of that is a new format version and must be settled before files are shared.

Notes: run files hold one history row per output or input; a file row is one per (address, transaction), as in the table. The run files carry no tx position, so the benchmark uses the rank of a row's txid among its block's as the position (same range and spread). That machine's T run files only hold transactions whose outputs were all spent before a cache flush, which does not change their randomness.

## 7. Not in this part

- **Catch-up buffers instead of `addr_log`** (new installs never writing the log, the runs or the big tables), and the **sealer** at the tip that writes each range as it settles.
- **Reclaim**: deleting converted rows from the tables and compacting. Needs §8's tx-lookup question answered first.
- **Range-parallel build from blocks plus spent coins (B + S)**, and the cross-check that both builders give the same file ids.
- **Mesh sharing** of files, manifests, checkpoints.
- A live trigger in the arcade (settings or admin action), progress in chain status, and the explorer reading history by position page by page (today `history_for_script` resolves every row's txid, which is slow for an address with a very long history once its chain is converted).
- Moving index files with a chain (`ChainDb::relocate` refuses a chain that has them), and deleting them when a chain is removed.

## 8. What the design got wrong or left open

1. **Tx lookups while connecting blocks.** Every input of every new block is located by txid through the tx index. If `txs` lives only in T files, each input costs up to one block decompression per era (about ten on Bitcoin), far too slow at the tip. Before any `txs` rows are removed, either a small per-file filter, or a table of the txids that still have unspent coins, is needed. This part keeps `connect_one` on the tables.
2. **History conversion is not a purely sequential scan.** `script_hist` names a transaction by txid prefix and is sorted by address, so turning prefixes into positions is one random `txs` lookup per row (about 6 G on Bitcoin; hours with the table in memory, much longer on a hard drive). The build from blocks (part 2) has the position at hand and avoids it.
3. **History rows are about twice the estimated size** in fixed blocks (§6): 13.5–16.4 bytes a row measured, not 8.6.
4. **Memory for the footers** is about 125 MB for Bitcoin, not "a few megabytes" (the row and filter hashes are kept with the first keys).
5. **`history_for_script` returns txid prefixes**, so a converted chain pays one transaction read per history row; the count-only path avoids it, and the explorer should move to positions (part 2).
6. **Spends rows carry heights, not block numbers**, so a P lookup needs the coin's height (one block-meta and header read) and the answer one height-to-block read. Cheap, but not free.
