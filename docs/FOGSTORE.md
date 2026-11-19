# Fog-of-war storage for ARCADE blobs (rules version 5)

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
  | Permanent | 7 | 12 | 394,800 (0.003948 ARCADE) |
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

- A blob-family segment needs the floor of its strongest class (Permanent 7, otherwise Release 5). A Temporary segment
  needs 3 until its time is up, then none.
- **Copies count only when audited.** Once a segment is listed in the catalog, its copies are the registry's replicas
  of it (`Registry::holders_of`): sealed, registered and still passing audits. A replica goes after
  `MAX_MISSED_AUDITS` misses. While a segment is still open (its range not complete), its keepers are the arcades that
  announce keeping it. Announcements older than a day don't count, so a restart moves nothing.
- **Short segments.** Below its floor a segment is short. Arcades with room in their keep share step in, ranked by
  `sha256(node id ‖ family ‖ range)`, as many as are missing, so they don't all take it at once.
- **Above the floor**, the worst-ranked keeper lets go. That's what happens when a keeper that was down comes back
  after another took its place.
- **Nobody has room:** the segment stays short and shows as a warning in `arc.fog`. It's never forced on anyone.
- Scarce data pays more by itself: a segment's pay is split over its replicas (weight B/n).

## Holder announcements

Each arcade adds a `fog` part to its "sh hold" frame under the ARCADE chain:
- `bf`: a bloom filter of the blob ids whose bytes it holds (at most 16 kB, about 13,000 blobs);
- `bn`: how many;
- `keep`: the segments it keeps by choice, as `[family, range]`;
- `room`: its free keep room in MB.

Announcements are signed and passed on across the mesh. They're used only to find someone to fetch from and to rank
who steps in, never as a copy count for a listed segment.

## Settings and API

| Setting | Default | Where |
|---|---|---|
| Grace window | 7 days | `fogstore::grace_secs`; `ARCADE_FOG_GRACE_SECS` on a closed test net |
| Idle time | 30 days | `fogstore::idle_secs`; `ARCADE_FOG_IDLE_SECS` |
| Announcement age that still counts | 1 day | `fogstore::stale_secs`; `ARCADE_FOG_STALE_SECS` |
| Keep share | 0 (none) | `arc.keep {mb}` (0 or "off": none) |
| Viewer cache size | 2 GB | `arc.cache {mb}` |
| Archive (keep everything) | off | `arc.archive {on}` |

- `arc.fog`: what fog storage does here. It shows the blobs held here (and how many are this arcade's own), the
  segments it keeps, the network's segments short of copies (with how they were counted), and warnings.
- `arc.keep`: the share kept by choice and its segments.
- For other reads: `fogstore::blob_floor(class)` and `fogstore::short_of_copies()`.
- `explorer.arc_storage_summary` (what the ARCADE Storage app reads) carries them as `floors` (per class), `short`
  (segments and blobs below their floor) and `fog` (this arcade's settings, keep share and blobs held).

## Switching it on

The ARCADE testnet has no version 5 height yet. A release adds `(5, height)` to `params::TESTNET.rules`. A closed test
net sets it in its settings (`"rules": [[3, 0], [4, 1], [5, H]]`). See `tests/regtest/fognet.py`.
