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

## Node storage management (keep, pin, forget; the Storage app)

Martin 2026-11-19: operators "opt into being additional keepers for data that they value and opt out of keeping any
data that they wish to forget" (docs/STORAGEAPP.md in the plans). `crate::fogkeep`, the choices kept per network in
`<home>/arc/<net>/fog-choices.json`:

- `fog.keep {segment | blob | publisher, mode?: "segment" | "pin", preview?}` (Admin):
  - `segment` (default): this arcade becomes a chosen keeper of the blob's whole segment (`P#n` / `T#n`; a publisher:
    every segment holding its blobs). Counted once registered and audited, sealed and paid, inside the keep share. The
    fog round never lets a chosen segment go above the floor as it does segments the share took by itself; over the
    share, chosen segments go last. A full share is refused with the reason, or room is made by letting go of
    segments the share took by itself whose copies hold without this arcade (the card lists them).
  - `pin` (a blob; a large blob with its parts): one uncounted extra copy, never faded, served to anyone. It uses the
    keep share and earns nothing (decided 2026-11-19).
  - Refused with the reason on a borrowed ARCADE chain ("no keep share: ... borrows everything") or with no keep share.
- `fog.forget {segment | blob | publisher, preview?}` (Admin): the local forget list. Its blobs are dropped here (viewer
  cache, grace copy, pins; a blob this arcade published is un-pinned, its `.mine` mark goes) and refused from then on:
  `arcblob::view` doesn't fetch them, `bq` asks get "not here", `fetch_keep` skips them, and the holder bloom leaves them
  out. A blob in a segment this arcade keeps keeps its bytes (the network is owed the segment) but is still never
  served. A forgotten segment is never taken again until undone.
- **Hand-over.** Forgetting a segment this arcade keeps doesn't drop it: the hold announcement lists it under `leave`,
  every arcade counts leaving keepers out of its keep decisions, so the next ranked arcade with room steps in (the
  lost-keeper path), and this arcade drops its copy only once the copies counted without it are back to
  `min(floor, copies when forgotten)`: the new keeper registered and passed an audit. An open segment (not listed:
  nothing can be registered) waits for a new keeper that announces it and holds every blob of it. With no arcade
  announcing room, it stays a keeper and `fog.mine` says so. On the drop its sealed copy is deleted: it stops answering
  audits, so the registry removes it the usual way (3 missed); nothing is written on a chain. The sealing run never
  goes back over a segment let go (`fogkeep::released_end`).
- `fog.undo {id}` (Admin): takes back a keep, pin or forget (a hand-over not finished stops: it stays a keeper).
- `fog.mine` (Admin): kept by choice (with pay estimates), pins, forgotten, handing over (`handing over (n/floor)` and
  why it waits), the keep share (`none · borrows everything` on a borrowed arcade), each segment kept here (counted or
  not), its blobs by role and how much of the map is in the fog for it. Never public.
- **From an app page** (the Storage app): `POST /r/fog/<keep|forget|undo>` files a request (kind `fog`) in the wallet's
  approval queue, as `/r/repo/<op>` does; Arcade V1.0's app window shows its card (`appwindow.js`: what it does, size,
  copies before and after, pay estimate, seal time on this machine from `sysinfo::seal_speed`) with Approve / Refuse.
  `GET /r/fog/req/<id>` is where it stands; `GET /r/fog/mine` is `fog.mine` for a page this arcade serves its operator.
  The card is worked out again when shown (`wallet.preview`) and applied on approval; nothing changes before.
- Pay estimate: the segment's weight B/n (n = copies with this arcade) against all pay weight now (owners' replicas and
  the genesis bytes), of 49 ARCADE a block over a 30-day month. An estimate, labeled so.
- Closed test nets never complete a 10,000-block range, so nothing is listed or audited there:
  `ARCADE_FOG_TEST_AUDIT_SECS=n` (local chains only) counts an announced keeper as registered at once and audited once
  it has kept the segment n seconds holding every blob (`fogstore::test_counted`). See `tests/regtest/fogkeepnet.py`.

## Switching it on

The ARCADE testnet has no version 5 height yet. A release adds `(5, height)` to `params::TESTNET.rules`. A closed test
net sets it in its settings (`"rules": [[3, 0], [4, 1], [5, H]]`). See `tests/regtest/fognet.py`.
