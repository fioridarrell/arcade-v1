# One database per chain — PLAN

Status: **approved by Martin 2026-11-10; built (PR feat/split-db).** Written 2026-11-10 on the JB20B, after Dogecoin's address build and ord import.

Decisions taken with the approval (the plan's defaults): transition mode (chains move one at a time), a separate tool run with the arcade stopped, index-copy files later.

## As built
- **`ChainDb::store(c)`** gives the file that holds chain `c`, for reads; it never creates a file. **`ChainDb::wstore(c)`** gives the file to write into: a chain with nothing in the shared file gets `chains/<key>/chain.mdbx`, created at its first write. That covers every new install, with no hook in arcaded. **`store_for(key)`** routes by a key's first two bytes.
- **Open:** every `chains/*/chain.mdbx` is opened at start, and the log says which chains have their own files.
- **One batch lock per file**, not one for the process. `flush_all_for_stop` takes each chain's lock in turn.
- **Removing a chain:** its rows in the shared file are deleted in batches, then its own directory is deleted whole. arcaded waits 10 s after stopping the chain's threads.
- **Tools and tests that read a whole table** use `scan_all` / `count_all` / `table_sizes_all` / `addon_tables_all` across every file.
- **Tests with several databases in one directory:** a shared file not named `chain.mdbx` keeps its chains in `<name>.chains/`.
- **`arcade-split`** (`cargo build --release -p arcade-chaindb --bin arcade-split`):
  - `arcade-split --home DIR` lists the chains and where each one lives;
  - `--chain KEY` moves one chain;
  - `--all` moves every chain still in the shared file, lowest tip first.
  It takes the home's `arcaded.lock`, so it refuses while the arcade runs. It copies with appends in durable ~256 MB transactions from one read snapshot, into `chain.mdbx.part`. It checks rows per table, first and last key, and STATE byte for byte, then renames. The release tarball doesn't carry it yet.

## Why
Today every chain lives in one file, `var/arcade/chain.mdbx`. On the JB20B that file is 739 GiB.

- **One writer for all chains:** libmdbx allows one write transaction per file at a time. So a slow Dogecoin write holds up every other chain.
  - Measured on the JB20B on 2026-11-10: `sync peptest at 1564471: 1 blocks, 1 txs · … write 312.30s`. That was one Pepecoin-testnet block waiting five minutes while Dogecoin's ord index caught up.
  - Pepecoin testnet and Flopcoin routinely show 15 s writes for a handful of blocks while Doge flushes.
- **Space never comes back:** the file never shrinks. Removing a chain leaves its pages free inside the file, and compacting means copying the whole 739 GiB.
- **One fault reaches everything:** a bad write, a full disk or a corrupt page in one chain's data puts every chain at risk.
- **Sharing is harder than it needs to be:** a chain's data can't be handed over as a file today.

## What does not change
- **Lookups:** the file is memory-mapped, so opening is instant at any size, and a smaller tree saves at most one level per lookup. Reads never wait for writers (MVCC). The limit is RAM against data size, and that stays the same.
- **Key layouts:** every chain key already starts with C, the chain number as a u16 (`schema.rs`). The new files **keep the C prefix**, so:
  - no key is rewritten;
  - the move is a straight copy;
  - the code paths that build keys stay as they are.
  The cost is 2 bytes per key.

## Layout
```
var/arcade/arcade.mdbx              node: wallet, settings, mail (unchanged)
var/arcade/chains/<key>/chain.mdbx  one chain: core.*, ord.*, tok.*, names.*, maps.*, ledger.*, msg.*
```
- **Each chain's indexes stay in that chain's file.** Indexers write in the same commit as the chain and are undone by the same undo record (`indexer.rs`), which is what keeps an index from getting ahead of or behind its chain. Separate per-index files would need a cross-file recovery scheme, so that is not part of this plan (see "Later").
- **Transition mode:** arcaded opens `chains/<key>/chain.mdbx` when it exists. Otherwise the chain stays in the old shared `chain.mdbx`.
  - Chains move one at a time.
  - **A new install always gets its own file.**
  - When the old file no longer holds any chain's rows, it can be deleted.

## Code changes (all mechanical except the audit)
1. **arcade-chaindb:** `ChainDb` holds `stores: RwLock<HashMap<u16, Arc<Store>>>` plus the legacy shared store.
   - `db.store` becomes `db.store(c)`.
   - Direct `.store.read/.write` call sites: chaindb 98, arcaded 84, ledger 34, app 21, oldimport 5.
2. **Per-chain globals:** these become per store:
   - the `BATCH` lock (`db.rs:506`);
   - the coin caches, which are already per chain;
   - the txruns filters, keyed by `store.id()`;
   - `flush_all_for_stop`.
3. **Scans across all chains go per store:**
   - `load_runs` and addrbuild scan `STATE` with an empty prefix to find the chains; they loop over the stores instead.
4. **Audit of cross-chain writes: done on the GX10, 2026-11-10. Clean.**
   - Every write transaction on chain.mdbx touches one chain. `connect_blocks_with` filters indexers by chain. `flush_coins`, `recover_coins`, `remove_*_batch`, `mempool_*`, `put_headers`, prune/restore, `disconnect_tip`, addrbuild and coreimport all take one C. The `0..=255` loops (recover at open, `flush_all_for_stop`) do one write per chain.
   - arcaded and arcade-ledger never write chain.mdbx directly; they go through chaindb or indexers in the chain's own commit.
   - Every key begins with u16 C. That was checked for every KeyBuf builder, including `engine.rs:714`.
   - Cross-chain **reads** exist: the Explorer and tag pages read the home ledger while showing another chain. No transaction needs those reads consistent, so separate files are fine.
5. **Callers that open `chain.mdbx` by path get a home instead:**
   - `arcaded/main.rs`, `sitesvc.rs` tests, `arcade-oldimport`, and the read-only probes (`--blocks-from`).
   - `ChainDb::open_home(home)` finds every `chains/*/chain.mdbx` plus the legacy file.
6. **Mesh copies (`v1copy`, `v1import`):** an import writes into the chain's own file. That's the same rows, in a different file.
7. **Removing a chain:** stop its threads, close its store, delete `chains/<key>/`. The space comes back at once.

## Moving the existing data: yes, from the current file
A new tool, `arcade-split --home DIR --chain KEY` (or `--all`), runs with arcaded stopped:
1. Open the old `chain.mdbx` read-only.
2. For each table, put a cursor on prefix C and copy every row into `chains/<key>/chain.mdbx.part` with `append` (dup tables: append-dup).
   - The rows come out in key order, so this is a sequential read and a sequential write.
   - Free pages aren't copied, so the new file is compact.
3. **Check:** row counts per table and the first and last key per table are equal, and `STATE` matches **row for row, byte for byte**.
   - **`_meta`:** copy only this chain's `addon:<table>` registrations (with the dup flag byte) plus the schema and version. Don't copy the whole `_meta`.
   - **Dup tables** (`core.v1.txs` and any dup add-on table) are copied with append-dup.
4. Rename `.part` to `chain.mdbx`. From the next start, arcaded uses it.
5. **The old rows stay where they are** until every chain has moved and the checks have passed. Then `chain.mdbx` is deleted, which needs **Martin's word**. Until then, going back is just deleting `chains/<key>/`.

- **Order:** small chains first (peptest, flop, pep, kot), Dogecoin last.
- **Space on the JB20B:** 939 GB is free. The new files together are smaller than the live data in the 739 GiB file, so everything fits before the old file is deleted.
- **Time:** measured on the GX10 with Pepecoin testnet, from the live chain.mdbx opened read-only.
  - **Method:** 200k-row chunks, each read in its own short read transaction and appended in one write transaction.
  - **Result:** 37 tables, 24,644,147 rows and 2.51 GB copied in 9 s, about 280 MB/s. Every table matched on row count and first and last key.
  - **Size:** the new file was 2.95 GB against 4.1 GB shared.
  - **Caveat:** the source was mostly in the page cache. The JB20B's 739 GiB will be read-bound, so plan for **1–3 h for `--all`**. `block_tx` and the ledger row tables are the big-value ones.
- **Restarting:** a chain is either finished (renamed) or not. An interrupted chain starts over: its `.part` file is deleted and the chain is copied again. Finished chains are skipped. A dup table can't be resumed in the middle of a key, so restarting the whole chain is the simple, safe rule.

## Later (not in this plan)
- **Index copies as files:** a read transaction copies one chain's index tables (`ord.*`, `tok.*`, `names.*`, `maps.*`) into a standalone mdbx file, in key order, at a known block.
  - The receiver checks the file the way copies are checked today, then appends its rows into its own chain file.
  - This brings the sharing benefit of per-index files without splitting live commits.
- **Per-index live files:** only if the above isn't enough. It needs its own tip and recovery per file.

## Questions for Martin
1. Should the old shared file stay supported (transition mode), or should every chain be moved in one go?
2. Should the move run inside arcaded at start, or as a separate tool run while arcaded is stopped (this plan)?
3. Should the index-copy files ("Later") be planned next?
