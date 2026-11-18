# ARCADE: a coin paid for keeping the mesh's chains and indexes — DESIGN

Status: **draft for Martin's review.** Written 2026-11-10 on the GX10; revised 2026-11-11 after an independent review (fixes are marked *(review finding n)*). Built so far (`crates/arcade-arc`, 77 tests): the sealing code, the block format, the writer draw, the script and coin rules, the chain's own rules (fork choice, difficulty, draw seeds, moving between branches) running on stand-ins for the proof of time and the storage records, the block log that keeps a chain across a restart, and the pool of waiting transactions. In the arcade (`crates/arcaded/src/arcsvc.rs`): a **local test chain** that only a hidden start option turns on, with a writer, blocks and transactions carried over mesh links, fees paid to the writer, the best branch copied into the chain store, and its own entries in the wallet, the Explorer and Chains & Coins (§14.9 to §14.11). §14 writes down the rules as built. *(Since then the public testnet runs the real proof of time, registration, audits and serving checks; §14 and §15 say what is built. For node operators, when and how pay comes: [EARN.md](EARN.md).)*

ARCADE is a new blockchain, built into Arcade V1.0. Its coins are earned by **keeping and sharing data**: the blocks of the chains the mesh follows and the indexes built from them. An arcade's chance of writing a block and its share of the reward both follow its share of the network's **verified** storage.

## Martin's decisions (2026-11-10)

| | |
|---|---|
| Name, unit | **ARCADE**, divisible to **8 decimal places** (1 ARCADE = 100,000,000 units) |
| Block 0 | **2027-01-01 00:00:00 UTC** |
| Testnet | **public testnet from about mid-December 2026**, throwaway coins; **mainnet genesis is fixed at Jan 1** |
| Block time | **2.5 minutes** |
| Reward | **50 ARCADE per block, forever** (no cap, no halving): about 10.5 million a year |
| Premine | **none**: every coin is earned by storage |
| Who writes blocks | **drawn by chance, weighted by verified storage** |
| Who gets the 50 | **everyone with verified storage, split by their share** |
| Fees | **into the pot**, shared by weight with the block reward (§14.18; the first draft gave them to the block's writer) |
| What earns | **every listed chain and index, by bytes; every verified copy earns, and each piece's share is split among the arcades holding it** (changed 2026-11-10 from "only the first 10 copies earn": that rule locked latecomers out and capped the network's size) |
| Anti-cheat | **sealed copies**: each arcade's copy is encoded with its own key and challenged on that copy |
| Sharing | **a serving check**: arcades must hand the data to other arcades on request, or they earn nothing (§5.4) |
| Time | **a proof of time** on every block, as Chia has, so history can't be rewritten quickly (§6) |
| Security on the testnet | **every guard in this document is in the testnet from its first day**; nothing is left for a later upgrade |
| Code | **native in V1.0** (Rust, inside `arcaded`) |

Everything below is my proposal for how to do that; numbers marked *(proposed)* are mine to change on review.

## 1. The shape of it

```
 source chains (Dogecoin, Pepecoin, Bitcoin, ...)
        │  blocks + index changes, cut into fixed segments
        ▼
 CATALOG on the ARCADE chain ── what data exists: one manifest root per segment
        │
        ▼
 REPLICAS ── an arcade seals a segment with its own key, registers the sealed root
        │                     (every verified replica earns; a segment's share
        │                      is split among its holders)
        ▼
 CHALLENGES ── every block's randomness picks pieces; holders answer with proofs
 SERVING CHECKS ── secretly drawn arcades fetch pieces and report who served
        │
        ├──► WRITER DRAW + PROOF OF TIME: the next block's writer, weighted by
        │    verified bytes; a block is valid only after a set amount of real time
        └──► REWARDS: each 100-block epoch the pot is shared by verified bytes;
             each arcade is paid at its own turn, once audits release it (§7)
```

A block has two parts:
1. **Coin transactions**: ordinary UTXO transactions in Dogecoin's format (P2PKH, P2SH), so V1.0's wallet, signing, explorer and coin index work on ARCADE as on any other chain.
2. **The storage section**: catalog entries, replica registrations, challenge answers and the epoch payout. These are not coin transactions: they need no coins and pay no fees (nobody has coins at genesis). Each kind is tied to something that can't be made up for free, and has a per-block limit (§9).

## 2. What data earns: the catalog

### 2.1 Segments

Paid data is **fixed once written**, so a sealed copy never goes stale:

