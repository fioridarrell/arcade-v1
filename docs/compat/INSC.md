# INSC: the arcade's own inscriptions (inscriptions.py, state.py; via the Arcade v1.0 session, 2026-11-07)

Carried as Omni AnyData (type 200): 00 00 00 c8 || data (version not checked). Class B keeps trailing NUL padding.
data[:4]=="INSC" and len>=6 -> inscription; data[5] in (3,4) -> tag; else parse kind. Invalid -> arcade_tx valid=0
(valid IS fingerprinted; invalid_reason is not). **No per-tx rollback; a StateError halts the indexer.**

## Layouts (big-endian). Header "INSC" | version u8 (=1) | kind u8
| kind | body after header | trailing bytes |
|---|---|---|
| 1 chunk | id[8] countdown u16 clen u16 body[clen] | ignored (padding OK); short body -> error |
| 2 transfer | txid[32] | >= 38 bytes total, rest ignored |
| 3/4 tag | len u8 tag[len] | ignored (>= 7 bytes) |
| 5 swap | give Leg, take Leg, [order txid 32] | remainder exactly 0 or 32 |
| 6 offer | txid[32] take Leg | must end exactly |
| 7 ask | txid[32] (00 \| take Leg) | cancel = exactly 39 bytes; else exact |
| 8 bundle | a[21] b[21] n_give u8 (1..64) legs n_take u8 (1..64) legs | only zero bytes after |
Leg: 01 txid32 | 02 prop u32 amount u64 | 03 sats u64; other kinds -> "unknown swap leg". Party = ver + hash160.
=> class-B-padded swaps/offers/asks are malformed; chunks, transfers, tags, bundles survive padding.

## Chunks and Manifest
Manifest stream: total u32 | sha256[32] | ctype_len u8 | ctype | json_len u16 | json | content...
Writer: room = capacity(7646) - 18; count = ceil(len/room) <= 65536; FIRST piece = HIGHEST countdown (has manifest),
last piece countdown 0; id = 8 random bytes.
complete() = 0 in pieces and set == range(highest+1). join() = pieces highest..0, decode manifest, content[:total];
len != total -> error; sha256 mismatch -> error.
_inscription_chunk:
1. key (sender, id hex, countdown) already present -> "already on chain" (invalid).
2. INSERT chunk row (never rolled back).
3. not complete -> valid, done.
4. complete: join(); error -> invalid "does not assemble", rows STAY (out-of-order arrival makes intermediate
   pieces invalid=0; the last piece completes validly).
5. INSERT inscription: txid = manifest piece's (highest countdown) txid; number = MAX+1 from 0; creator = owner =
   sender; height/position = completing tx; content_type, content_len = total, sha256 hex, json, chunks.
6. inscription_piece rows; 7. collection_item via collection_of + joins_collection; 8. DELETE chunk rows.

## Handlers (testnet gates: swaps/asks/fills/named_fills/bid_fills/names 1,496,133; bundles 1,527,064;
chunks/transfers/offers/tags ungated)
- transfer: exists; owner == sender; has reference; reference != owner -> owner = reference, move "transfer".
- swap: class C only; seller = sender; buyer = first other input address; fills + bid fills; check legs; move.
- bundle: a != b; both sign; no third-party inputs; legs checked first; coins one way only.
- offer: inscription exists, not yours, non-empty -> nft_offer (never consumed by the engine).
- ask: gated; holder only; coins or token; cancel = take_kind 0 -> nft_ask (liveness decided when read).
- collection_of: Python json/regex/Unicode semantics (see message 2026-11-07); supply from edition 1 (<= 1e7).

## Real testnet rows (test-ledger.sqlite): txid | number | creator | height | pos | ctype | len | sha256 | chunks
874735723caef105072611b538f69e8bb501231f94c3e8703cd8d73191dbc7df | 0 | ncpXrSCQx667v4y92zSWidxs96N5TtPjTE | 1496139 | 1 | image/png | 5717 | 5cc2abe5dda709388d8067f02c00494690f9ed5b26ad2f22560dbff60332959b | 1
4c17ac06c8ff46abce4a33273fb095ee459cb82113d889293fb7c837e42a1211 | 1 | ncpXrSCQx667v4y92zSWidxs96N5TtPjTE | 1496151 | 1 | text/html | 4820 | 74d3a7e6c22302b1c8f76d2cdc8c11f027e82aa6b6c4ae1dbcf84111a5da5d24 | 1
c3c536666af104cf92877b4d888866645e66a56738f4f4950f1dd14e815dea3a | 2 | nXFzDiKRvX1CeS59XnV4YNhSSDBNBjaaBp | 1504673 | 3 | image/png | 96 | eccb66c0de6149afdafee7143540b6fd673d33fcb86ff430c3427f82c7bbd3f0 | 1
e8ec70cf9da44249556eef082f426fc9e3a59e0b3716139308337d9783f7f0df | 15 | | | | image/png | 43393 | 92625448cbf2c7b510fbd2e8cc424bb09bffbd98850ebe437d4a248506e7fce5 | 6
f1613f4290d885b912f6ef8830335e3f9a8e901c1a2a8f12fb3a1728d8defece | 17 | | | | image/png | 43393 | d92b67df8e5db7c59ef294b82160878de1f2c9b15fd30f64dd71403a20320dfa | 6
