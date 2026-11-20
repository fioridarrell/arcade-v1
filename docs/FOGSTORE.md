# Fog-of-war storage for ARCADE blobs (rules versions 5 and 6)

From rules version 5, no arcade has to keep every blob's bytes. Every arcade still keeps every block and every blob's
~110-byte record (root, length, class, type), so any bytes can be checked when they arrive. The bytes themselves live
where they're wanted. Code: `crates/arcaded/src/fogstore.rs`; consensus: `arcade-arc` `params::rules_v5`, `blob.rs`,
`chain.rs`, `segment.rs`.

## What changes at the switch-on height

- **Blocks without bytes.** A block is valid with its blobs' records only, whatever its age. Before version 5 a block
  younger than the blob window (30 days) had to carry every blob's bytes.
- **Fees by class.** A blob pays for the copies its class keeps, for the months it prepays, at 47 ARCADE per GB per copy
  per month (`blob::COPY_MONTH_PER_KB` = 4,700 base units per kB):

  | Class | Copy floor | Months prepaid | Per started kB |
  |---|---|---|---|
  | Temporary | 3 | 3 | 42,300 (0.000423 ARCADE) |
  | Release | 5 | 12 | 282,000 (0.00282 ARCADE) |
  | Permanent | 7 (3 from version 6) | 12 | 394,800 (0.003948 ARCADE) |
  | Boost | +2 | 3 | 28,200 (0.000282 ARCADE) |

  Before version 5 the flat 0.01 ARCADE per kB holds.
- **A Temporary blob family.** Temporary blobs (and those a boost brought first) are sealed in their own family,
  `segment::FAMILY_BLOBS_TEMP` (13), so their copies are audited and paid and their segments can expire.
  `FAMILY_BLOBS` (10) keeps Permanent and Release blobs.

Before the height everything works as before: every arcade takes every blob's bytes, and the old class pruning applies.

## Who keeps the bytes

An arcade keeps a blob's bytes when any of these holds:

1. **It published the blob.** It keeps it for good (a `<id>.mine` mark in the blob store).
2. **The blob is new.** Every arcade keeps a new blob for the grace window (7 days), so data outlives its publisher
   going away before anyone views it.
3. **It keeps the blob's segment by choice.** `arc.keep {mb}` sets a share of the chain in MB. The arcade takes whole
   blob-family segments (10,000-block ranges), fetches every blob in them, seals them and earns for them.
4. **It was viewed or served here** (fog of war). An arcade lacking a blob fetches it over the mesh from one that has
   it, checks it against the record, keeps it and serves it from then on. It drops it once nobody has viewed it or
   asked for it here for the idle time (30 days). These copies aren't audited; they're a bonus.

Anything else is dropped after the grace window. A Temporary blob goes everywhere but at its publisher once its time
(90 days) is up.

## Copy floors and keepers

- A blob-family segment needs the highest floor of the classes in it (Permanent 7, from rules version 6 Permanent 3;
  Release 5: a segment with a Release blob needs 5). A Temporary segment needs 3 until its time is up, then none; one
  holding a blob upgraded to Permanent (version 6) needs the Permanent floor for good. The floors are the rules' (`blob::FLOORS`, read at the next block's
  height by `blob::floor_at`).
- **Copies count only when registered and audited.** Once a segment is listed in the catalog, its copies against the
  floor are the registry's replicas of it that passed an audit and haven't missed one since. A replica stops counting
  at its first miss and goes after `MAX_MISSED_AUDITS` misses. Arcades that announce keeping a segment but whose copy
  isn't counted (not registered yet, or not audited) are shown apart as "announced, not yet counted"
  (`announced_not_counted`). While a segment is still open (its range not complete) nothing can be registered, so
  nothing is counted; who steps in is decided by the arcades that announce keeping it then, so they don't all take it.
  Announcements older than a day don't count, so a restart moves nothing. This is each arcade's own view for its fog
  decisions and reports; the registry's count that splits pay (`Registry::holders_of`) is consensus and unchanged.
