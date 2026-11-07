# ord / Pepinals and Doginals (from the existing arcade, via the Arcade v1.0 session, 2026-11-07)

Code: ord_rules.py, doginal_index.py, ord_notes.py, ord_values.py, ord_snapshot.py. Not ord_envelope.py (that is
Litecoin ord-lite). The ledger fingerprints (index_digest, ix) do NOT cover ord; ord has its own (dx, §7).

## 1. Envelope: input 0 scriptSig
dialect = "pepecoin" for main/test, else "dogecoin".
Layout: "ord", N (pieces), content type, then (N-1, data), (N-2, data) ... (0, data), trailer pushes, sig, redeem.
decode_push_datas: op 0 -> b""; 81..96 -> bytes([op-80]); 1..75 direct; 76 PUSHDATA1; 77 PUSHDATA2 (pepecoin:
LE length; dogecoin: wonky-ord quirk len = b[1]*256 + 0x4d); 78 PUSHDATA4 (pepecoin LE; dogecoin quirk
b[3]<<24|b[2]<<16|b[1]<<8|b[0]); any other opcode -> the whole script is None; a short push -> None.
push_number = None if len > 8 else int.from_bytes(little) (b"" = 0).
_start: pushes[0]=="ord", npieces=push_number(pushes[1]); npieces 0 -> complete with empty body (pepecoin only);
pieces must count down exactly N-1..0; left==0 -> complete else partial.
_more: continuation script's first push must be npieces-1.
Trailer tags only from the script where the countdown ends: pepecoin pairs (utf-8 key, value) over range(0,len-1,2),
non-utf-8 keys skipped; dogecoin pairs while key is 1 byte.
Births: input 0 only (PEP/DOGE), n = 0; coinbase skipped.

## 2. Multi-tx pieces (pending)
pending(txid PK = latest piece, ctype, left, body, size, pieces, height, origin = genesis txid).
Every non-coinbase tx: chain = pending[vin0.prevout.txid] (ANY vout). No chain -> start; chain -> more (and the
script is read ONLY as a continuation). complete -> delete pending[prev], id = origin+"i0", insert unless exists.
partial -> move pending to this txid. "none" removes nothing. No timeout, expiry or piece limit.
number/height/txpos are the COMPLETING tx's; id is the GENESIS txid i0. size = all body bytes; pieces = scripts.

## 3. insc
tx = genesis txid as DISPLAY bytes; number = next_number from 0, +1 per completed inscription (block, tx order);
rid = number + 1; ct via ctypes (utf-8 "replace"); proto = protocol_of(body): body.lstrip()[:1]=="{" and len <=
65536, json.loads ok, dict with non-empty str "p" -> p[:32]. **No sha256 stored.** No cursed numbers on PEP/DOGE.

## 4. Extras
charms: only Litecoin. burned = height when it landed past all outputs (OP_RETURN does NOT burn on PEP/DOGE).
delegate: first "delegate" tag (dogecoin key b"\x0b"); blob = 32 display bytes + u32 LE n.
  pepecoin id_from_tag: len 36, f"{value[:32][::-1].hex()}i{u32le(value[32:36])}".
parent (pepecoin): "parent" values parsed as ids; kept only if the parent is indexed and carried by this tx;
  single-tx envelopes only; first valid stored; every valid parent moves to output 1 offset 0 (if >1 outputs).
reinscribed = 1 if an old inscription sits at input offset 0.

## 5. Starts
main 186,920; **test 34,237**; doge 4,600,000; bel 22,470; lky 88,554; wjk 1; dgb 0; flop 18,295.
Rules version 4 (doge 5, bel 7, ltc/ltctest 6). Chunks = (h - base)//100.

## 6. Location (at)
carried: per input j, base = sum of earlier input values, p = base + old_off; births (rid, 0, new).
Stable sort by p (input order kept; new births last). Land in first output k with p < cumulative end;
off = p - end[k-1]. Past all outputs: delete at, burned = h, move "X {iid}". No dust rule.
Moves: "B {iid} {txid}:{k}:{off}\n", "M ...", "X {iid}\n".

## 7. Fingerprints
block line = sha256(block_hash_hex + "\n" + for each genesis seen (incl. partial/duplicate):
  f"{txid}i{n}:{vin}:{ctype_hex}\n"); n = count; moves = sha256(concat move lines).
chunk_hash = sha256(concat 100 lines as hex). dx rows [chunk, chunk_hash, births, nd];
nd = sha256(b"ord-notes\n" + notes) — requires porting ord_notes.py exactly.

## 8. Names and tokens
names: text/plain or application/json; JSON p=dns op reg/register, or first word with a dot; spec/json/doge2023
columns; lowest number wins. Tokens prc-20 (main/test) / drc-20: body >= 40 chars, JSON p, op deploy/mint/transfer,
tick 4 UTF-8 bytes, amounts ^[0-9]+(\.[0-9]+)?$ <= 18 decimals; only new single-piece landed inscriptions.

## 9. Vectors (Pepecoin MAINNET, ord-main.sqlite)
id | number | height | txpos | ctype | proto | size | pieces
6316281cccc1c95295aa7301b4f3a890f7e7d535a9e07f8511a3dc8ff20fff1bi0 | 0 | 186920 | 2 | text/plain;charset=utf-8 | prc-20 | 63 | 1 (at vout 0 off 0)
e1f045d8b5ae0c8caeb518eb05a20faeedb35897964f3486a86766caa3248824i0 | 1 | 186923 | 6 | text/plain;charset=utf-8 | prc-20 | 54 | 1
8541ade0622e7e02915bf566af4b264fad0d78bc5ea8c5179dc4eb4fffd428b6i0 | 2 | 186927 | 3 | "text/plain; charset=utf-8" | - | 7 | 1
d2681c731c6ee85d8923d31f2b8eb09b7a3c69a5df0b26e246054067c5f30fddi0 | 146 | 262731 | 43 | image/png | - | 8861 | 7
d994f3b928fb7fb52159cb32af8d8503bcbd97a47ac31332c97e88c9f175aaa7i0 | 147 | 262731 | 53 | image/png | - | 7398 | 6
d85ed8a801833a77ec0c8060f5978fe3b73fc1a8a3fb22b08a5651f1fa4e35aai0 | 17271936 | 965783 | 2 | "" | - | 0 | 1 (delegate 768e65ae…a9a4i0)
2671648978ef4e4730f99af347f940b36995346bacdeb30dc3008968a70ccc3ei0 | 17267824 | 964741 | 2 | text/plain;charset=utf-8 | - | 21 | 1 (parent cd5ba6aee9f4d8479b5c873092b976bf2b9a06fe6fe5093f3d0b79d313d77e79i0)