- **One segment = one family of one chain over a run of whole 10,000-height ranges** (`RANGE`, as V1.0's shares already use), **at least 16 MiB of raw data** (Martin 2026-11-12: was 1 GiB; small so an arcade that keeps only a small chain, such as Pepecoin testnet, earns from most of it). A segment starts where the previous one ended and takes ranges until it holds 16 MiB, so it's the same everywhere. Early blocks are small, so one segment may span several ranges; a range of Dogecoin or Bitcoin blocks is a segment on its own, often far over 16 MiB.
- A segment is written as V1SHARE's copies are: canonical rows, **8 MiB pieces**, each piece zstd-compressed at a fixed level, a canonical JSON manifest. Any two arcades holding the same segment make byte-identical pieces, so the manifest id is the same everywhere.
- A segment is listed only when it is full (16 MiB) and its last block is **at least 100 source blocks deep** (*proposed*), so reorgs of the source chain don't touch it. A chain's newest data, under 16 MiB and the range it is in, waits for the next segment to fill.

### 2.2 Families

| Family | What a segment holds | Status in V1.0 |
|---|---|---|
| **blocks** | every raw block in the range (header + transactions) | made today (V1SHARE `blocks`) |
| **index journals**: ord, tokens, names, maps, ledger, addresses; dogefeed from rules version 4 (testnet) | for each block in the range, the changes that block made to the index (inscriptions born and moved, token transfers, names claimed, ledger actions, address spends and receives), in a fixed per-family record format | **new**: today V1.0 shares an index as its current tables, which change every block. A journal is the fixed history the tables are rebuilt from |

Index tables themselves (state) aren't paid: they change every block, so they can't be sealed once. Their journals are, and anyone holding the journals of an index can rebuild its tables.

### 2.3 Which chains

**Listed chains** are written in the chain rules (`params.rs`, `Net::listed`, by V1.0's keys). Adding a chain or family later is a rule upgrade (a new version all arcades take, activated at a set ARCADE height).

| Network | Listed |
|---|---|
| Mainnet `arc` | `arc`, `pep`, `peptest`, `doge`, `dgb`, `flop`, `wjk`, `kot`, `ltc`, `btc` |
| Testnet `arctest` | `arctest`, `pep`, `peptest`, `doge`, `dgb`, `dgbtest`, `flop`, `wjk`, `kot`, `ltc`, `ltctest`, `btc` |
| Testnet `arctest`, from rules version 4 | the above and `shic`, `bel`, `lky`, `jkc` (`Net::listed_v4`) |

That is the ARCADE chain itself, Pepecoin, Pepecoin testnet, Dogecoin, DigiByte, Flopcoin, Wojakcoin, Kotia, Litecoin and **Bitcoin**; the testnet adds the DigiByte and Litecoin test chains, and from the rules version 4 height (ARCADE issue #6) Shibacoin `shic`, Bellscoin `bel`, Luckycoin `lky` and Junkcoin `jkc`. Listing is read at a height (`Net::listed_at`): a catalog entry is judged at its block's height, so one for a Shibacoin segment is refused below the version 4 height and taken from it; block 0's catalog (the genesis list) at height 0; what an arcade offers, seals, watches for fraud and shows (Earn ARCADE's "not paid on this network", `arc.catalog`, `arc.sealing`) at the next block's. On mainnet those four are not listed. Listing a chain is a rules version, and each listed chain adds its headers to every ARCADE node (§2.4).

*What "adds its headers" is in the code (2026-11-16):* fraud proofs and challenges are judged against the arcade's own view of the source chain (`registry::Sources`, `arcsvc::NodeSources`): the height and block ids of the chain as this arcade keeps it, whole (its stored blocks) or by headers only (Coins & Chains "light", §15 step 15), and only about blocks `FRAUD_DEPTH` = 100 deep in it; an arcade that doesn't follow the chain makes a block holding such a proof wait instead of refusing it. Nothing is installed by itself (Bitcoin's headers are the operator's choice too). The four chains of version 4 need nothing of their own here: their headers are checked by the same sync as any coin's (Scrypt proof of work, and AuxPoW with each chain's id and strict-id rule; Luckycoin's revival hashing is a signature rule, not a header rule), and the fraud watch treats them as it treats Dogecoin and Flopcoin (merge-mined: a differing root is not demanded on the chain until leaves show it). Bellscoin's witness data and Junkcoin's witness and MWEB data are open in the same way as Litecoin's and Bitcoin's (§11, §14.13).

Bitcoin needs work V1.0 doesn't have yet:
- Keeping Bitcoin at all: about 650 GB of blocks; the chain parameters exist (`btc`), but nothing syncs it fully today.
- Bitcoin's inscriptions and runes live in **witness data**. V1.0's ord indexer reads only the scriptSig envelopes of the Dogecoin family, so Bitcoin ord and runes journals need new indexers.

### 2.4 Catalog entries

A catalog entry is `{net, family, first height, last height, manifest id, root}`. `root` is the Merkle root over the segment's raw (uncompressed) bytes, cut into **127-byte data leaves**.

**Is the manifest right?** Data is useful only if it matches the source chain:
- **The set of segments is fixed by the source chains.** A segment's bytes follow from the chain alone (a table of every height and its offset, then each block in height order), so each range has exactly one right root. **One entry per range earns.** Nobody can list data of their own making. *(Review finding 4: without this, private junk segments would draw full pay.)*
- **Segment id = `H(net ‖ family ‖ range ‖ root)`.** The sealing mask uses this id (§3.3), so a sealed copy can't be made first and its "data" worked out backward.
- **Only confirmed entries earn.** An entry is confirmed when arcades holding **more than half of the draw weight** have attested to it *(proposed)*. An arcade that keeps the chain attests only if its own root is equal; an arcade that doesn't fetches the table and 8 random blocks from a holder and checks them against the headers. Keys without weight don't count, so extra names don't help. An arcade attests to one root per range. *(Replaces "3 different arcades".)*
- **Reveal demands:** if holders of an entry won't hand out its data, an arcade keeping the chain can demand a run of its leaves on the chain. A demand names **at most 64 kB** of leaves, so the answer always fits in a block; an arcade may have one demand open at a time; answers go first in the storage section (§9). No answer in 100 blocks removes the entry. An arcade whose demand is answered loses its draw weight for one audit period, so demands against good entries aren't free. *(Second review: a demand for a whole block could ask for more than a block can carry, and so remove a correct entry.)* With the fraud proofs below, a wrong root can always be tracked down to one wrong block, even when its holders hide the data.
- **Fraud proofs** are made of leaves, not whole pieces (an 8 MiB piece wouldn't fit in a block). Anyone keeping the chain can post a run of the segment's leaves with their paths to the entry's root. Two kinds: a header in the segment that differs from the source chain's header at that height (proved at once), or a transaction that isn't in that block (the entry's holders have 100 blocks to answer with the transaction's path to the header's merkle root; no answer removes the entry). A removed entry's replicas stop earning. Every ARCADE node keeps the **headers** of every listed chain (small; V1.0 already syncs headers), so a fraud proof about blocks can be checked by anyone.

## 3. Sealed copies

### 3.1 What sealing has to stop

Chain data is public, so three cheats are open to anyone:
1. **Many names, one disk:** register the same data under many arcade keys.
2. **Keep nothing:** fetch the data from a neighbor when checked.
3. **Rebuild on demand:** keep a little, recompute the rest when checked.

A **sealed copy** is the data encoded with the owner's key. Sealing is slow, unsealing is fast, and a copy sealed under one key is useless under another. Martin's requirements (2026-11-11): **fast unsealing, one copy on disk, and no way to cheat that pays.**

### 3.2 What can and can't be promised

No sealing with fast unsealing is cheat-proof in the mathematical sense. If unsealing is fast, then someone who has the plain data (everyone can get it) can always **recompute** a sealed piece instead of storing it. Filecoin's team tried a fast-unsealing design (ZigZag) and dropped it for one with slow unsealing. Two earlier drafts of this section tried to make recomputing *too slow to finish in time*; both broke (the second by keeping small checkpoints through the sealed copy and recomputing between them).

So this design does not depend on timing at all. It depends on one claim that can be measured:

> **Every sealed piece an arcade doesn't store costs it a fixed amount of processor work each time that copy is audited, and that work costs far more than the disk space it saves. And nothing is paid until an audit is passed.**

Any cheat (many names, keep nothing, keep part, checkpoints, borrowing) comes down to "not storing some sealed pieces", so all of them pay the same price.

### 3.3 How (proposed)

- The segment's raw bytes are cut into **127-byte leaves**. Each sealed leaf is 128 bytes (0.8% larger than the data).
- **Seal:** `s_i = sloth(d_i + m_i)`, where `m_i = H(owner key ‖ segment id ‖ i)` and `sloth` is one modular square root under a fixed 1024-bit prime `p ≡ 3 (mod 4)`, with Sloth's rule for numbers that have no root so that it is a permutation (Lenstra & Wesolowski 2015). One root costs about 1,023 modular squarings.
- **Unseal:** `d_i = ±s_i² − m_i` (minus when `s_i` is odd, which is Sloth's rule run backward): **one** squaring, measured about 320 times cheaper than sealing (§3.4).
- **Each leaf stands alone.** No leaf depends on another, so:
  - sealing runs on all cores at once,
  - any single leaf can be read back alone, which is the fastest possible random read,
  - there is no chain for a cheater to shortcut with checkpoints: the only way to get `s_i` without storing it is to do the full square root again.
- The **replica root** is the Merkle root of the `s_i`.

### 3.4 Cost to arcades *(estimates; measured on the testnet)*

- **Sealing:** **1.03 core-hours per GiB**, once (measured 2026-11-11 on the GX10 with the first, unoptimized code in `crates/arcade-arc`; `cargo run --release -p arcade-arc --example sealbench`). All of Bitcoin's blocks (about 650 GB): about **3.5 days on 8 cores**, in the background. Sealed parts earn as they finish. The code must be brought close to the fastest known, so that a cheater's software isn't faster than an honest arcade's.
- **Unsealing:** **11.6 seconds per GiB** on one core (measured, same run): about 320 times faster than sealing; a 1 MiB read takes about 11 ms. The sealed copy is the only copy on disk; the arcade reads and serves from it.
- **Audits (§5):** each sealed GiB is read through once every 2 days on average, about 10–15 seconds of disk and processor per GiB. For 1 TiB that is about 1.5–2 hours of background reading a day.

### 3.5 Why cheating doesn't pay *(estimates)*

| | Cost per GiB |
|---|---|
| Storing it honestly (own disk and power) | about $0.00001 a day |
| Recomputing it for each audit on a PC (1 core-hour every 2 days) | about $0.001 a day: about 100 times more |
| Recomputing it on graphics cards (the reviewer's estimate from published figures) | about $0.0003 a day: **about 30 times more** |

The honest number is the lower one: **about 30 times**, not 100. Cheating always earns the same pay as honesty and always costs more, so it is never the cheaper way to the same coins as long as this ratio stays above 1.

Two rules keep that ratio from being cut further *(review finding 7)*:
- **Each audit releases only its own slice of pay** (§5.3). A cheater that answers one audit in three earns one third, so skipping audits saves nothing.
- **The owner key is public**, so one computing farm could answer audits for many arcades, fetching the plain data from honest ones. That is the same cheat at a larger size: the farm still pays the full recomputing cost for every GiB at every audit.

The known weak point is **special hardware**: a custom chip could do the square roots for much less energy than a PC. If someone builds one, the gap narrows. Guards:
- the audit period and the prime size are rule parameters; shortening the period or enlarging the prime widens the gap again by upgrade;
- the worst case is not free coins: a cheater with such hardware still pays for energy on every audit, and every claimed copy lowers the pay per copy, so the chain turns into "coins for computing" for that cheater. It stays secure; it would stop rewarding real storage until the upgrade.

## 4. Registering replicas

A registration takes two steps, so the registrant can't choose what gets checked:
1. **Commit:** `{segment, owner key, payout address, replica root}`.
2. **Proof:** the seed of the block **10 blocks after** the commit's block picks **16 leaves**. The proof is due within 20 blocks after that. *(Review finding 1: in the earlier draft the next block's seed made the picks and didn't depend on what blocks hold, so a writer who also wrote the next block could rewrite its own commit after seeing them. §5.1 has the rules now.)* For each, the registrant shows `s_i` with its path to the replica root and `d_i` with its path to the catalog root. Anyone squares `s_i` once and compares.

- **Owner key:** the arcade's mesh identity key. One key, one replica per segment.
- **Every verified replica earns** (§7). There is no limit on the number of copies and no order of arrival.
- **Bytes counted** = the segment's raw size, the same for every holder.
- A replica **earns** from its proof on, but each slice of pay is released only by a passed audit (§5.3).
- A replica **counts for the writer draw** (§6.1) only from **200 blocks after its first passed audit**, and stops counting the moment an audit of it fails or goes unanswered, until it passes again. *(Review finding 2: counting from the proof let someone seal on the fly, keep nothing, and hold days of block-writing weight for the price of one sealing run.)* **Rules version 4 (testnet only, ARCADE issue #11):** no wait: a replica counts from the block after the one holding its first passed audit (`params::draw_weight_wait` = 0). A replica whose first audit passed before version 4 and whose 200 blocks hadn't run out counts from version 4's first block. Mainnet's schedule has no version 4 and keeps the 200 blocks.
- **Commits aren't free to post** *(review finding 6)*: a registration commit carries a work stamp of at least about one core-minute (far less than sealing the segment). When more commits wait than a block's 200 places, **the ones with the most work go first**, so filling the places with junk gets dearer the longer it is kept up, and an honest arcade can always outbid it with a few more minutes. *(Proposed; a fee in ARCADE would shut out newcomers who have no coins yet.)*

## 5. Checks

### 5.1 Randomness, and what a block fixes

Every block carries its writer's **VRF output** and a **proof of time** (§6). The **block seed** is `H(time output ‖ VRF output)`. Block seeds drive every check in this section.

A writer must not be able to choose its seed, and must not be able to change what its block holds once later seeds are known. Three rules do that *(review finding 1: in the earlier draft a seed didn't depend on the block's contents at all, so a writer could add or rewrite a commit after seeing the challenge to it)*:

- **The proof of time for a block starts from the hash of the block before it** (the whole block: coin transactions and storage section). Change anything in a block and every later proof of time is void and has to be run again, which takes as long as it took the first time.
- **Blocks are signed.** The header holds the hash of the whole block and of the previous block and is signed with the writer's owner key. Two different signed blocks from one key on the same previous block are proof of cheating that anyone can post: that key's replicas are removed and what they have accrued is forfeited.
- **Challenges wait 10 blocks.** Any check that answers a commit takes its randomness from the seed of the block **10 after** the block that holds the commit. A cheater can't change the commit after that without redoing every block since.

**What is left, stated plainly** *(second review)*: a cheater that writes the **two blocks just before a challenge** can try different contents for the first of them, running one proof of time per try on as many cores as it has, and keep the try whose challenge suits it. An arcade storing half of a copy passes an audit this way in about 16 tries. It can do this for about one replica per block it writes that way, so on the public chain it saves little. On a secret branch, where it writes every block, it could keep about 1 TiB of unstored copies alive per 16 fast cores. The reorg limit (§6.3) keeps such a branch from ever replacing the public chain unless the cheater is also strong enough to outrun it.

### 5.2 Storage audits: the whole copy, every time

Checking a few leaves isn't enough: a cheater would recompute just those. An audit makes the arcade answer for **every leaf of the segment at once**:

1. **Pick: a sample of every arcade's replicas** (Martin 2026-11-12: with 16 MiB segments an arcade holds hundreds or thousands of replicas, too many to audit each one every period). In every audit period of 1,152 blocks (2 days) *(proposed)*, **each owner** (an arcade's key) is audited on **`AUDIT_SAMPLE` = 4 of its replicas**, drawn by bytes: draw `k` (0 to 3) picks the replica where `H(period seed ‖ owner ‖ k) mod (the owner's registered bytes)` falls, the owner's replicas laid end to end in replica-id order (the same replica twice is audited once). Each picked replica's block within the period is `H(period seed ‖ replica) mod 1,152`. The period seed is the seed of the last block of the period before, so the picks are fixed before the period starts and nobody can steer them. *(Second review: picks drawn by chance in every block let a cheater choosing among seeds keep some replicas from ever being picked; a fixed place in every period leaves nothing to choose.)* The audit's own seed is the seed of the picking block, which nobody knows before that block exists. Audits on the chain grow with the number of arcades (4 each per period), not with how much they keep.
2. **Commit.** In one of the **20 blocks** after the picking block (50 minutes), the arcade posts `A`: the Merkle root over `H(seed ‖ i ‖ s_i)` for **all** leaves of the segment. This takes one read through the sealed segment. Because the seed is fresh, `A` can't be prepared in advance.
3. **Open.** The seed of the block 10 after the one holding `A` chooses **4 leaves**. In one of the **20 blocks after that one**, the arcade shows, for each: `s_i` with its path to the replica root, its entry's path to `A`, and `d_i` with its path to the catalog root (about 10 kB in all).

To post a correct `A`, the arcade must have every `s_i` at that moment: stored, or recomputed at full cost. An arcade that posts `A` with missing leaves is found out with a chance equal to the missing share on each of the 4 openings.

The windows are 20 blocks, not 4 as in the earlier draft, and answers go first in the storage section (§9), so a full block or one unfriendly writer can't make an honest arcade miss. *(Review finding 6.)*

### 5.3 What audits decide

With sampling, an audit period decides for the **owner**, not one replica: each period covers one **slice**, what all of the owner's replicas accrued (§7) in that period.

- **Every sampled audit passed:** the slice is released for payout (if the serving rule in §5.4 allows).
- **Any sampled audit not passed** (no commit, or a commit with no valid opening in time): the whole slice is **forfeited** to the next pot, and the replica that failed stops counting for the writer draw until it passes an audit again.
- **A replica that missed can ask for a new audit at once** (the request counts as a pick), so an outage of an hour costs one slice, not two days of draw weight. *(Second review.)*
- **A replica that fails three audits in a row** is removed. It can register again.
- **Why a sample is enough:** an owner missing a share `f` of its sealed bytes passes a period only with chance `(1 − f)⁴`, and each failure costs the slice of everything it holds, not just the missing part. Keeping 90% of the data and claiming 100% loses about a third of all pay; keeping half loses about 94%.
- **Nothing is ever paid for time that isn't covered by a passed period.** Seal, register and delete earns zero.

The earlier draft released everything on any pass and took everything on one missed opening. That let a cheater skip two audits in three for free, and let congestion wipe out an honest arcade's pay. *(Review findings 6 and 7.)*

### 5.4 Serving checks

Audits prove the data is on disk. Serving checks are meant to show the arcade **hands it out**:

- For each block and each replica, every other arcade computes `VRF(its key, seed ‖ replica)`. If the result is under a threshold, that arcade is a **secret checker** for that replica; nobody else can tell until it reports. The threshold follows the checker's draw weight and gives each replica about **6 checks per audit period** *(proposed)*.
- The checker asks the replica's arcade for a seed-chosen 8 MiB piece **of plain data**, through the ordinary fetch that arcades use to borrow chains. A check looks the same as any other request, so an arcade can't serve checkers only.
- The checker compares the piece with the catalog and posts a signed verdict with its VRF proof: served in time (60 s) or not.
- **Rule:** a slice is **held back** when it has **5 or more verdicts and most say "not served"**. A held slice is released with the next passed audit whose verdicts are good; held three times in a row, it is forfeited. With fewer than 5 verdicts nothing is held.
- **Limits, stated plainly** *(review finding 6)*:
  - Nobody can prove on a chain that a file was *not* handed over, so a verdict is a report, not a proof. It catches arcades that are offline or refuse. Lying about a rival takes about half of the network's storage (the earlier two-thirds rule let a fifth of the storage block a rival about one time in ten).
  - Checkers aren't paid, so only arcades running the stock software check. A changed arcade can skip its checks. That is why missing verdicts never cost the checked arcade anything.
  - An arcade could pass a check by fetching the piece from a neighbor and passing it on. That gains it nothing: to pass audits it must hold the sealed copy anyway, and with that copy it can serve the piece itself in milliseconds.
  - This is the weakest guard in the design. The storage audits don't depend on it.

## 6. Who writes the next block, and the proof of time

Storage decides **who**, a proof of time decides **when**. The structure follows Chia's; the draw is fixed for a window of blocks as in Chia and Cardano.

### 6.1 The draw

- **One draw seed per epoch** (100 blocks). Take `H(draw seed of epoch e−1 ‖ the VRF outputs of the writers of the first 80 blocks of epoch e−1)` and run a **proof of time** from it, about 15 blocks long (as many steps as the last 15 blocks took). Its output is the draw seed of epoch `e`; the first block of the epoch carries the proof. The seed doesn't depend on what any block holds. The first epoch's draw seed comes from the genesis list and beacon (§10).
- **Why the proof of time** *(second review, its most serious finding)*: without it, someone building a secret branch with many keys could choose which of its keys writes each block, work out the next draw seed for millions of such choices in seconds, and keep one that makes its next epoch twice as fast. That would have lowered the safe limit to about a third of the storage. With the proof of time, every try costs a fast core for about 40 minutes.
- For height `h` in that epoch, each arcade computes:
  - `(output, proof) = VRF(owner key, draw seed ‖ h)` (ECVRF-EDWARDS25519-SHA512-TAI, RFC 9381: one possible output per key, checkable by anyone),
  - `u` = the output read as a number in (0, 1],
  - **score** = `−ln(u) / W`, where `W` is the arcade's verified bytes: the full size of every replica it has that counts for the draw (§4), as of the block before.
- Lower is better. Twice the storage gives twice the chance. `W` counts real sealed bytes, with no splitting, so the only way to gain weight in the draw is to keep more sealed data on disk and keep passing audits.
- **Why per epoch** *(review finding 5)*: in the earlier draft every block gave a new seed for the next draw. A writer could hold its block back to get a second try, and someone with many keys and several time calculators could try many branches and keep the luckiest. That is known to lower the safe limit from about half of the storage to about 27%. With the draw fixed for the epoch, an arcade's score for a height is the same on every branch, so there is nothing to try. What is left is the last few writers of the 80 blocks: each can choose not to publish, which changes the next draw seed, but it can't see what the change would be until the proof of time is done, 15 blocks later.
- An arcade knows its own scores for the coming epoch a few blocks ahead of it. Nobody else does until it publishes.

### 6.2 The proof of time

- A **proof of time** (a verifiable delay function, VDF) is a calculation that takes a set number of steps one after another, can't be sped up with more machines, and leaves a short proof anyone checks quickly. ARCADE uses the same kind Chia uses (repeated squaring in a class group with a Wesolowski proof), from an existing, widely used implementation rather than new code.
- The block at height `h` is valid only with a proof of time of **`score × D` steps, started from the hash of block `h−1`**. `D` is the **difficulty**.
- Every candidate for a height starts from the same block hash, so **one calculation serves everyone**: arcades that run it ("time keepers", one fast core each) release the proof for each candidate as the count reaches its number. Candidates announce their score and VRF proof first; a time keeper makes proofs only for the lowest scores it has seen, so it can't be flooded with requests. Anyone can run a time keeper, so one that refuses to serve some arcade can be replaced by any other.
- **`D` is reset every 100 blocks** so that blocks average **150 seconds**: `D` is multiplied by `15,000 s ÷ (time of this epoch's last block − time of the previous epoch's last block)`, changed by at most 4× per reset. The measured stretch runs from last block to last block with no gap. *(Review finding 8: a gap of one block between the stretches is the mistake that allows the "time-warp" trick on Bitcoin-style chains.)*
- **Timestamps:** later than the previous block's, and no more than 2 minutes ahead of the node's clock.
- There are **no fixed slots**. "50 per block" stays exact.

### 6.3 Fork choice

- **The chain with the most total difficulty wins.** Two blocks at the same height carry the same difficulty, so ties are common; then **the branch with the lower total score since the split wins**. Every node can work that out for itself, whichever block it saw first. *(Review finding 5.)*
- **Why a rewrite fails:** on any branch, difficulty adds up at a rate of (storage writing on that branch) × (speed of its time calculator). An attacker with a share `f` of the storage on a secret branch gains difficulty `f/(1−f)` times as fast as everyone else. To overtake, it needs a time calculator faster than the best honest one by more than `(1−f)/f`.
- **A secret branch gives little free weight.** On its own branch a cheater writes every block, but a block's seed depends on everything in the blocks before it (§5.1), so each try at a better challenge costs a proof of time. New replicas wait 200 blocks after their first passed audit before they count for the draw (§4).
- **Reorg limit: 100 blocks** (about 4 hours) *(proposed; both reviews advised it)*. An arcade never leaves its chain for one that splits off more than 100 blocks back. That is shorter than the 200-block wait, so nothing a cheater registers on a secret branch can count for the draw before the branch is too old to be accepted. **Rules version 4 (testnet only):** the testnet drops the wait (§4), and with it this guarantee: a replica registered and audited on a secret branch counts there at once. Mainnet keeps the wait (the `REORG_LIMIT < DRAW_WEIGHT_WAIT` test holds for mainnet at every height).
  - **Cost, stated plainly:** if the network splits in two for more than 4 hours, the halves don't rejoin by themselves; the smaller half's arcades must be told to follow the larger. And an arcade that is new, or was off for more than 4 hours, can't tell an old secret branch from the real chain by rules alone, so **each arcade release carries the hash of a recent ARCADE block** (a checkpoint), and a starting arcade also takes the chain most of its mesh neighbors are on.
- **Payments are final after 60 blocks** (2.5 hours) for the exchange and large amounts *(proposed; the first draft said 20)*. Block rewards need 100. *(Second review: writing blocks in secret costs a writer nothing but fees, so a holder of 30% of the storage would out-run 20 blocks about once in 200 tries, and 40% about once in 10. At 60 blocks, 30% succeeds about 5 times in a million.)*
- **Limits, stated plainly:**
  - The safe limit is **below half** of the storage: by the second review's estimate, **about 40%**. The testnet's cheating program is to measure it.
  - **Honest writers: the lowest score wins, not the fastest calculator** *(as built, ARCADE #11)*. A writer whose block on a parent is still in its proof of time when a rival block on that same parent arrives keeps going if its own score is lower (its branch would beat the rival's by the rule above): it made its block's contents from the parent's state when it drew, finishes the proof and publishes, and every node switches to it. It gives up once the rival is built on (one more block of difficulty than its own can have), or when its score is not lower, and it never writes a second block on one parent (§5.1). Meanwhile it draws on the rival as well, so it doesn't lose the next height. So among honest writers a slower time calculator delays a block but doesn't lose it, as long as it finishes before the next block lands on the rival.
  - **Against a secret branch** the speed still counts: whoever has the **fastest time calculator** multiplies its storage by its lead, twice as fast counting as twice the storage (above). The network needs at least one honest time keeper close to the fastest hardware that exists.
  - An arcade that sees it has the lowest score for several heights in a row can build those blocks in secret and publish them late, replacing the blocks others wrote meanwhile. A share `f` gets `k` in a row with chance `f` to the power `k`; this is why payments wait 60 blocks.
  - **A small network is cheap to outweigh, whatever the rules.** If all honest arcades together hold 20 TiB, a 24 TB disk and a few hundred dollars of computing to seal it is more storage than everyone else. Every new chain has this stage; the guard is many arcades sealing before genesis, and release checkpoints while the network is small *(proposed)*.

## 7. Rewards

*As built (§14.19, §14.20, §14.26; for operators [EARN.md](EARN.md)):* the pot is shared every epoch but **accrued**, and each earner is **paid at its own turn**, one block in each audit period, released by its audits. The first draft's "one coinbase at each epoch's start pays everyone" is not what runs on the testnet or will run on the mainnet.

- **Epoch = 100 blocks** (about 4 hours 10 minutes). **Epoch pot** = 49 × 100 = 4,900 ARCADE (50 a block, less the writer's 1 from rules version 2, §14.26), plus every fee and anything forfeited or left over.
- **Every copy earns; each piece's share is split among its holders.** In each block, a segment of `B` bytes held by `n` registered replicas gives each of them the weight `B / n`. The pot is split in proportion to weight.
  - Every held segment gets the same pay per byte, however many arcades hold it.
  - A holder of data that few others keep gets a large part of that segment's pay; a holder of data everyone keeps gets a small part. That is what pulls disks toward data the network lacks.
  - Registering the same segment under several keys takes a separately sealed copy for each, so it buys a larger part only with real disk.
- **Accrued, then released by audits.** An epoch's share is **accrued** to its earner: a genesis holder's payout address, or an owner (an arcade's key, for all its replicas together). An owner's audits run from one of its turns to the next (§14.20): all passed, what it accrued is paid at the turn; one not passed, it goes back to the pot (§5.3); no audit yet (registered after its last turn), it waits for a period an audit covers. The serving rule can hold it back (§5.4).
- **Paid at each earner's turn**, `H(earner) mod 1,152` (`registry::turn`): the coinbase of that block of every audit period pays it, in one output, everything released. Genesis earners need no audit. Amounts under 0.01 ARCADE wait for a later turn.
- **The writer** gets 1 ARCADE of the 50 from rules version 2 (§14.26; the testnet from block 0 since its 2026-11-15 restart, the mainnet from block 0); fees join the pot (§14.18).
- **Coinbase maturity:** 100 blocks.

## 8. Coins, addresses and fees

| | Mainnet (proposed) | Testnet (proposed) |
|---|---|---|
| V1.0 key / net | `arc` / `arc` | `arctest` / `arctest` |
| Store chain number | 1000 | 1001 |
| P2PKH version byte | 0x17 (addresses start with **A**) | 0x53 (start with **a**) |
| P2SH version byte | 0x1c (start with **C**) | 0x57 (start with **c**) |
| WIF version | 0x97 | 0xd3 |
| P2P / mesh | the arcade mesh only (no separate P2P network) | same |
| Transaction format | Dogecoin's (legacy, no SegWit), SIGHASH as in V1.0's `legacy.rs` | same |
| Fee floor | 0.001 ARCADE per kB | same |
| Dust | 0.001 ARCADE | same |
| Max block size | 1 MB of coin transactions + 4 MB of storage section (§14.19; was 1 MB) | same |

- ARCADE blocks and transactions travel over the existing mesh (new frame types).
- **The coin exchange** (Martin, 2026-11-11): ARCADE is listed on the arcade's exchange like the other coins. Its transactions use the same format and signature rules as the Dogecoin family, so the existing swap trades apply without a new trade type.

## 9. Storage section limits per block (proposed)

Items go into the section in this order, so that answers with a deadline are never crowded out by new requests:

| Order | Item | Size each | Per block | Who may post it |
|---|---|---|---|---|
| 1 | Audit openings (4 leaves); answers to reveal demands and fraud proofs | about 10 kB; up to 64 kB | what fits | the picked replica, once per pick; a holder of the entry |
| 2 | Audit commits | about 120 bytes | no separate limit | the picked replica, once per pick |
| 3 | Registration proofs (16 leaves) | about 30 kB | 8 | the committer, once per commit |
| 4 | Serving verdicts | about 220 bytes | 1,000 (§14.19) | a checker, with its VRF proof |
| 5 | Registration commits (batches, §14.19) | 134 bytes and 64 a replica | 200, most work first | anyone, with a work stamp (§4) |
| 6 | Catalog entries, attestations, reveal demands, fraud proofs | 100 bytes to a few kB | 64 | arcades with draw weight |

- Nothing in the section can be posted without limit by someone who holds no storage, except registration commits, which cost work. *(Review finding 6.)*
- **Registrations:** 8 proofs a block is about 4,600 segments (4.5 TiB) a day.
- **Scaling limit:** with every replica audited every 2 days, the openings fill the storage section at about **100 TiB** of sealed storage on the network, and the verdict limit is reached at about 75 TiB. **The audit period stretches by rule, not by a writer's choice:** it is 1,152 blocks, or as many more as it takes for a block's scheduled openings to fill no more than three quarters of the storage section. *(Second review: otherwise about 50 TiB of real storage, a few thousand dollars, would overfill the section and let block writers decide whose openings get in.)* A longer period shrinks the gap in §3.5, so before the network reaches about 50 TiB, openings must be made smaller (combined proofs) by upgrade. The chain reports the current audit period so this can't arrive unseen.

## 10. Genesis

- **Block 0** is dated 2027-01-01 00:00:00 UTC. It holds no coins.
- **The first-writer problem:** on a chain where storage decides who writes, whoever registers first has all the weight and could shut everyone else out. So the chain doesn't start empty. Arcades **seal before genesis** (sealing needs only the owner key and the segment), and the first replicas are collected in a public list.
- **The list is closed before anyone can know what will be checked** *(review finding 3: the earlier draft had no seed to pick the leaves before block 0, and a list open to the last minute could be flooded or tuned)*:

| When (UTC) | What |
|---|---|
| 2026-12-21 to 12-27 | Arcades send their registration commits over the mesh, each with a work stamp. The list is public as it grows. |
| 2026-12-28 00:00 | **The list closes.** Its hash is published. |
| 2026-12-28 12:00 | **Beacon 1**: the hash of the first Bitcoin block with a header time at or after this moment, taken once it is 6 blocks deep. Nobody can know it when the list closes. With the list's hash it picks 16 leaves for every listed replica and seeds a full audit of each (§5.2). |
| by 2026-12-29 12:00 | Every listed replica posts its 16-leaf proof and its audit commit `A`. |
| 2026-12-30 00:00 | **Beacon 2**, taken the same way, picks the 4 leaves each replica must open. |
| by 2026-12-30 24:00 | Openings posted. Replicas without a valid proof and a passed audit are struck from the list. |
| 2026-12-31 | The final list is public for a day. Any arcade can check every proof on it and see that its own replicas are there. |

- **Block 0 carries the hash of the final list and both beacons.** Every replica on it has passed one full audit, so it counts for the draw from block 1. The first epoch's draw seed is `H(hash of the closed list of 12-28 ‖ beacon 2)`. *(Second review: the final list's hash would let anyone with several replicas steer the seed by leaving some openings out.)*
- **Before block 0 there is no chain**, so commits, proofs, openings and fraud proofs are published on a public bulletin over the mesh, kept by the arcade that assembles the list and copied by any arcade that wants to. The assembler signs a receipt for every commit it takes in. A receipt for a commit that is missing from the list proves it was left out, and the list is corrected on 12-31. *(Second review: one assembler could have dropped rivals with no remedy.)*
- **What fake weight costs at genesis:** every listed copy must be sealed for real and held through the audit, about $20–50 of rented computing per TiB by the second review's estimate. If it is then deleted, it stops counting at its next audit, within 2 days.
- **The list also holds the catalog entries** the replicas point to. Segments follow from the source chains (§2.4), so every arcade keeping a chain works out the same roots. Where two roots are listed for one range, the fraud proofs of §2.4 settle which is wrong before the list is final.
- **Every genesis replica is audited again within the first 2 days**, as in every later period (§5.2).
- No coins exist in block 0; the list holds storage only. The first 50 ARCADE is minted by block 1.
- **Testnet** (mid-December): the same rules and the same genesis procedure with `arctest`, with throwaway coins and the timetable shortened from days to hours. Sealing is **not** made faster on the testnet: the numbers in §3 are what it has to measure.

## 11. Risks and what the testnet must show

| Risk | Guard | Still open |
|---|---|---|
| Recomputing sealed pieces instead of storing them | whole-copy audits every 2 days; each missing piece costs a full square root per audit; each audit releases only its own slice of pay (§3, §5) | the gap is about 30 times on graphics cards and less on special hardware (§3.5); the figures are estimates until measured |
| Many names on one disk | each key's copy is sealed separately and audited separately | none known |
| Seal, register, delete | pay is released only by a later audit; draw weight only after a first passed audit (§4) | none known |
| Changing a commit after seeing its challenge | proof of time starts from the previous block's hash; signed blocks; challenges wait 10 blocks (§5.1) | a writer of two blocks in a row can try several challenges, one proof of time per try; small on the public chain |
| Rewriting history | proof of time; most total difficulty, lower score on ties; 100-block reorg limit; payments final after 60 blocks (§6.3) | safe limit about 40% of the storage (estimate); a faster time calculator multiplies its owner's storage; block writers earn only fees, so secret branches cost them nothing |
| Trying many draws | draw fixed per epoch; the draw seed takes a proof of time (§6.1) | each try costs a fast core for about 40 minutes |
| A small network is outweighed | many arcades sealing before genesis; release checkpoints | true of every new chain; grows safer only with size |
| First arcade captures the chain | genesis list closed before the beacon; every genesis replica passes a full audit before block 0; signed receipts (§10) | the list is assembled by one arcade; receipts and public checking are the guard |
| Keeping data but not serving it | secret serving checks (§5.4) | verdicts are reports, not proofs; checkers are unpaid; the weakest guard |
| Wrong or made-up catalog entry | segments fixed by the source chain, one per range; id includes the data root; confirmed by half the draw weight; reveal demands; fraud proofs (§2.4) | index journals need fraud proofs of their own; so do witness, merged-mining and MWEB data, which a header check can't reach |
| A block writer leaves out a rival's answers | 20-block windows; answers go first in the section; a miss costs one slice; re-audit on request | needs about 80% of the storage to matter (second review) |
| Flooding the storage section | every item is tied to a pick, a commit, a VRF draw, draw weight or a work stamp; commits ranked by work; the audit period stretches by rule (§9) | none known |
| Difficulty and timestamp tricks | stretch measured last block to last block; timestamps must rise; 4× clamp (§6.2) | none known |
| The network outgrows on-chain audits | limit known in advance (§9) | needs combined proofs before about 50 TiB |
| Seven weeks to a fixed genesis | build order below; the review and the cheating program run from the start, not at the end | a cheat found late is closed by a rule upgrade after genesis, and is published until then |

**Review:** an independent reviewer attacked the 2026-11-11 draft and found eight groups of flaws; the fixes are marked *(review finding n)*. A second reviewer then attacked those fixes and found nine more points; those fixes are marked *(second review)*. Neither review looked at attacks on the mesh itself (cutting an arcade off from honest neighbors) or at groups of serving checkers working together.

**Byte-level rules still to be written** (the second review listed them; two programmers could otherwise build chains that disagree): the exact arithmetic of the score (fixed-point logarithm, rounding, the sizes of `u` and `W`); total difficulty as the plain sum of `D`; the pick and schedule functions; every deadline as block numbers; the exact bytes of a segment for each chain, including witness, merged-mining and MWEB data. They are written down alongside the code in §14, which grows with each build step and is reviewed before the testnet. The testnet includes a **cheating arcade** program that tries each cheat in the table for real. **Genesis is fixed at 2027-01-01** (Martin, 2026-11-11). A cheat still open on that day is listed publicly and closed by upgrade.

## 12. Build order (each a PR)

| # | Work |
|---|---|
| 1 | Chain basics in V1.0: params, signed block format with a storage section, coin txs, block store and validation, mesh frames, wallet + Explorer + Chains & Coins entries |
| 2 | Segments: blocks family as fixed 16 MiB segments, 127-byte-leaf Merkle roots, catalog entries, attestations, reveal demands, fraud proofs |
| 3 | Sealing (§3): 1024-bit Sloth, all-core background sealing, read and serve from the sealed copy, measured costs |
| 4 | Registration (work stamp, commit, then proof) and draw weight (§4) |
| 5 | Audits: pick, commit, open; slices accrued, released and forfeited (§5.2, §5.3, §7) |
| 6 | Serving checks (§5.4) |
| 7 | The per-epoch draw, the proof of time, time keepers, difficulty, fork choice (§6) |
| 8 | Genesis list with beacons (§10) and the "Earn ARCADE" page |
| 9 | The cheating arcade program and the public testnet |
| 10 | Exchange listing |
| later | Index journals for the Dogecoin family; Bitcoin sync, witness ord and runes indexers and their journals |

Everything in 1–9 is in the testnet from its first day. At launch the paid data is the **blocks** of every listed chain; index journals and Bitcoin's indexes join by upgrade.

## 13. Open questions for Martin, as settled for the build (§15 step 14)

Decided: **genesis stays 2027-01-01** (Martin, 2026-11-11). The rest are written into the rules as below: Martin's
answers where he gave them, otherwise the defaults of §15.1 (each can still change by a rules version before mainnet).

1. **Pay delay:** pay is released by passed audits, so it arrives about 2 days after it is earned. *Default: as built.*
2. **What is stored at launch:** blocks, the coins family and the index journals (ord, ledger, profiles, messages,
   names, tokens, maps, tags) and blobs, all from genesis; Bitcoin's indexes by upgrade. *Default.* Journals are
   confirmed by more than half of the draw weight; ord rows of one-piece inscriptions, coins records and block headers
   and transactions also have fraud proofs (§15.4).
3. **Time keepers** aren't paid (as in Chia). *Default.*
4. **Address prefix:** **A…** for mainnet (p2pkh 0x17). *Default, set in params.*
5. **A share of the 50 for the block writer:** 1 ARCADE per block to its writer, 49 to the storage pot, fees into the
   pot. *Martin 2026-11-14* (rules version 2, §14.26).
6. **BIP44 coin type:** the home chain's (1 on test chains, 3 on mainnet), so an @name's ARCADE address is its claim's
   key hash. *Default, relied on since v0.1.71.*
7. **Small fees:** moot since §14.18: every fee joins the pot, none is paid out to a writer.

## 14. The rules as built

What the code in `crates/arcade-arc` does today, to the byte. Where this section and the code disagree, that is a bug in one of them. The subsections were written as each part was built, so a later one can replace an earlier one: registration (§14.15, §14.19), pay at each earner's turn (§14.19), audits (§14.20), serving checks (§14.18, §14.19), the writer's share (§14.26) and the real proof of time (§14.22 item 4, §14.23) all run on the public testnet. [EARN.md](EARN.md) puts the pay rules together for operators.

### 14.1 Hashes, keys and networks

- `H(label; parts)` = `SHA-256(net tag ‖ "/" ‖ label ‖ 0x00 ‖ parts joined)`. Net tags: `ARCADE/arc` (mainnet), `ARCADE/arctest` (testnet). Every part has a fixed length for its label, or there is only one part of free length and it comes last.
- A signature by an owner key is ed25519 over `net tag ‖ "/" ‖ label ‖ 0x00 ‖ parts`.
- An arcade's **owner key** is its mesh key (ed25519). The same key makes its writer draws (ECVRF-EDWARDS25519-SHA512-TAI, RFC 9381; only the one canonical proof encoding is accepted).
- Coins are locked by secp256k1 keys, as on Dogecoin. Testnet addresses: key hash version `0x53`, script hash `0x57`.
- Numbers are little-endian unless said otherwise. Trees are RFC 6962 Merkle trees (leaf prefix `0x00`, node prefix `0x01`); an empty tree's root is 32 zero bytes.

### 14.2 The block header

Fields in order: version `u32` = 1 · height `u64` · id of the block before (32) · time `u64` (seconds since 1970) · transaction root (32) · storage root (32) · difficulty `D` `u128` · writer's owner key (32) · claimed draw weight `u64` (bytes) · VRF proof (80) · the block's proof of time (output and proof, each with its length in front, at most 512 bytes each) · one byte 0 or 1, and after a 1 the draw seed's proof of time · signature (64).

- **Block id** = double SHA-256 of everything but the signature. The writer signs `("block"; id)`.
- Because the id doesn't cover the signature, and covers the body only through the two roots, an arcade **never remembers an id as bad** for a wrong signature or for a body that doesn't match the roots: anyone can put those around a good header. An id is remembered as bad only for what the id covers.
- Transaction root: tree over the transaction ids. Storage root: tree over the storage items' bytes, each starting with its tag. Items are ordered by group (`tag >> 4`), groups never going down: `0x10` audit opening, `0x20` audit commit, `0x21` re-audit, `0x30` registration proof, `0x40` verdict, `0x50` registration commit, `0x60` catalog entry, `0x61` attestation, `0x64` equivocation proof. An unknown tag makes the block invalid.

### 14.3 Block 0

Block 0 is six numbers: time, hash of the closed list, hash of the final list, beacon 1, beacon 2, first difficulty. Its id is `H("block 0"; those six in that order)`. The first epoch's draw seed is `H("draw seed 0"; closed list ‖ beacon 2)`: the final list can't steer it. Block 0's own seed is `H("seed 0"; id)`. It has no writer and no transactions.

### 14.4 The draw and the proof of time

```text
draw input   = "draw" message of (the epoch's draw seed ‖ height)       signed by the VRF
x            = the first 8 bytes of the VRF output, big-endian
L            = −log2((x + 1) / 2^64), rounded up to a multiple of 2^−32
score        = floor(L × 2^96 / W)                                      W = draw weight in bytes
steps        = floor(score × D / 2^96), at least 1
block seed   = H("seed"; VRF output ‖ proof-of-time output)
```

- The logarithm is counted in base 2, not base e: every score is multiplied by the same constant, which `D` absorbs. No floating point anywhere.
- A block's proof of time runs `steps` steps from `H("time"; id of the block before)`. Every candidate for a height starts at the same place.
- **The next epoch's draw seed.** The proof starts from `H("draw seed time"; this epoch's draw seed ‖ the VRF outputs of the writers of the epoch's first 80 blocks, in order)` (79 in the first epoch: block 0 has no writer) and runs for as many steps as the epoch's blocks 66 to 80 took together. The seed is `H("draw seed"; the proof's output)`. The first block of the next epoch carries the proof; no other block may carry one. From rules version 4 (ARCADE issue #20) the proof may come in up to 8 runs one after another, in the same bytes as a block's own proof (§14.23), so the arcade makes it in pieces saved under `arc/<net>/seed-pieces` and a restart carries on instead of starting over; the output, and so the seed, is the one a single run gives. Below version 4 it is one run.
- **Difficulty** changes with the first block of each epoch: `D × target ÷ actual`, where *actual* is the time from the last block of the epoch before the last one to the last block of the last one (from block 0 for the first epoch), *target* is 150 s for each block between, and *actual* is held between a quarter and four times the target. No block is left out between two measured stretches, so there is no gap to hide time in.

### 14.5 A block against the chain

A header is checked against the headers before it, cheapest first: the block before is known and not bad · height is one more · it is the release's block where the release names one · time is later than the block before · time is at most 120 s ahead of the arcade's clock (a block that fails only this is tried again later) · `D` is what §14.4 gives · the claimed weight isn't zero and isn't more than the key could have on any branch · the draw seed's proof (first block of an epoch) · the VRF proof · the proof of time, of exactly the steps the claimed weight gives.

The body is checked when the block joins the branch the arcade is on: the claimed weight **is** the writer's draw weight there · the coinbase starts with exactly the outputs owed · every transaction · every storage item.

*Why the weight bound is a header rule.* The exact weight depends on the branch, so it can only be checked when the block joins. Without a cheap bound, anyone could sign 100 headers that claim enormous weight (so need one step of proof each) and make every arcade undo 100 blocks to find out they are worthless. Weight is gained only 200 blocks after an audit, further back than any branch may split off, so the most a key can have is the same on every branch an arcade could move to.

### 14.6 Which branch

1. More total difficulty (the plain sum of `D` from block 1).
2. Equal: the lower total of the blocks' scores.
3. Still equal: the lower id of the last block.

Every arcade gets the same answer whichever block reached it first. An arcade doesn't take in a block whose branch splits off more than 100 blocks behind its tip. A block that fails a body check is remembered as bad with everything built on it, and the arcade goes back to exactly where it was. Two blocks by one writer on the same block are kept as proof against it (§5.1).

### 14.7 Coins

- A transaction is Dogecoin's legacy format, version 1, at most 100,000 bytes, written the one way its fields can be written.
- Every spendable output holds at least **0.001 ARCADE** (a chain rule, not a relay habit). An output starting with `OP_RETURN` burns what it holds.
- A coinbase's one input carries the height (8 bytes), the id of the block before (32) and up to 60 free bytes, so no two coinbases share an id on any chain or fork. Its outputs are spendable 100 blocks on.
- **A coinbase starts with exactly the payouts owed**, in order, then (from rules version 2) the writer's 1 ARCADE (§14.26); a zero `OP_RETURN` may follow. The block's fees join the pot (§14.18).
- A lock time is a block height. At most 8,000 signature checks per block.
- **Script** is a strict part of Bitcoin's, enough for what V1.0 signs: pay to a key hash, pay to a script hash, the exchange's pre-signed trades, the prize pool, bare multisig, a hash lock and a height lock. An opcode outside the list makes a script invalid even in a branch that isn't run; pushes are written the shortest way; signatures are strict DER with low S; hash types are ALL, ALL|ANYONECANPAY and SINGLE|ANYONECANPAY; a signature covers the whole locking (or redeem) script. New opcodes come by upgrade.
- **Payouts** (§7, §14.19): at each epoch's end (heights that are multiples of 100) the pot is shared by weight and accrued; a block's coinbase pays every earner whose turn that block is (`H(earner) mod 1,152`), one output per payout address in address order, when its audits released it (§14.20) and it is at least 0.01 ARCADE. *(Until registration was built, the first coinbase of an epoch paid the whole pot; that is gone.)*

### 14.8 Stand-ins (local test chains only)

Two parts of the chain began as stand-ins so the rest could be built and tested. **Neither runs on the public testnet or the mainnet any more:**

- **Proof of time:** the stand-in is one hash, made in no time. It is left only for quick local test chains (`--arc-local` settings without `"time": "vdf"`), and the code refuses to start a mainnet chain on it. The public testnet runs the real one (`"time": "vdf"` in `arctest.json`): Chia's class-group VDF with a Wesolowski proof (§14.22 item 4), served by time keepers (§14.23).
- **Storage records:** the fixed list of holders at block 0 was replaced by registration (§14.15, §14.19), audits (§14.20) and serving checks. What stays fixed at block 0 is the genesis earners' weight (the testnet's two genesis writers, 1 GiB each; on the mainnet the audited genesis list, §14.24).

### 14.9 In the arcade

- **One copy on disk: the chain store's** (§14.17, built 2026-11-11). The best branch is kept in the chain store, each block whole: its transactions as the store keeps any coin's, and the rest (the ARCADE header and the storage section) as the stored block's extra bytes (`store::stored_block`, `Block::encode_rest`). At start the branch is read back from the store (`store::unstored`) and **every check is run again**, so a damaged store can leave blocks out, never add one the rules refuse. Side blocks are kept in memory only and come again from other arcades after a restart. (The earlier block log, `<home>/arc/<net>/blocks.log`, is read once by a newer arcade, the store's copy is made again whole, and the log is deleted. Checking everything again takes longer as the chain grows; a saved state at a checkpoint comes before the real proof of time, whose checks are slow.)
- **The copy in the chain store.** Wallets, the Explorer and the address index read chains from the arcade's chain store, so the branch the arcade is on is copied there (chain numbers 1000 `arc` and 1001 `arctest`, far from the coins' own, which count up from 1). The store's 80-byte header stands in for the ARCADE header: the block before, the time, and the ARCADE block's id carried in the merkle root field. Transactions are copied as they are. Nothing is ever read back from the copy to decide what is valid, and after any stop the copy is brought back in step with the rules' branch, or made again whole.
- **An optional install, as every coin but the home chain is** (Martin, 2026-11-11). Nothing of the chain runs until the operator installs it in Chains & Coins (`chains.add`); from then on it starts with the arcade. `chains.remove` stops it and deletes its copy in the chain store and its sealed data, in the background; while that runs it can't be installed again. Removing it loses no coins: they are on the chain, and the wallet keeps its keys. A block being taken in at the moment of removal is either in before the removal or never.
- **Not in the list of coins.** The chain has no coin P2P network, no DNS seeds and no node to borrow, so it isn't in the arcade's list of coins and can't be lent or borrowed, queued, paused or given a storage mode or indexes: its blocks come over the arcade mesh and it is kept whole. Everything that walks the list of coins (chain sharing, peer lists, imports) leaves it alone. The wallet, the Explorer and Chains & Coins reach it through entries of its own (§14.11).
- **Blocks over the mesh** (frames of kind `ac`; every frame names block 0's id, and an arcade answers only for the chain it runs):

  ```text
  tip  {id, h, w}          my best block: id, height, total difficulty. Sent on each link when it changes.
  get  {id: n, loc}        send me your branch after the newest of these blocks that is on it
                           (first the side branch your last answer brought me, from its newest
                           block back to my best chain, thinning out; then my last three blocks,
                           then twice as far back each time; block 0 last; at most 32)
  a    {id: n, seq, end, more} + bytes     up to 50 blocks or 4 MB, oldest first, each with its
                           length in front, cut into frames of at most 60,000 bytes
  ```

  An arcade that hears of a tip it doesn't have asks for it; an answer that brought something is followed by the next ask at once, **going on from the last block that answer brought** (as Bitcoin's `getheaders` goes on from the last header it got). Before 2026-11-15 every ask started from the asker's own best chain, so a branch more than one answer (50 blocks) past the fork was never fetched past its first 50 blocks while it was still the lighter branch here: the testnet split for good that day after the GX10 lost its network for 5½ hours and both writers wrote alone (forked at 126; GX10 to 171, JB20B to 175; the GX10 got the JB20B's branch to 157 and no further). An arcade that named a tip and whose answer brought nothing toward it is asked again only after 10 seconds; one whose blocks another arcade delivered first is not held back. Asks from one arcade are answered at most twice a second: one that comes sooner waits its turn (the newest ask replaces an older one still waiting).
- **The writer.** On each new tip the arcade draws, waits as long as its draw's proof of time takes, and writes one block whose coinbase pays what is owed and which carries the waiting transactions that pay best (§14.10). It never writes a second block on the same block: one the chain doesn't take yet (its time ahead of the clock) is offered again as it is.
- **The local test chain.** `arcaded --arc-local settings.json` tells the arcade of a chain on the two stand-ins of §14.8, for arcades on a closed test network; it is then there to install. The settings give block 0, the holders (arcade id, bytes, payout address) how many steps a second the stand-in proof of time counts, and, if wanted, a smaller segment size (`segment_bytes`; a closed network's chains may never fill 16 MiB). Every arcade of one local chain must be given the same settings. An arcade started without the option knows no ARCADE chain and shows none. (The public testnet's chain will be known to every arcade without an option.)

### 14.10 Waiting transactions

What an arcade keeps waiting, passes on and puts in its blocks is **its own choice, not a chain rule**: a block is valid whatever its writer left out. These are the rules every arcade ships with.

- **What waits.** A transaction that would be valid in the next block, counting the coins of transactions already waiting. It pays at least **0.001 ARCADE per 1,000 bytes**.
- **The first spend stays.** A transaction that spends a coin a waiting one already spends is refused. There is no replacing by a higher fee: an arcade that showed a payment as on its way doesn't swap it for another.
- **Rows.** A waiting transaction may spend another's coins, up to 25 in a row.
- **Room.** 5 MB. A full pool takes a newcomer only if it pays more per byte than the cheapest waiting ones, which leave with everything that spends them; a transaction never pushes out one it needs.
- **Checked once.** Scripts and signatures are run when a transaction arrives. After each new block the arcade only looks its coins up again: what the block held or made impossible leaves, the rest stays in the order it came. When the chain changes branch, the transactions of the blocks taken back are checked again in full and wait again.
- **Into a block.** The writer takes the best paying per byte, a transaction never before one it spends from, up to 1 MB and 8,000 signature checks. Its coinbase holds only the payouts owed; the fees join the pot (§14.18). The block's transactions go in the order its seed sets, not the order the writer picked them.
- **Not kept.** The pool isn't saved: after a restart a wallet offers its transaction again.
- **Over the mesh** (more frames of kind `ac`):

  ```text
  inv  + bytes                      ids (32 bytes each, at most 1,000) of transactions that
                                    came to wait here since I last told you
  gtx  {id: n} + bytes              send me these (at most 100 ids)
  txs  {id: n, seq, end, left} + bytes    the ones I still have, each as it is, up to 4 MB,
                                    cut into frames; `left` counts ids at the end of the
                                    ask that didn't fit
  ```

  An arcade asks each link for one lot at a time and only for what it doesn't have. A transaction it refused isn't asked for again until the next block. One whose coins it doesn't know isn't remembered as refused: the transaction that makes them may still be on its way.
- **From a wallet.** The local commands `arc.submit {hex}` (answers the id and the fee) and `arc.pool` (what waits here). `arc.status` counts the waiting; `arc.block` with `raw` gives a block's transactions as they are.
- **Refused a moment ago.** A transaction an arcade refused is answered "refused a moment ago" if it is offered again before the next block, without being checked a second time.

### 14.11 In the wallet, the Explorer and Chains & Coins

An arcade that has the chain installed shows it beside its coins, read from the copy in its chain store (§14.9). An arcade that hasn't can borrow it, as any coin (§14.14); with neither, there is no chain of that name for its wallet and Explorer.

- **Chains & Coins.** One row, as a coin's: "Not installed" with the **Borrow** switch and **Install…**, or "Installed · block N" with **Manage…**, whose card shows where the chain is and **Stop and remove**. The install card has no storage modes or indexes to pick (§14.9).
- **Icons.** The arcade cabinet of the DogecoinArcade beta: **purple for ARCADE testnet, green for ARCADE** (Martin, 2026-11-11).
- **The wallet.** The arcade's 12 words give its ARCADE addresses as they give every coin's. The balance and the coins to spend come from the address index of the copy. A send is built and signed as for any coin, then handed to the arcade's own pool (§14.10) and passed on over the mesh from there. It never picks a coin a waiting transaction already spends, nor a block reward younger than 100 blocks. Fees: at least 0.001 ARCADE per 1,000 bytes, twice that when the wallet picks; no output under 0.001 ARCADE.
- **The Explorer.** Blocks, transactions, addresses and the waiting transactions, in the chain picker like a coin. A block's page names its writer, its difficulty and the steps of its proof of time, where a coin's shows bits, nonce and merkle root. Answers come from this arcade's own copy; a borrowed copy asks its lenders for what it doesn't keep (§14.14).
- **Still to come:** amounts on their way to a wallet before their block (`pending`), imported and linked wallets, and the public site's API.
- **Test chain keys.** The test chain's addresses come from the same derivation path as the other test chains' (coin type 1), so a wallet's test ARCADE address has the same key as its test Pepecoin one. The real chain's coin type is an open question (§13).

### 14.12 Segments of blocks

Everything here follows from the source chain alone, so every arcade that keeps the chain makes the same segments, and exactly one root is right for each.

- **Where a chain is cut.** Ranges are 10,000 heights (`RANGE`), from height 0. A segment starts where the one before ended and takes whole ranges until **its blocks** hold at least 16 MiB (`SEGMENT_BYTES`; the table below isn't counted). Ranges left at the end wait. A segment never changes once cut. A local test chain may set a smaller size in its settings; it is part of that chain's rules.
- **A segment's bytes**, for heights `first..=last`: a **table** with one `u64` (little-endian) per height, saying where that block starts, counted in bytes from the start of the segment (the first block starts right after the table); then **each block as its chain sends it** (header, then transactions), in height order. What "as its chain sends it" means for witness, merged-mining and MWEB data is still to be written per chain.
- **The root** is the Merkle root over those bytes cut into 127-byte leaves, the last filled up with zeros. A leaf's hash and a node's hash are tagged apart, so a node can't be passed off as a leaf.
- **A run of leaves** with one path proves any stretch of a segment against its root: the leaves, then the hashes that complete the tree from that run. This is what reveal answers and fraud proofs will carry.
- **A segment that is too long is provable from one table row:** if the block bytes before the first block of the segment's last range already reach the full size, the segment should have ended earlier.

### 14.13 The catalog

Which segments exist, as every arcade agrees on them. Kept by the blocks: catalog items are in a block's storage section (at most 64 per block), and a branch change takes them back block by block.

- **Listed chains** (`params.rs`, as §2.3). Mainnet: `arc`, `pep`, `peptest`, `doge`, `dgb`, `flop`, `wjk`, `kot`, `ltc`, `btc`. Testnet: `arctest` and the same coins, plus the two other test chains V1.0 follows, `dgbtest` and `ltctest`, so the testnet has small chains to work on. `shic`, `bel`, `lky` and `jkc` are not listed on either.
- **Families** (`segment::FAMILIES`): blocks (0); the index journals ord (1), ledger (2), profiles (3), messages (4), names (5), tokens (6), maps (7), tags (8); the coins each block spends (9); the ARCADE chain's blobs (10). An arcade cuts and offers an index family only when its journals run from the index's start (`arcseg::families`). Indexes without a journal family (addresses, collections) are never listed or paid. **Rules version 4 (testnet, ARCADE issue #6):** dogefeed's journals are family 12, taken by the catalog only in blocks where `params::rules_v4` holds (`segment::family_at`; below it, and on mainnet, an entry of family 12 is refused as an unknown family is). Number 11 is kept for collections, which stays unlisted: it is read by a background pass outside the block's commit, from transaction bodies an arcade may not hold, so its tables aren't the same on every arcade.
- **Rows of spans.** For each family of each listed chain the catalog holds a row of spans from height 0 on, each starting where the one before ended. A span holds one confirmed segment, or nothing where its segment was proved wrong and removed.
- **An entry** is `{chain, family, first, last, bytes, manifest, root, poster, signature}`. Its **segment id** is the tagged hash of `chain, family, first, last, bytes, root`. An entry is taken into a block only if, in this order: its chain is listed; its family exists; `first..=last` are whole ranges; `bytes` is at least the table plus a full segment; its signature is its poster's; its poster has draw weight; its id was never removed; it isn't waiting already; its span is open (the one after the row's last, or a removed entry's with the same first and last); and its poster doesn't stand behind another entry for the same first height.
- **Offering is attesting.** An **attestation** `{segment id, attester, signature}` is taken if the entry waits, the signature is the attester's, the attester has draw weight, and it stands behind no entry for that first height yet. A key stands behind one entry per first height at a time.
- **Confirmed** in the block that brings the weight behind an entry over **half of all draw weight** (twice the weight behind it is more than the total), counted with the weights of that block. It is checked when an item touches the entry, not when weights change by themselves. From rules version 3, a blocks or coins entry is confirmed with more than a tenth, and replicas of it register only after its 100-block challenge window (§15.4, step 11). **Rules version 4 (testnet only, ARCADE issue #14):** every family's entries, the index families too, are confirmed with more than a tenth, with the same challenge window when less than half stands behind them; on today's testnet (two genesis writers of 1 GiB each) one writer alone lists an index segment, as intended. Mainnet has no version 4: its index families need more than half. The other entries offered for its span are dropped with their attestations.
- **Removed** entries (step 2c) leave their span open for another entry with the same first and last. A removed id is never taken again.
- **The manifest id is a hint for fetching, not a rule.** It isn't in the segment id, and attesters don't vouch for it: compressed pieces can differ between versions of the compression library while the raw bytes are the same. The first poster's manifest stands. *(This narrows §2.1's "the manifest id is the same everywhere": the root is.)*
- **What the catalog can't check** is whether a root is the right one. That is what the attesters say, and what reveal demands and fraud proofs settle when they lie (step 2c).

**Open points found while building step 2**, to settle before 2c:

1. A fraud answer "this transaction is in that block" can be larger than the 64 kB a block gives it: transactions can be bigger than that. The answer needs a form that fits (a hash of the transaction with its path, checked against the leaves).
2. Bitcoin's and Litecoin's witness data isn't covered by the transaction id, so a path to the header's merkle root doesn't prove it. Witness bytes need the path through the witness commitment.
3. A fraud proof is checked against **each arcade's view of the source chain's headers**. Two arcades that disagree about a source chain's recent headers would disagree about an ARCADE block. Entries only for blocks 100 deep narrow this; the rule for what an arcade does when its source headers are behind still has to be written.
4. Fees too small to pay out are burned today: a block's fees go to its writer only when they reach the smallest output (0.001 ARCADE). A lone small payment's fee is less than that. The alternative is to add them to the pot (§13).

### 14.14 Borrowed

An arcade that hasn't installed the chain can still use the coin: it borrows the chain from arcades that have it, exactly as it borrows a coin's Core (the same **Borrow** switch, the same offers and calls over the mesh). What differs is how a lender's word is checked. A coin's block is checked by its proof of work. This chain has none: a block is right only if the chain's rules say so, and those need the headers before it.

- **Lending.** An arcade whose copy has caught up offers the chain with its other coins; the offer names block 0, so a local test chain can't be taken for another. It answers the calls a borrowed coin asks (block count, block ids, blocks, coins, waiting transactions, sending). Blocks and headers go out in the chain's own form. Two calls are its own: `arcade_headers [from, count]` (at most 500 headers of its branch) and `arcade_coinscan` (the unspent coins of up to 100 addresses, from its address index). Each new block leaves a copy a moment behind (told of it, not yet holding it): the offer stops for that moment, but borrowers it already has are answered for 30 seconds after it was last caught up, since a borrower checks every header itself and follows the branch with the most work. A question about a chain an arcade doesn't lend is answered with an error at once, so the borrower turns to another lender.
- **Headers, all of them, checked.** The borrowed copy follows every header from block 0 by the chain's rules: the writer's signature, the draw and its weight, the proof of time, the difficulty of its epoch, the draw seed, the time limits. It keeps the id of every height and what the rules need of the newest 400 blocks, so a restart goes on where it stopped. *(Later a release will carry a checkpoint, so a new arcade doesn't start at block 0.)*
- **Which branch** is decided as an installed copy decides it (§14.6): total difficulty, whoever lends it. A lender's other branch is read from up to 100 blocks back and taken only if it is better and every header of it passes; a branch that breaks a rule changes nothing, and that lender is set aside for a while.
- **Blocks.** Kept from the block where borrowing starts (the floor), as for a borrowed coin: not stored, only what pays this arcade's wallet. A block is taken only if its id is the one the headers give for its height, and its transactions and storage section match the roots its header signs for.
- **Older coins** come from a lender's coin scan. Each coin named is checked against its block (fetched and checked as above) and asked for again as unspent.
- **Sending** builds and signs here and hands the transaction to a lender, which puts it in its pool (§14.10).
- **Taking over.** When the chain is installed, the borrowed copy goes on answering until the arcade's own has caught up, then its rows are deleted. Removing the chain starts borrowing again; the switch off deletes the borrowed rows.

**What a borrowed copy takes on a lender's word**, as with every borrowed coin:

1. That the transactions in a block spend coins that exist and that the block's reward is the right amount. The headers prove who wrote a block and that it is on the best branch, not that every arcade that checked it in full would have taken it. A writer with draw weight would have to write the bad block, and every full copy refuses it, so it never becomes the branch full copies are on; a borrowed copy with only lying lenders could be shown it.
2. That a coin scan left no older coin out, and that a coin is still unspent. A lender can hide a coin, not invent one.
3. That there is no better branch. A borrowed copy sees the branches its lenders show it.

**Open, before real storage decides the draw (step 3):** a borrowed copy must know each writer's draw weight to check a draw. With the stand-in holders it reads the weights from its settings, as everyone does. With real storage records the weights are in the chain's state, which a borrowed copy doesn't keep. Proposed: the first block of each epoch commits to that epoch's table of weights (one root in the header), so a lender shows a writer's weight with a short path and the borrowed copy checks it against a header it already checked.

### 14.15 The catalog from the arcade's side, sealing and registration (Martin 2026-11-11)

Built for the live testnet. Martin's decisions: the test coin is in main for every arcade; anyone may write by sealing and registering; **a replica counts for the draw from its registration proof** until audits (step 5) are built.

- **Storage items** (catalog entries, attestations, registration commits and proofs) wait in a pool and are shared over the mesh (`ac` frame op `it`); a writer puts in its block the ones the next block can carry, in group order.
- **Segments:** an arcade with the chain installed measures every listed chain it keeps in full, a range at a time (100 blocks below that chain's tip), cuts it by the catalog's rule, offers the next segment when nobody has and it has weight, and attests every waiting entry whose root it makes itself. `arc.catalog` shows the catalog; `arc.segment_root` the root an arcade makes.
- **Sealing:** `arc.seal` seals a listed segment under the arcade's mesh key on every core (`sealed.bin`, 128 bytes a leaf), keeps the leaf hashes of the sealed copy and of the data, checks the data root against the catalog's, and deletes the unsealed bytes: one copy on disk. `arc.replicas` lists them; `arc.open` opens leaves.
- **Registration** (`registry.rs`, replacing the fixed holders): a commit names a listed segment, the owner (mesh key), a payout address, the replica root, an anchor block at most 100 blocks back on the branch, and a work stamp of at least 20 zero bits; one replica per owner and segment. The seed of the block 10 after the commit's picks 16 leaves; the proof (each sealed leaf to the replica root, its data to the catalog root, and unsealing with the owner's key to that data) is due in the 20 blocks after. An arcade registers each of its sealed replicas by itself, paid to its wallet's first address on the chain.
- **Weight and pay:** a key weighs its genesis weight plus, for each registered replica, the segment's bytes split among the segment's replicas. Each epoch's pot is shared by weight among the genesis holders' and the replicas' addresses.
- **Borrowed copies** check genesis writers' weights from the built-in list; a registered writer's weight they take from the header, on their lenders' word (`light::UNKNOWN_WEIGHT`), until blocks commit to the weights.

### 14.16 The testnet in V1.0

`crates/arcaded/arctest.json` is the ARCADE testnet every arcade knows: block 0 `c4d9433f69c20b0cb74f4dac8fe4f70e1c93fce468a42d70fb9006c5a290c757` (time 2026-11-12 01:51 UTC; restarted from the first testnet, block 0 `849a30bf…`, with 16 MiB segments so small chains such as Pepecoin testnet take part, and the rules of §14.17 and §14.18; the lists and beacons are placeholders until the genesis list of §10 exists), the genesis writers (the GX10 and the JB20B, 1 GiB each), 2.5-minute blocks. `--arc-local <settings>` replaces it with a local test chain for closed test networks. *(Out of date since: the testnet was restarted again on 2026-11-15 (§15.5, rules version 3 from block 0) and runs the real proof of time (`"time": "vdf"`), registration, storage audits (§14.20) and serving checks per owner and audit period (§14.19). No stand-in runs on it.)*

### 14.17 Earn ARCADE: one switch per coin (Martin 2026-11-11, to build)

What Martin asked for, word for word in substance: "Each coin should just have a toggle ... earn arcade. When that toggle is clicked on it should start sealing the complete database starting at block zero including all indexes, block by block until it reaches the tip, and then it should continue block by block as they are confirmed. It needs to be as easy as it can be for the user." Earlier the same day: "Sealing is not an option for arcade coin, it is the only option"; "I don't want duplicate copies of anything ... we should just have sealed databases that we read from."

- **The switch:** each coin's manage card has one switch, *Earn ARCADE*. Nothing else to choose. The card shows one line: sealing progress (block n of tip, percent), then earning (weight, paid so far).
- **On:** the arcade seals that coin's whole database from block 0, the blocks and every index it keeps, block by block in the background, up to the tip, then each new block as it settles. Registration (§14.15) and the catalog run by themselves.
- **One copy:** a sealed piece replaces its plain copy; reads unseal only the leaves they need (about 1.4 µs per 127-byte leaf). The newest, not yet settled data stays plain until it is sealed. Off stops sealing and earning; the sealed data stays the database.
- **Pay** is still counted per confirmed segment (so arcades agree on what a copy holds): a segment earns once it is sealed here, confirmed and registered. Each index is its own family of segments, in a fixed journal form, so arcades keeping the same index confirm it together; lookup tables keep pointers into the sealed data, not the data.
- **The ARCADE chain** itself is a listed chain, sealed like any coin, and keeps its blocks once (built: the chain store's copy, whole; no block log).
- **Testnet:** restarts with 16 MiB segments (the mainnet's size too, 2026-11-12) so small chains (Pepecoin testnet) take part.

Build order: the sealed-only store for blocks; the switch and its card line; index journals (ord first); the ARCADE chain's single copy; the testnet restart and release.

*Built: index journals, ord first.* An indexer may name a journal family and the tables it covers (`Indexer::journal`; ord: family 1, its own tables, not the derived content-type table). After it folds a block in, the rows of those tables the block touched are read back as they stand after the block, ordered by (table, key), and stored as that height's journal record (`arcade_chaindb::journal`: count, then per row table name, key, and gone or the value), written through the undo log so a reorg takes it back. Writes and rewrites inside a block collapse to the value after it, so the record doesn't depend on the order the indexer wrote in, and an index built live and one backfilled make the same bytes. A family segment has the blocks' layout (a table of offsets, then one unit per height) with each height's journal record as its unit; a height before the index starts has the empty record. The catalog takes families 0 (blocks) and 1 (ord). An arcade cuts, offers, attests and seals an index family only when its journals run from the index's start (an index taken in as a copy has none for the heights before), and no further than the index has folded in. Earn ARCADE seals a coin's blocks, then its index journals; once a journal segment is sealed its plain rows go, and reads come back from the sealed copy (`set_sealed_journal_reader`). The index's tables stay as they are: they are its current state, not a copy of the journals.

### 14.18 Taken from Darrell's blueprint (Martin 2026-11-11, to build)

Martin read Darrell's "Arcade Coin Blueprint" (registered CPU mining, committee-signed 6-hour tallies, buckets for chain nodes, indexers, storage, serving and relay, bonds) and chose four of its ideas for this chain. The rest stays as designed here: blocks are drawn by proven storage and paced by the proof of time (no CPU race); pay is computed by every arcade from the chain itself and enforced by the coinbase rule (no committee, no new Script opcodes); held-back pay released by audits is the deterrent (no bonds, so no founders' allocation: no premine).

1. **Indexes earn.** Each index is sealed and earns as its own family of segments in a fixed journal form (§14.17), so an indexer is paid for index data every arcade can check, as storage is.
2. **Serving earns, via the serving check (§5.4).** Checkers drawn at random ask a replica's holder for random leaves; a replica accrues pay only while its serving checks pass. This covers what the blueprint calls chain-node uptime and serving, without self-reported uptime or request receipts.
3. **Bookkeeping.**
   - Each payout is rounded down to a whole unit; the leftover units go one each to the largest remainders, ties broken by key.
   - A share too small to pay (under the dust amount) is carried forward to that address's next payout, not lost.
   - **Fees join the pot** and are shared by weight like the block reward (this replaces "fees to the block writer"): writers are drawn at random, so fees kept by the writer would reach runners unevenly.
4. **Transaction order is not the writer's choice.** A block's transactions are ordered by `H(block seed ‖ txid)`, every transaction after the ones it spends from the same block, so no writer can front-run or reorder for advantage. (Blueprint risk 5.)

*Built (2), until audits exist:* the serving check of §5.4 with an epoch (100 blocks) in place of an audit period. A key with weight is a checker of a replica at a block when its VRF output on `H("serve", seed, replica)` falls under 6 × its weight ÷ (total weight × 100), so each replica gets about 6 checks an epoch. The checker asks the replica's owner, with the ordinary block fetch, for the plain blocks from a seed-chosen height on (about 8 MiB, at most to the segment's end), compares them with its own headers, and posts a signed `Verdict` within 20 blocks, in the same epoch. One verdict per (replica, block, checker); never on one's own replica. At the epoch's end a replica with 5 or more verdicts, most "not served", has its slice held back; the next settlement where it isn't held pays it with the new slice; held 3 settlements in a row, the held pay goes back to the pot. An arcade checks only replicas of chains it keeps itself (it compares with its own copy); other checks are skipped, which costs the checked arcade nothing. Pay is shared per earner (genesis holders by address, then each replica), so a held replica doesn't hold back its owner's other pay. `arc.status` shows the checks made here and every replica with verdicts or held pay; `arc.replicas` shows them for this arcade's own.

*Built (3 and 4):* `coins::canonical_order` (parents first, then the lowest `H("txorder", seed, txid)` of those ready); `Ticket::seal` puts a block's transactions in that order and `join` refuses any other (`TxError::Order`). A coinbase that claims any fees is refused; `Applied.fees` joins the pot. `Registry::settle_on` does the floors, the largest remainders (ties: lower address) and the credit (`credit_of`); the credit is undone with its block. The settlement is worked out before the block's own items, as its coinbase was checked.

Not taken, possibly later: a "my work" panel and a network page (city and country only); a maintenance button; a cap per key on recent blocks; a legal review before mainnet. Not taken: pay for relay bytes or request receipts (easy to fake by trading traffic with oneself); 2/3 finality signatures.

### 14.19 At 100,000 arcades (Martin 2026-11-12: "make sure our plan will work with 100000+ arcades")

With 16 MiB segments an arcade holds hundreds of replicas (Pepecoin testnet alone: about 200 across its families;
Dogecoin: thousands). Everything the chain did **per replica** would grow with what arcades store, and at 100,000
arcades it would not fit. Counted per replica (100,000 arcades × 200 replicas = 20 million replicas):

| | per replica | room on the chain |
|---|---|---|
| Registrations | 20 million, once | 200 a block, ~115,000 a day: 174 days |
| Serving verdicts | 6 per replica per epoch: ~1.2 million a block | 400 a block |
| Payouts | every earner each epoch: a coinbase of 100,000 outputs (~3.4 MB) | the block |
| Audits (sampled per owner, §5.2) | 87 a block at ~6–10 kB | 1 MB of storage section |

So the chain's work is counted **per arcade** (per owner key), drawn by bytes where it must touch a replica. Its load
then grows with the number of arcades, never with what they keep:

1. **Registration in batches.** An owner registers a batch of new replicas in one commit: the root over its
   `(segment, replica root)` pairs. The proof's 16 leaves are drawn across the whole batch (each draw picks a pair by
   bytes, then a leaf in it). At most one batch per owner per audit period (2 days). 100,000 arcades: 50,000 commits a
   day, under half the room.
2. **Serving checks per owner.** About `SERVE_CHECKS` = 6 checks per owner per audit period, each on a replica drawn
   by bytes. 100,000 arcades: ~520 verdicts a block; `MAX_VERDICTS` goes to 1,000 (about 100 kB).
3. **Paid when released.** What an owner earns accrues on the chain; it is paid when its audit period is released
   (§5.3), in one output. 100,000 arcades: ~87 payouts a block. Less than `MIN_PAYOUT` waits as credit (§14.18 3).
4. **Audits stay 4 sampled replicas per owner per period** (§5.2): ~87 a block. The storage section grows from 1 MB to
   4 MB, so audits, registrations, verdicts and payouts of 100,000 arcades take about a quarter of it.

The writer draw is already by owner weight, the catalog by segment (once for everyone), and the mesh asks a few
holders per read. These four rule changes go into the same testnet restart as 16 MiB segments and sealing block by
block.

**As built (2026-11-12):**

- **The commit** is `{owner, payout, [(segment, replica root)] (1 to 4,096), anchor, nonce, signature}`: 134 bytes
  and 64 a replica. The chain learns every segment once, so it can count holders; the signature and the work stamp
  cover one digest of it all, so mining the stamp costs the same for any batch. Its payout address is where all the
  owner's pay goes from then on.
- **The proof** names the owner and opens 16 leaves. Draw `k` is `H(seed ‖ owner ‖ k) mod (the batch's leaves)`,
  the batch's replicas laid end to end in its order; each leaf is checked against its replica root, the catalog
  root, and unsealing with the owner's key. The whole batch registers with the proof, or none of it.
- **One batch an audit period:** a commit is refused until 1,152 blocks after the owner's last proved batch. An
  arcade waits until its sealed replicas stop arriving for a minute (or the oldest has waited 30 minutes), so
  segments closing one after another go in one batch.
- **Serving checks:** a checker's VRF is on `(block seed, owner)`, the threshold gives each owner about 6 checks an
  audit period, and the replica checked is `H(seed ‖ owner) mod (the owner's bytes)` over its replicas in id order.
  A verdict names the owner.
- **Pay:** each epoch's pot is shared by weight as before (§14.18), but **accrued**, not paid. Each earner (a
  genesis holder's address, or an owner) has a **turn**, `H(earner) mod 1,152`; the coinbase of a block pays
  everyone whose turn it is, in one output each. An owner's serving checks count from one turn to the next: with 5
  or more verdicts, most "not served", its pay is held at the turn (forfeited the third turn in a row). What is under
  0.01 waits for the next turn. An owner's turn releases its pay only after a period whose audits all passed (§14.20);
  a genesis earner's needs none. This is what runs on the testnet and is the mainnet's rule; the per-epoch coinbase of
  §7's first draft and §14.7's stand-in is gone.
- **Registration and the turn together:** a batch proved at block `R` has weight from `R`. The owner's first turn
  after `R` finds no audit (its pay waits) and draws the first audits; the turn after pays, if they passed. So the
  first payout is 1,152 to 2,303 blocks after `R`, and a replica sealed after a batch waits up to 1,152 blocks for the
  next one. Worked example: [EARN.md](EARN.md).
- **Limits:** the storage section is 4 MB and a block up to about 5 MB, so the mesh's block answers hold up to
  8 MiB; 1,000 verdicts a block.

### 14.20 Storage audits as built (Martin 2026-11-12: "yes", after "is there any way someone could spoof ... and earn more")

Until now a replica counted from its registration proof alone: seal, register, delete, and keep earning. The audits of
§5.2–5.3 close that, fitted to the turns of §14.19:

- **An owner's audit period runs from one of its turns to the next.** At its turn, the turn block's seed samples
  `AUDIT_SAMPLE` = 4 of its replicas by bytes (`registry::sample`; one drawn twice is audited once) and gives each a
  picking block 1 to `PICK_SPAN` (1,092) blocks later, so commit and opening are done before the next turn.
- **Commit:** in the 20 blocks after its picking block the owner posts `A`, signed over the picking block's id: the
  Merkle root over `audit_leaf(seed, i, s_i)` for every sealed leaf, with the picking block's seed. One read through
  the sealed copy.
- **Opening:** the seed of the block 10 after the commit's chooses 4 leaves (`draw::leaf_pick`); in the 20 blocks
  after it the owner shows each sealed leaf in the replica, its data in the segment, the unsealing with its key, and
  its audit leaf in `A`.
- **At the next turn:** every audit passed: the accrued pay is released (the serving rule of §5.4 can still hold
  it). One not passed: the accrued pay goes back to the pot, and each replica that missed stops counting (weight 0)
  until it passes again; `MAX_MISSED_AUDITS` = 3 in a row removes it. An owner with no audits yet (registered after its
  last turn) is not paid until a period an audit covers: seal, register and delete earns nothing.
- **The arcade** answers its own audits on the 5-second loop (`arcaudit.rs`); `arc.status` shows its picks and what
  it answered, and every owner's weight and accrued pay.
- **Built since (§14.22 items 2 and 3):** a re-audit at once after a miss (`Reaudit`: the replica counts again when it
  passes, the period's pay stays forfeited), and the 200-block wait before a replica counts for the writer draw
  (`DRAW_WEIGHT_WAIT`). Pay and the draw now use two weights: pay weight from the registration proof, draw weight 200
  blocks after the first passed audit.

### 14.21 Rules versions, not app versions (Martin 2026-11-12: a version check so only current arcades are followed)

A check on the app's version would be easy to fake (it is a number the sender writes), needs one party to say what
"latest" is, splits the network for the hours a release takes to spread, and would stop arcades for releases that
never touch the coin. So the rules themselves are versioned, as Bitcoin and Dogecoin switch on upgrades:

- **Each network has a schedule** of rules versions and the heights they start at (`Net::rules`; now version 1 from
  block 0 on both). A block's header carries the version its height calls for (`Net::rules_at`); any other is refused.
  A header of a version this arcade doesn't know is refused with "update this arcade".
- **A rule change** ships in a release with its version and a height some days ahead. Arcades that updated switch at
  that height together; one that didn't stops there instead of following a branch of its own.
- **The mesh:** every ARCADE frame carries the protocol (`PROTOCOL` 1) and the sender's rules schedule. Frames of a
  protocol under `MIN_PROTOCOL` aren't taken (arc.status counts such peers). An arcade on the mesh that schedules a
  version this one doesn't know makes it say, in arc.status and on the Earn ARCADE card, "update before block N".

### 14.22 The plan from here (Martin 2026-11-12: "add everything to the plan")

**Done for the testnet restart:** 16 MiB segments sealed block by block (§14.19), the coins family, batch
registration, per-owner serving checks, pay at each owner's turn, storage audits (§14.20), rules versions and the
mesh protocol (§14.21), and, from the miner's Pepecoin testnet ban (arcade-v1 #112: several sibling blocks for one
height got the GX10 banned by the coin's nodes):
- **The writer never signs two blocks on one parent** (`arcsvc::run_writer`), even after its chain went to another
  branch and back, or after a restart: the parents it wrote on are kept in `<home>/arc/<net>/wrote`. Two such blocks
  are proof of cheating (§5.1), and with item 2 below they cost the writer everything it holds.
- **A peer that sends blocks that break the rules is ignored, as coin nodes ban one:** 5 such blocks in an hour and
  its ARCADE frames are ignored for an hour. Blocks only out of order, from too far back or early don't count.

**Built since (2026-11-12, for the restart):** items 1, 2, 3 and 6 below.
- **Equivocation:** the arcade posts the proof the chain keeps (`take_evidence`) as an item; a block holding it
  removes the writer's replicas, sends its accrued pay to the pot, and takes its weight (genesis weight too) for good.
- **Draw weight:** `pay_weight` (what the pot is shared by: from registration, but replicas whose last audit failed)
  and `weight` (the writer draw, serving checks and the catalog: a replica counts from `DRAW_WEIGHT_WAIT` = 200
  blocks after its first passed audit, while its audits pass).
- **Re-audit:** a replica that missed (at its owner's last turn, or this period's due pick closed unanswered) and has
  no audit open may ask, signed by its owner over a recent block; the block holding the request is the pick, and
  passing it counts the replica again at once. The period's pay stays forfeited. The arcade asks by itself.
- **Packing:** the writer orders what waits by group, registration commits by their stamp's work (most first), and
  keeps each kind within its limit and the section within 4 MB (before, a full queue could make a block that broke
  the limits).

**The real proof of time (item 4, 2026-11-12):** `time::ChiaVdf`, Chia's class-group VDF with Wesolowski proofs
through the `chiavdf` crate (Chia's own C++, Apache-2.0; GMP on Linux, MPIR bundled on Windows): a 1,024-bit
discriminant from the challenge, Chia's generator form, 100 bytes of result and 100 of proof. On the GX10 about 270,000
steps a second on one core; a proof checks in about 2 ms. A chain's settings pick it (`"time": "vdf"`; the stand-in
stays for quick local tests). The writer runs its block's VDF in the background and drops it when the tip moves; the
epoch's draw seed proof starts as soon as its task is known (block 80) and is ready by block 99. Not yet: time keepers
(one run serving every candidate of a height, §6.2): each writer runs its own.

**Fraud proofs, first kind (item 5, 2026-11-12):** `Item::BadHeader` (tag 0x62): the table row and the header of one
height of a listed segment of blocks, each a run of leaves with its path to the root. Judged against the arcade's own
source headers (Martin: only blocks `FRAUD_DEPTH` = 100 deep; an arcade whose headers don't reach that far waits on the
block, `BlockError::Waiting`, and tries again, instead of refusing it). A header that isn't the source chain's takes
the segment out of the catalog and its replicas with it. The arcade finds such segments itself (`arcfraud.rs`): a
listed root that isn't its own, a holder's leaf hashes (checked against the root), the first differing leaf, the
segment's own table walked to the block holding it, its header fetched and compared.

**Fraud proofs, second kind (2026-11-12):** the block's header is the source chain's but its transactions differ.
`Item::TxChallenge` (tag 0x63), signed by an arcade with draw weight (one open at a time): the block's table rows (where
it starts and ends), its header (checked against the source headers as above), and the bytes of one transaction in it
(at most 64 kB) that the challenger says isn't in it. Any holder answers within `REVEAL_WINDOW` = 100 blocks with
`Item::TxAnswer` (tag 0x11): **the transaction of the block that covers the challenged byte**, whole, from the segment,
with its Merkle path to the header's root. That refutes both a false claim and bytes that only look like a
transaction (a challenge in the middle of a real one, which no holder could otherwise answer); an honest holder can
always answer, as every byte of a block's body belongs to a transaction. Answered, the challenger has no draw weight
for an audit period; unanswered, the segment leaves the catalog with its replicas. The arcade challenges by itself when
the watch finds the right header with other bytes (its block body fetched from a holder, the first transaction not in
our block challenged), and answers challenges against segments it holds. Limits: a transaction over about 63 kB can't
be challenged or answered (the mesh's frame).

**Reveal demands (2026-11-12):** `Item::RevealDemand` (tag 0x65), signed by an arcade with draw weight (one open at a
time): up to 64 kB of a listed segment's leaves, any family. Any holder answers within `REVEAL_WINDOW` = 100 blocks with
`Item::RevealAnswer` (tag 0x12): those leaves with their path to the root. Unanswered, the segment leaves the catalog
with its replicas; answered, the demander has no draw weight for an audit period. Holders answer from their sealed
copy by themselves; an arcade demands by itself when no holder of a segment whose root isn't its own hands out its
leaves, and by hand with `arc.reveal {segment, first?, count?}`.

**Serving checks that can reach the owner (2026-11-14):** on the restarted testnet every check ever made said "not
served": the checker only dialed the address the owner offers its chain at, and most owners sit behind a NAT (one offers its
port but takes no dial). Owner 257daca3 had 7 "not served" verdicts while, asked by hand,
it handed out 2,000 sealed blocks in 5 s. Now (`arcserve::check`): over the owner's mesh link when there is one,
whoever dialed it (the request goes onto the link as borrowing's does, `crvia`, and the link loop hands the `bk`
answers back by id), else by dialing its offer. **No way to the owner from this checker = no verdict** (`Outcome::
Unreachable`), since that says where the checker stands, not what the owner does; an owner offline altogether misses
its storage audits. Reached and refused, wrong blocks, or over 60 s = "not served". `arc.serve_check {owner, chain,
first, count | hashes}` makes a check by hand (no verdict). Owners need no update; checkers do. Limit, stated plainly:
an owner that keeps links only to friends is checked only by them; relaying a check through a third arcade would
close it.

**Index journals** (ord, names, tokens, the coins family) have no fraud proof of their own: proving one wrong means
re-running the indexer's rules inside a proof. They rest on more than half of the draw weight confirming each root
(every arcade keeping the index makes the same one) and on reveal demands keeping their data available. A design of
their own before mainnet.

**Test chains:** `--arc-local` settings may also set `audit_period` (100 to 1,152 blocks; `Net::audit_period`) beside
`block_seconds`, so a test sees turns, audits and the 200-block draw wait in minutes. The public testnet and mainnet
keep 1,152.

**To build next, in this order:**
1. **Equivocation costs the writer** (§5.1; small, before the restart). The chain already finds two signed blocks of
   one writer on one parent and keeps the proof, but no block can carry it yet. Item `Equivocation` in a block: the
   writer's replicas are removed and its accrued pay goes to the pot; its genesis weight, if any, too.
2. **A replica counts for the writer draw 200 blocks after its first passed audit** (§4, §6.3; medium, before the
   restart). Pay and the draw use one weight now, from registration; the draw gets its own, so nothing registered on
   a secret branch can write blocks there before the branch is too old to be taken (reorg limit 100).
3. **A re-audit at once after a miss** (§5.3 item 3; small, before the restart). Today a replica that missed waits
   for its owner's next turn (up to 2 days) to count again; a request item (`Reaudit`, tag kept) counts as a pick.
4. **The real proof of time** (§6.2; large, before mainnet). Today's stand-in is a hash anyone makes at once, so on
   the testnet waiting for one's turn is only good manners and a secret branch can be built at any speed. A
   class-group VDF with a Wesolowski proof from an existing implementation (Chia's), time keepers that run it and
   release proofs for the lowest scores, and measured steps per second.
5. **Catalog fraud proofs and reveal demands** (§2.4; medium, before mainnet). A wrong segment root confirmed by
   more than half of the weight can't be challenged yet (item tags 0x11, 0x12, 0x62, 0x63 are kept for it).
6. **Registration commits most-work-first** when more wait than a block's places (§4; small).
7. **Testnet restart** with all of the above but item 4 and 5, then the GX10 and the JB20B updated, the genesis list
   (§10) for mainnet, and release checkpoints.


### 14.23 Time keepers as built (2026-11-14, rules version 2)

One calculation per height serves every candidate (§6.2). Chia's library proves only one run at a time, but it can start
a run from any form, and the squaring is the same however it is cut. So a **time keeper** runs from the challenge to the
lowest candidate's count, hands that candidate its proof, runs on from that output to the next candidate's count, and so
on. The k-th candidate's proof is the first k runs (`time::join`).

- **In a block:** the runs ride in the proof's bytes, with no change to the header. `output` is the last run's output;
  `proof` is each earlier run (8 bytes of steps, its output, its proof: 208 bytes) in order, then the last run's proof.
  The last run's steps are what is left of the block's count. A plain single run is the case of no earlier runs.
- **Rules version 2** (`params::pot_runs`): a block's proof may come in up to `MAX_POT_RUNS` = 8 runs (`MAX_TIME_BYTES`
  is 2,048). Version 1 takes single runs only. Testnet: version 2 from block 2,200; mainnet: from block 0. The draw
  seed's proof stays a single run (one calculator per epoch).
- **Over the mesh** (`arckeep.rs`, "ac" frames, each passed on once per link): `cand {prev, w, writer, vrf}`, a writer's
  draw for the block after `prev`; `tp {prev, steps, ol} + output ‖ proof`, a proof for one candidate's count. A keeper
  takes a `cand` only on its own tip, with a draw that checks (`Chain::candidate`: the VRF on the draw input, no more
  weight than the writer has) and a score that could win; it takes a `tp` only for a count some candidate needs, checked
  in full before it is passed on.
- **Writers:** a draw that could be the height's best (score below 3× the best expected from the whole weight, about 19
  heights in 20) is announced and the writer also runs its own proof; it writes with whichever is ready first. A draw
  that can't win is not announced and its own proof runs at the lowest CPU priority, so the chain moves even if every
  likely writer is away. Nothing depends on a keeper.
- **Keepers are opt-in** (`arc.keeper {on: true}`; a core while on). `arc.status` → `keeper`; `arc.block` → `pot_runs`.
- **Tested:** real-VDF unit test (cut runs = single run, tampering refused); a local three-arcade net where two arcades
  make no proofs of their own and write only with the keeper's (`ARCADE_ARC_TEST_NO_OWN_PROOF`, tests only). Two bugs it
  found: the "could win" threshold was 2^32 too strict (score units are 2^96), and block ids in frames were decoded
  without `Hash256::from_hex`'s byte order. Local test settings may set `"rules"`.
- **Still open (§13 question 3):** keepers aren't paid.

### 14.24 The genesis list as built (2026-11-14)

The list of §10 runs the chain's own checks with the seeds of the list in place of blocks (`genesis.rs`):

| Where the chain uses | the list uses |
|---|---|
| a commit's anchor block | `list_anchor(net)` (anchor 0) |
| the challenge block (proof leaves), the picking block (audit leaves) | `proof_seed = H(closed hash ‖ beacon 1)` |
| the block after an audit commit (openings) | `open_seed = H(closed hash ‖ beacon 2)` |

- `registry::check_reg_proof` and `registry::check_opening` are shared by the chain and the list.
- `finalize` keeps each owner's batch with the most stamp work, strikes (with a reason) a batch whose proof fails or
  whose segments aren't on the list, and each replica whose audit commit or opening fails. A range claimed by two
  different entries stands for neither, nor anything after it in its chain, until a fraud proof takes the wrong one out;
  each chain's entries must run from height 0. Entries left with no replica go, and so does what comes after them.
- `FinalList::hash` is what block 0 carries; `Registry::from_list` starts block 0 with the list's catalog
  (`Catalog::list_at_genesis`, confirmed at height 0) and replicas registered, counting for the draw from block 1.
- **The bulletin** (`arcgenesis.rs`, `--arc-genesis <settings>`): records (items, the assembler's receipts, its signed
  closed hash) kept in `<home>/arc/genesis-<net>/board.bin` and passed on once over every link ("gb" frames). The
  assembler receipts an item only in its window; every arcade builds the lists from receipted items only. An arcade with
  Earn ARCADE on for a kept listed chain seals it, posts its entries and one commit, answers beacon 1 (proof and an audit
  commit per replica) and beacon 2 (openings) by itself, and writes `final.json` once the last window closes.
  `arc.genesis` shows where it stands. Before block 0, `arc.earn` only marks the coin (it doesn't install a chain).
- **Beacons:** the first block of `beacon_chain` (Bitcoin on mainnet) with a header time at or after the beacon's moment,
  `beacon_depth` deep; closed test networks give fixed ones.
- **Not built yet:** block 0 from `final.json` (settings that name a final list instead of genesis holders), the
  assembler's 12-31 correction step, and a ready-made mainnet settings file.

### 14.25 The arcade's own code, kept by every arcade (Martin 2026-11-14)

Martin: "make arcade v1 the code into the genesis block and make everyone keep it": the source with its whole git
history, the vendored dependencies so it rebuilds with no internet, and the prebuilt programs. Built (`arccode.rs`,
`scripts/code-publish.py`):

- **A release** is a signed record (canonical JSON: name `@arcade/arcade-v1`, version, git commit id, time, and its
  files by SHA-256 and size): `arcade-v1.bundle` (a git bundle of the history up to the commit: `git clone` reads it),
  `vendor.tar.zst` (`cargo vendor --versioned-dirs` into `cargo-deps/`, not `vendor/`, which the repo uses for a patched crate of its own; tarred with a fixed order, times and owners), `program-<platform>`
  (release archives), `BUILD.txt` (how to rebuild offline). Its id is the SHA-256 of the record; the signature
  (ed25519) is over `"ARCADE code release\0" ‖ id` by a trusted release key (`RELEASE_KEYS`: Strawberry's release signing key
  `9c955cd5…008a`, the original arcade's (Martin 2026-11-14); `--code-key` adds one on closed test networks). Vendored crates check against the checksums in `Cargo.lock`, which is
  in the commit; programs rest on the signature (builds aren't reproducible yet).
- **Kept by every arcade** in `<home>/code/files/<sha256>`: every release's bundle and dependencies, and the programs of
  the newest 5 releases of each name. Each file is hashed before it is kept.
- **Over the mesh** ("co" frames): `r` a release record (once per link), `h` the files held, `g {id, f, at}` 48 kB of a
  file asked over the link one already has (`crvia`, so arcades behind a NAT are reached), answered `a`; 16 pieces are
  asked at once (58.6 MB in about 2 s between two arcades on one machine; one at a time took 86 s). A file fetch
  resumes where it stopped.
- **API:** `code.status`, `code.add {record, sig, files}` (what code-publish writes as `release.json`), `code.export {id, dir}`.
- **The publisher scans the whole history** up to the commit for what must never be published (private keys,
  WIF-shaped strings, extended private keys, home paths, private network addresses) and stops on any hit unless told
  otherwise: published code is public forever.
- **Sizes (v0.1.66, 2026-11-14):** bundle 8.8 MB (577 commits), dependencies 58.6 MB (537 crates, 763 MB unpacked),
  ARM Linux programs 13.8 MB.
- **Updates come from the network** (Martin 2026-11-14: "Arcade V1 should be where arcade V1 is pulling all future
  updates"): `updater.rs` takes the newest `@arcade/arcade-v1` release whose `program-linux-<arch>` archive is held
  here (looking every 10 minutes is a read of the code store), checks it again, and installs it as before (`.prev`
  kept, clean restart). GitHub is no longer asked. Tested: an arcade on v0.1.90 behind no open port fetched v0.1.91
  from another arcade, installed it and restarted into it in about 20 s.
- **Still to do:** mainnet block 0 naming the first release's id; repos for everyone under @tags (`@tag/name`, push = signed ref update) once the ledger runs on ARCADE.

### 14.26 The writer's share (Martin 2026-11-14, rules version 2)

From rules version 2 (testnet block 2,200 on the chain before its 2026-11-15 restart, from block 0 since; mainnet from block 0) a block's coinbase pays, after what is owed, **1 ARCADE
to its writer** (`params::WRITER_REWARD`) at its payout address: the one its registration names, or its genesis entry's.
The pot takes the other 49 and the fees (`Applied::writer_paid`). Fees stay in the pot, so a block with large fees is no
prize to fight over (no fee sniping); the writer's share is fixed. Why: both reviews pointed out that a writer paid
nothing for writing loses nothing by holding a block back or building in secret; now each withheld or orphaned block
costs it. The draw is by storage weight, so the share falls to writers in the same proportions as storage pay; splitting
into many keys gains nothing. Tested on a local chain switching at block 5: 49 blocks, every coinbase from block 5 pays its
writer exactly 1 ARCADE, none before, nothing refused.

## 15. The whole build, to mainnet (Martin 2026-11-14: "Build the whole entire thing don't skip anything. Make sure this time everything is working.")

Every step is done only when its **pass test** passes: unit tests, then a closed local network (`unshare -Urn`, several
arcades), then the live testnet where it applies. A step's status is kept here: **todo**, **built** (code in, tests not all
passed), **done** (every pass test passed, released).

### 15.1 Defaults taken for questions not yet answered

Taken from the recommendations made to Martin on 2026-11-14; each can be changed by a rules-version upgrade before
mainnet. Marked *(default)* where Martin has not said yes.

| Question | Taken |
|---|---|
| Blob bytes' fee | 10× the transaction floor: 0.01 ARCADE/kB *(default)* |
| Blob budget per block | 4 MB, beside the 1 MB of transactions and 4 MB of storage items *(default)* |
| Pruning | every full node keeps a blob 30 days, then may drop it once 5 audited replicas hold it *(default)* |
| Storage classes | Permanent (never pruned by rule), Release (newest 5 per release line, reference-counted chunks), Temporary (90 days) *(default)* |
| Viewer cache | on by default, size in the Options tab (**Martin: yes**) |
| Segment confirmation | block and coins segments: 10% of the draw weight and a 100-block challenge window; index segments: more than half *(default)* |
| @tag expiry | activity keeps a tag; a check-in costs 0.1 ARCADE into the pot; 12 months then 30 days' grace; claims committed ahead *(default)* |
| Repo names | under the owner's @tag, `@tag/name` (**Martin: yes**) |
| Home chain | ARCADE at mainnet, the Pepecoin ledger migrated by snapshot; on the testnet the ledger also runs on ARCADE beside Pepecoin testnet, so nothing that runs today breaks (**Martin 2026-11-14: "Yes, arcade should become the home chain"**) |
| Writer's share | 1 ARCADE of each block's 50 (**Martin: yes**, built, §14.26) |
| Pay delay (§13 q1) | as built: pay is released by passed audits *(default)* |
| Blocks only at launch (§13 q2) | blocks, the coins family and index journals at launch *(default)* |
| Time keepers' pay (§13 q3) | none, as in Chia *(default)* |
| Mainnet address prefix (§13 q4) | `A…` (p2pkh 0x17, already set) *(default)* |
| ARCADE's BIP44 coin type (§13 q6) | the home chain's: 1 on test chains, 3 on mainnet, so an @name's address on ARCADE is its claim's key hash (relied on since v0.1.71) *(default)* |
| Fees under dust (§13 q7) | moot since §14.18: every fee joins the pot |
| Genesis assembler | the GX10 *(default)* |

### 15.2 The steps

**A. Storage and code on chain**

1. **Blobs** (rules version 3): a transaction carries a blob record (an output `OP_RETURN "ablb"` with the blob's root over 127-byte leaves, its length, type, encoding and class); the bytes ride in the block's blob section, committed in the header; a block whose blobs aren't all there is refused; fees per blob byte at the blob rate. Pass: unit tests; a local net where one arcade publishes blobs, all keep them, a block missing one is refused, and the fee rule holds.
2. **Blobs as storage**: a blob segment family, sealed, audited and paid like blocks; reveal demands answered from it. Pass: a local net seals blob segments, audits pass, pay accrues.
3. **Pruning and storage classes**: Permanent / Release (newest 5) / Temporary (90 days); a full node may drop a blob after 30 days once 5 audited replicas hold it; an archive mode keeps all. Pass: a local net where an old Release blob and an expired Temporary blob are dropped and a Permanent one is kept.
4. **The viewer cache and a paid boost**: blobs fetched to view are kept (size in Options, on by default) and served; a boost is a Temporary class with a copy count. Pass: a blob viewed on three arcades is served by all three; the cache drops what isn't viewed.
5. **`/content/` from blobs, and ASHVALE on the ARCADE testnet**: arcades serve a blob by its id as `/content/` does inscriptions; ASHVALE's modules published as blobs and its launcher loads from them. Pass: ASHVALE boots on the testnet from blobs only.
6. **Git objects as blobs and a git server**: every git object a blob (stored once across repos); an arcade answers `git clone` / `git fetch` (smart HTTP) for repos it keeps; Arcade V1's own repo published so. Pass: `git clone` from an arcade gives the same commit ids as GitHub.

**B. The ledger on ARCADE**

7. **Ledger records on ARCADE**: `arcd` records of up to 1 kB in an OP_RETURN; the ledger indexer runs on the ARCADE chain (tags, profiles, follows, tokens, orders, messages). Pass: a tag claimed and a profile published on the ARCADE testnet are read back by every arcade.
8. **@tag expiry**: as 15.1. Pass: on a local chain with short periods, an idle tag expires, a checked-in one stays, a claim after release follows the commit rule, and sends warn of the owner change.
9. **@tag repos**: `@tag/name` records, signed ref updates (pushes) by owners and maintainers, forks. Pass: two arcades push, fork and clone a repo.
10. **Pull requests, issues, comments, reviews**, and an importer of a GitHub repo's history. Pass: a pull request opened, reviewed and merged on a local net; arcade-v1's GitHub history imported.

**C. Consensus rules**

11. **Segment confirmation** as 15.1. Pass: a chain kept by 10% of the weight lists its block segments; a wrong root is taken out by a fraud proof inside the window.
12. **A fraud proof for the coins family**. Pass: a wrong coins record is proved wrong on a local net.
13. **Index journals**: a design for proving a wrong one, built for the ord journal at least. Pass: a wrong ord journal record is proved wrong.
14. **§13's questions**, as 15.1, written into the rules.

**D. Mainnet**

15. **Bitcoin headers** in light mode (for beacons and Bitcoin segments), by the operator's choice: Chains & Coins, Bitcoin, Light. Nothing adds them by itself. Pass: an arcade that chose them follows Bitcoin's headers.
16. **The genesis list's 12-31 correction step** (a receipted commit left out is put back). Pass: a rehearsal where the assembler leaves a commit out and the correction restores it.
17. **The mainnet settings**: the timetable, the assembler, block 0 naming the first code release. Pass: a dry run of the whole timetable shortened to hours.
18. **Release checkpoints** for new or long-offline arcades. Pass: an arcade started fresh takes the checkpointed branch.
19. **Migration**: the Pepecoin ledger snapshot at an announced height carried into ARCADE's genesis; apps stay where inscribed. Pass: a snapshot of the testnet ledger migrated into a local ARCADE chain; every tag and balance matches.
20. **Testnet rehearsal** (mid-December), 21. **the genesis window** (Dec 21–31), 22. **mainnet launch** (2027-01-01).

**E. Housekeeping**

23. The ThinkPad and arcade_v1 on the current release (Martin). 24. Automatic updates on for Ashburn. 25. The GX10 → @arcade_v1 send and the faucet's owed ARCADE gifts, once the coins mature.

### 15.3 Status

| Step | Status |
|---|---|
| 1 | built: chain unit test and local three-arcade net pass (blobnet: publish, hold byte for byte, relay, named again free, refused without bytes, restart); live testnet with the rules version 3 release |
| 2 | built; stornet passes: the blob family's segment listed, every arcade's replica registered, audits passing for three periods, pay accruing to all three owners (and paid at their turns) |
| 3 | built; prunenet passes: Temporary dropped, Permanent and the newest Releases kept, the archive keeps all, a late arcade gets the Permanent blob and not the expired one |
| 4 | built (viewer cache on by default, its size in Options; boosts) |
| 5 | built; ashnet passes: ASHVALE's 128 modules in one transaction (39.2 ARCADE), its launcher a blob, booted to character creation from blobs only on an arcade that published nothing |
| 6 | built; gitnet passes: arcade-v1's 577-commit history pushed as 3 packs (about 90 ARCADE), cloned from another arcade with the same head and tree, fsck clean; an incremental push adds one pack |
| 7 | built; ledgernet passes: a tag claimed and a profile published on ARCADE read back by all three arcades and after a restart, the home chain untouched, a 600-byte record in one output |
| 8 | built; tagnet: release after idle + grace, check-ins keep a tag, the send warning pass; the commit-rule check is being rerun after two fixes |
| 9 | built; reponet passes: push, clone from another arcade, fork, fork push, a non-maintainer refused (and invalid on chain), a maintainer's push, history |
| 10 | built; prnet (pull request reviewed and merged, the GitHub history imported) under way |
| 11 | built (rules version 3): unit tests pass (a quarter of the weight lists, the window, a fraud proof inside it); a network test needs a source chain a closed test net can't sync |
| 12 | built: BadCoins; chain unit test passes (inflated entry taken out; right entry, forged leaf, foreign header refused); watcher posts it |
| 13 | built for ord: BadOrd (one-piece inscription rows); chain unit test passes; watcher posts it; pieced inscriptions and the other journals rest on more than half the weight |
| 14 | done: §13 written as settled |
| 15 | built: "light" mode (headers only) and Bitcoin added by every ARCADE arcade; a fresh arcade reached Bitcoin's tip (975,881 headers) in about 3½ minutes |
| 16 | built; gennet2 passes: the assembler leaves B's receipted commit out of its signed list, every arcade corrects it, the final list and the chain are as with an honest one |
| 17 | built: Genesis::code; crates/arcaded/arcmain-genesis.json (timetable, the GX10 assembling); gennet3, the timetable shortened to minutes, makes a block 0 naming a release and starts the chain. To fill in before 12-21: the release id and the first difficulty |
| 18 | built; cpnet passes: a fresh arcade with the right checkpoint syncs to the tip, with a wrong one it stops below it; the testnet ships block 1,000's |
| 19 | built: the ledger snapshot (322 tags, 121 tokens, 462 balances of the testnet at Pepecoin testnet block 1,569,080, hash 3d0e902c…) named in block 0 and carried in; mignet passes: every tag on all three arcades with the same key hash, all 462 balances exact, a migrated tag taken. Since v0.1.84 the "carried in" note lives with the chain's own rows (chain store STATE `note:ledger_snapshot`): a copy made again takes the snapshot again right after block 0, and a store with blocks but no note has its ledger made again, the snapshot first, then every record walked once more (the JB20B lost all 322 tags to a rebuild under the old app-store flag, 2026-11-15) |
| 20–22 | the testnet rehearsal (mid-December), the genesis window (12-21 to 12-31), launch (2027-01-01): dates, not code |
| 23 | Martin |
| 24 | todo |
| 25 | running (waits for block 1,165) |

### 15.5 The testnet's third start (2026-11-15, v0.1.73)

Martin 2026-11-14: restart the testnet with block 0 naming the latest build and the ledger carried in; test coins
start over; at mainnet the testnet's data (not its coins) is carried by snapshot. Block 0 names release v0.1.72
(`0d4fdc79…`; a block 0 can't name the release that holds it, so the restart is the release after) and the Pepecoin
testnet ledger at its block 1,569,097 (322 tags, 121 tokens, 462 balances; `crates/arcaded/arctest-ledger.json`, hash
`4b25079a…`). Rules version 3 from block 0. Genesis writers: the GX10 and the JB20B. First difficulty: the old chain's
after its block-1,100 retarget.

### 15.4 As built: steps 5 to 11 (2026-11-14)

- **Blobs as pages (5):** `/content/<blob id>` answers with the same policy as ord inscriptions (inline scripts, loads
  from this arcade), so a launcher runs; a gzip blob goes out with `Content-Encoding: gzip`, and a blob fetched from
  another arcade keeps its encoding. ASHVALE's own account calls (`/r/inscriptions`, storage) need the arcade's app
  frame, as on the home chain.
- **Git (6), differs from 15.2 in two ways:** objects are stored as **packs** (git `pack-objects` under 3 MB, one
  block's blobs each) rather than one blob per object: a blob record costs about 110 bytes and its fee rounds up to a
  kB, so 5,000 objects as blobs would cost more in records than in data; a fork reuses its source's packs, so nothing
  is stored twice where it matters. And the arcade speaks git's **dumb HTTP protocol** (static files from a manifest
  blob), which every git client clones and fetches from with nothing running but the arcade; pushes are
  `git push arcade::@owner/name` through `scripts/git-remote-arcade` (or `scripts/git-publish.py`). `/git/<manifest>` or `/git/@owner/name`.
- **Ledger on ARCADE (7):** the same engine and records as the home chain, indexed in the same commit as each block;
  claims follow the chain they record; ARCADE's coins come from the chain's own coin set. Records up to 1 kB are one
  OP_RETURN. On the testnet the home chain's ledger runs beside it unchanged.
- **Tag expiry (8)**, ARCADE only: activity = any record from the owner's address; check-in (INSC kind 20) pays at
  least 0.1 ARCADE; released `idle + grace` after the last activity (by block time, swept each block); then for
  `reveal_blocks` (a day) a claim needs a commitment `sha256("arcade tag commit"‖tag‖address‖salt)` (kind 21) from an
  earlier block, and the earliest commitment wins at the window's end; sends warn for 90 days after a released tag
  changes hands. Local settings: `tag_idle`, `tag_grace`, `tag_reveal_blocks`, `tag_warn`.
- **Repositories (9) and their items (10):** `REPO` records (push, maintainer, fork, open, comment/review, state) in
  the ledger; texts are blobs. Reviews and merges are the owner's and maintainers'; authors close their own; a merge
  moves the repository to the merged manifest. `scripts/github-import.py` brings a GitHub repository's issues, pull
  requests, comments and reviews, each keeping its GitHub number, author and time.
  **Rules version 4 (ARCADE #10):** a push names the manifest it was built on (REPO op 7: name ‖ manifest ‖ base ‖
  flags, bit 0 force; base is zeros for a repository's first push), and the ledger refuses it (an invalid record, as
  any refused record) when base isn't the repository's manifest at that point of the chain, unless forced; a forced
  push replaces as before. Below version 4's height op 7 is unknown and refused, as it always was; from that height
  an op 1 push (no base) only makes a new repository and is refused onto an existing one. Arcades send op 7 from that height (`ledger.repo_push` fills `base` with the
  repository's manifest now when the caller gives none; the Git app's request takes its manifest's `prev`;
  `git-publish.py` and `git-remote-arcade` send the manifest they read, `--force` / `git push --force` force). This
  stops a push built from an out-of-date copy silently rolling another one back (2026-11-16: a mirror's second push,
  36 s after the first, put `main` back).
- **Light confirmation (11)**, rules version 3: block and coins segments list with more than a tenth of the draw
  weight (the first offer of an arcade over 10% lists it) and replicas of them register only after
  `CHALLENGE_WINDOW` = 100 blocks; fraud proofs and reveal demands reach them meanwhile (the watcher asks the poster
  too, since no replica is registered yet). Index families still need more than half.