- **Short segments.** Below its floor a segment is short. Arcades with room for it in their keep share step in, the
  most room first, then ranked by `sha256(node id ‖ family ‖ range)`, as many as are missing, so they don't all take it
  at once.
- **Above the floor**, the last keeper in that order lets go. That's what happens when a keeper that was down comes back
  after another took its place.
- **Nobody has room:** the segment stays short and shows as a warning in `arc.fog`. It's never forced on anyone.
- Scarce data pays more by itself: a segment's pay is split over its replicas (weight B/n).

## Keep shares by machine

Each machine keeps a different amount (Martin 2026-11-19): a fully borrowed arcade on small hardware keeps nothing, a
frontier one with many full chains and indexes keeps a lot. The amount is always the operator's choice (Options → Keep
ARCADE data); the default is Off (`fogstore::DEFAULT_KEEP_MB` = 0) and nothing sets it by itself.

- **The machine's budget** (`crate::diskbudget`): free space on the data volume; what Chains & Coins uses now (this
  arcade's own size measurement); what it will grow to (per kept chain, the size arcades report for its mode plus its
  indexes, plus 10%). The reserve is the difference. Shown as one line in Options and at the top of Chains & Coins:
  "Chains & Coins uses X · ARCADE keep Y · Z free".
- **Quick picks**: Off; 5, 10 or 25% of the free space past the reserve (recomputed as free space changes, at most once a
  minute, and never past 20 GB left free); "Suggested for this machine" (a quarter of the free space past the reserve
  and 20 GB, in whole GB; none under 1 GB); Custom (GB). Each shows what it would hold (segments and files, how many of
  the segments short today it holds and how many reach their floor with it) and is greyed when it doesn't fit.
- **The prompt** (Home): an arcade with Earn ARCADE on, 50 GB or more free and a suggestion above 0 that keeps nothing
  is asked once; "Not now" hides it for good. A borrowed-everything arcade is never asked.
- **Who steps in**: the candidates with the most free keep room first (by the bit length of their room in MB, then the
  hash rank), so a big machine takes most and a small one only what fits; a candidate counts only when its announced
  room holds the whole segment. Above the floor, the last in that order lets go.

## Holder announcements

