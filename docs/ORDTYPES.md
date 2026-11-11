# Inscriptions by type — the Explorer's type filter

Status: **approved by Martin 2026-11-10** ("Index + load images apart"). Built on the JB20B.

## Why
The type filter (Images, SVG, Text, HTML, JSON, Audio, Video, 3D, Other) had no index by type.

- To find one page of a type, the arcade opened every inscription's record one at a time, often 65,000 of them.
- On the JB20B's Dogecoin (180 M inscriptions, a 52 GB record table, 15 GB RAM), that made a page take **7–10 s**.
- The background count of every type ran at about 5,800 inscriptions a second, so about 8 hours for Dogecoin.
- It also kept a per-chunk count file in `cache/` as a workaround.

## The table
`ord.v1.types`, in each chain's file (C = chain number):

| Key | Value | Meaning |
|---|---|---|
| `C \| g u8 \| seq u64` | number u64 | The g-th group's inscriptions, oldest first |
| `C \| 0xFF` | 9 × u64 next seq per group, then top u64 | The counters. Also the mark that the table is built |

- **Group order** is `image, svg, text, html, json, audio, video, model, other`.
  - It is stored on disk, so only add groups at the end.
  - `ordtypes::group_of` is the one grouping function. The Explorer uses it too.
- **A page** at offset *o*, newest first, is a single range read of seq `total−1−o` downwards.
  - The totals are the counters, exact at once.
  - Reads skip any row whose number is at or above the chain's next number.
- **Size:** about 27 bytes per inscription, about 5 GB for Dogecoin.

## Kept with each block
- The ord indexer adds a row for each new inscription in the block's own commit. The rows are undo-logged, so a reorg undoes them with the block.
- Before each block, `trim` makes the rows match the chain:
  - It drops rows at or above `next`. A reorg of a block from before the build leaves such rows, because the build writes without undo records.
  - It fills in numbers between the counters' `top` and `next`. That covers a stretch indexed by an arcade version without this table.

## Building it once
`ordtypes::build`, run in the background by arcaded for each chain with the ord index whose mark is missing:

1. **One read snapshot:** every `ord.v1.insc` record, in key order. Only the number and content type are taken, without parsing the whole JSON. The result is one byte per inscription number (180 MB for Dogecoin).
2. **The rows, group by group:** written in key order with appends, 500k rows per write. These cover numbers below `next − 20,000`. Rows left by an interrupted build are deleted first.
3. **One write:** the newest inscriptions as they are now, read from their records, plus the counters (the mark). Sync carries on during the build. The ord index starts adding rows from the block after this write.

Until the mark exists, the old per-chunk counting answers. It no longer counts in the background while a build runs. Once the build finishes, the old count file is deleted.

**Derived, not copied:** index copies (`v1copy`) leave this table out, and each arcade builds it from its own records. Removing or rebuilding the ord index deletes it with the other ord tables.

## Images load apart
- The desktop asks `explorer.inscriptions` with `img: "links"`.
  - Images get `content.src = "/ord/<chain>/<id>"`, built from the record alone, with no read of the chain.
  - The list comes back at once, and each picture loads on its own from `GET /ord/<chain>/<id>`.
  - That route serves image types only, sandboxed, with `nosniff`.
- Only this computer's UI gets links (`explorer::handle_with`).
  - Answers from other arcades over the mesh keep the bytes inline, so a link never points at the wrong machine.
  - So do delegates and images over 96 KB.