Each arcade adds a `fog` part to its "sh hold" frame under the ARCADE chain:
- `bf`: a bloom filter of the blob ids whose bytes it holds (at most 16 kB, about 13,000 blobs);
- `bn`: how many;
- `keep`: the segments it keeps by choice, as `[family, range]`;
- `room`: its free keep room in MB;
- `bm`, `mn`: a bloom of the blobs it published, and how many (the fog map's publisher role);
- `km`, `ku`: its keep share and what the share holds, in MB (the fog map's node panel).

Announcements are signed and passed on across the mesh. They're used only to find someone to fetch from and to rank
who steps in, never as a copy count for a listed segment.

## Settings and API

| Setting | Default | Where |
|---|---|---|
| Grace window | 7 days | `fogstore::grace_secs`; `ARCADE_FOG_GRACE_SECS` on a closed test net |
| Idle time | 30 days | `fogstore::idle_secs`; `ARCADE_FOG_IDLE_SECS` |
| Announcement age that still counts | 1 day | `fogstore::stale_secs`; `ARCADE_FOG_STALE_SECS` |
| Keep share | 0 (none) | `arc.keep {mb}` or `{gb}` (0 or "off": none); the default is `fogstore::DEFAULT_KEEP_MB` |
| Floor cap (closed test nets) | none | `ARCADE_FOG_FLOOR_MAX` (fewer arcades than a floor) |
| Viewer cache size | 2 GB | `arc.cache {mb}` |
| Archive (keep everything) | off | `arc.archive {on}` |

- `arc.fog`: what fog storage does here. It shows the blobs held here (and how many are this arcade's own), the
  segments it keeps, the network's segments short of copies (with how they were counted), and warnings.
- `arc.keep`: the share kept by choice and its segments; also the default, whether the operator chose, free disk,
  whether Earn ARCADE is on here, `suggest` (the app's prompt: Earn ARCADE on, 50 GB or more free, no share, not
  dismissed; `{dismiss: true}` hides it for good) and `would_hold` (the segments short of copies now, the network's
  segments and bytes). The app's Options tab sets it ("Keep ARCADE data", in GB); nothing sets it by itself.
- `explorer.arc_blobs` (/r/explorer/arc_blobs): every blob as fog storage sees it from here, for the Storage app's fog
  map. A large blob is one item with its parts. Per item: id, type, publisher (@tag, address), class, bytes, segment
  (`P#n` for the blobs family, `T#n` for temporary blobs), copies (registered, audited) against the floor, announced
  keepers not counted yet, all copies, holders by role (keeper: registered replica or announced keep; publisher: its
  `bm` bloom; grace: holds the bytes while the blob is new; viewer: holds them past that), each also marked
  registered+audited, registered, here or announced; last viewed here, here, mine. With `id`, that item and its
  `journey`: published, grace until, the keepers (registry: sealed at, audited from, missed in a row; announced: since
  when this arcade saw them), keepers that came and went as this arcade's rounds saw them, the copies outside the
  floor. Paged (`offset`, `limit` up to 500), filtered (`publisher`, `class`, `short_only`, `here_only`), sorted
  (`publisher`, `newest`, `copies`), with totals for the filtered set: copy bands, per publisher, per holder (by role).
- `explorer.arc_fog_nodes` (/r/explorer/arc_fog_nodes, `node?`): per node: keep share (setting, used, free, from its
  announcement), blobs held by role, held and in the fog, segments kept (`P#0·2/2`: keepers of the floor), earning
  weight (the registry's pay weight), and the 10 nodes it shares the most blobs with.
- Bytes held by other arcades come from their blooms, so a false positive is possible; view times are known only for
  this arcade's own copies.
- For other reads: `fogstore::blob_floor(class)` and `fogstore::short_of_copies()`.
- `explorer.arc_storage_summary` (what the ARCADE Storage app reads) carries them as `floors` (per class), `short`
  (segments and blobs below their floor) and `fog` (this arcade's settings, keep share and blobs held).

## Rules version 6 (Martin's decisions, 2026-11-19)

Consensus change on the testnet from `params::RULES_V6_HEIGHT` (a placeholder, 3,000, until the release sets it).
Code: `arcade-arc` `params::rules_v6`, `blob.rs` (`FLOORS`, `floor_at`, the upgrade), `chain.rs` (`blob_changes`),
`pool.rs`; `arcaded` `coinsend::upgrade_blob`, `fogstore`, `fogmap`.

- **Permanent copy floor 3** (was 7). Release 5, Temporary 3 and a boost's +2 stay. The floor is a row of the rules
  table `blob::FLOORS` (`(version, [Permanent, Release, Temporary, Boost])`), so a later version can raise it again
  (toward 7 as the network grows) with a new row; nothing scales by itself. **Prices don't change:** a Permanent blob
  still pays for 7 copies a year (`blob::priced_copies`, 394,800 base units per started kB).
- **A Temporary blob can be upgraded to Permanent.** A transaction output `OP_RETURN <push>` whose push is `"aupg"` ‖
  the blob ids it upgrades (32 bytes each, 1 to `blob::MAX_UPGRADE_IDS` = 64) makes them Permanent from the block that
  holds it. The transaction pays, on top of its own fee floor, for each blob the **difference**: the Permanent fee
  minus the Temporary fee for its bytes, both by `blob::fee_at` at that block's height (`blob::upgrade_fee_at`; today
  394,800 − 42,300 = **352,500 base units per started kB**: 1 kB 0.003525 ARCADE, 1 MB 3.525 ARCADE, 100 MB about
  352.5 ARCADE). The fee goes into the pot, as every blob fee does. **Anyone may upgrade** any Temporary blob (the payer
  needn't be the publisher); it stays its publisher's. A blob a boost brought (kept as a Temporary one) is upgraded the
  same way, paying Permanent minus the fee of the class that brought it, the boost's (394,800 − 28,200 = 366,600 per
  started kB): every upgraded blob has paid a Permanent blob's fee in all.
- **From that block** the blob is Permanent: its class now (`blob::State::class_now`, the reads' `class`), its floor
  (Permanent's), no expiry, and fog storage treats it as one (its segment keeps the Permanent floor, the old class
  pruning never forgets it). Its record's class, and so the family it is sealed in (`FAMILY_BLOBS_TEMP`), don't change.
- **Refused** (the whole block is invalid; the pool refuses the transaction): an id the chain doesn't hold before the
  block (one brought in the same block counts as unknown: publish it Permanent instead); a blob that is Permanent or
  Release already, or upgraded already; a Temporary blob **past its time**: consensus counts the Temporary time in
  blocks (`blob::temporary_blocks`: `Net::blob_temporary` / block seconds = 51,840 blocks for 90 days) from the block
  that brought it, or from its latest boost, and refuses an upgrade at or after that height, since its bytes may be
  gone; a blob named twice in one transaction or one block; and a transaction paying less than the sum of the
  differences on top of the blob fee for what it brings.
- **A boost past a blob's time doesn't bring it back.** A boost keeps a timed blob `temporary_blocks` past the boost's
  block only while the blob is within its time; boosting one already past it is still valid and paid (as before
  version 6) but leaves it past its time, so it can't be upgraded.
- **Fog storage keeps a timed blob's bytes by the chain's count.** The old class pruning and fog storage drop a
  Temporary blob, and its segment's floor goes to 0, at the height consensus refuses its upgrade from
  (`blob::State::until`, `arcblob::Meta::timed_done`), not by the clock, so no arcade drops bytes an upgrade can still
  make Permanent. An upgraded blob is never dropped as Temporary.
- **Large blobs:** consensus can't read a manifest (no arcade must keep the bytes), so an upgrade names every blob it
  upgrades. `arc.blob_upgrade` on a manifest's id names the manifest and all its parts (each once) in one output and
  pays each one's difference by its own bytes (as each was paid for), i.e. the difference for the total bytes. When the
  manifest's parts can't be read here it refuses, rather than upgrading the manifest alone.
- **A block brings at most 4 MB of blobs by their records.** The lengths the records declare for the blobs a block
  brings new (a boost of a blob the chain holds brings nothing and doesn't count; each part of a large blob does) add up
  to at most `blob::MAX_BLOB_BYTES`, or the block is refused (`BlockError::TooBig("declared blobs")`). Until now only the
  bytes a block carries were capped, and since version 5 a block may come without them, so a writer could declare any
  amount in a bytes-less block. The pool refuses a transaction declaring more than that, and a block template's blob
  budget counts the declared lengths (`Pool::pick_with`), so writers never build such a block.
- **Before the height** an `"aupg"` output is only an OP_RETURN's data, as it always was (older arcades take such
  blocks too, so nothing splits before the switch); the pool and `arc.blob_upgrade` refuse upgrades until then.
- **API:** `arc.blob_upgrade {id, send?}` (Wallet): without `send` the quote (`bytes`, `already_paid` the Temporary
  fee, `permanent_fee`, `difference`, `fee` with the transaction's own, `at_height`, `floor`); with `send` it goes. `arc.blob`
  and `explorer.arc_blobs` give `class` as it is now and `upgraded {by, at_height, paid, txid}` (a large blob's `paid`
  sums its manifest and parts).
- **Pass test:** `tests/regtest/upgradenet.py`: a closed net with version 6 at a low height; a Temporary blob is
  published, an upgrade is refused before the height, a different wallet upgrades it after, every arcade has it
  Permanent with floor 3 at the same block, and an underpaid or second upgrade is refused by all.

## Switching it on

Version 5 runs on the testnet from block 2,150 (v0.1.133). Version 6 is scheduled at `params::RULES_V6_HEIGHT` (one
constant; the release sets the real height ahead of the tip). A closed test net sets its own schedule in its settings
(`"rules": [[3, 0], [4, 1], [5, 2], [6, H]]`). See `tests/regtest/fognet.py` and `tests/regtest/upgradenet.py`.
