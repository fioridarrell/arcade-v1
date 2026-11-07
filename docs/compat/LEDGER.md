# Arcade ledger (Omni port): engine, tokens, MetaDEx, fingerprints

From the existing arcade (state.py, indexer.py, index_digest.py, consensushash.py, mesh/indexcheck.py) at
c69f944e0, via the Arcade v1.0 session, 2026-11-07. NFTs (INSC) and tags are in INSC.md.

## 1. Per block (indexer.py:106-175)
- names_only = names_from <= h < activation_height (testnet: both 1,496,133 → never). Below the floor and not names-only: skip.
- ONE Engine per block (its `_emptied` map is per block).
- For position, tx in block (coinbase = 0): extract → TxError: no row, skip; no marker: skip.
  From cancels_last_from (test 1,513,300): types 26/27/28 (decodable) are deferred to the end of the block, in order.
  arcade_tx.position stays the real position.

## 2. Engine.process
```
message = P.decode(payload)            # raises (HALT) for <4 bytes, truncated field, unknown type, out-of-scope 2,10,11,12,15,31,40,51,52,53
if names_only and not name message: return (no row)
try _apply  except InvalidTransaction: valid=0, reason
INSERT arcade_tx {txid, block_height, position, encoding_class "B"/"C", message_type, message_version (header u16 BE each),
                  sender, reference (NULL if none), payload_hex (lowercase, FULL payload incl. class-B padding), valid 0/1, invalid_reason}
```
Decoder: big-endian, no version check (except type 3 v0), no trailing-bytes check, cstr to NUL or end, utf-8 "replace", no 255 cap.
No per-tx savepoint: writes before an InvalidTransaction stay (token handlers check before writing).
Dispatch: 0, 4, 50, 54, 55, 56, 70, 65534, 65533, 65535 (no-op valid), 200 AnyData, 25, 26, 27, 29.
Anything else that decodes → InvalidTransaction("message type {T} is recognised but not yet implemented (arrives in a later milestone)") (28 included).
MAX_AMOUNT 9223372036854775807. Ecosystems 1, 2. Non-fungible type 5. Property types {1,2,65,66,129,130,5}. Coin = property 0.

### Payload layouts (after version u16 BE, type u16 BE)
0 SimpleSend: prop u32, amount u64 · 4 SendAll: eco u8 · 25/26: sale u32, sale_amt u64, want u32, want_amt u64 ·
27: sale u32, want u32 · 28: eco u8 · 29: prop u32, amount u64, order[32] ·
50: eco u8, type u16, prev u32, cstr category, subcategory, name, url, data, amount u64 · 54: same without amount ·
55/56: prop u32, amount u64 (+ optional cstr) · 70: prop u32 · 65534: feature u16, block u32, minver u32 · 65533: feature u16 ·
200: rest. (3: prop u32, amount u64, v1 adds u32; 5: prop u32, start u64, end u64; 20, 22, 71-74, 185, 186, 201: decode, not implemented.)

## 3. Balances
credit(addr, pid, amount, bucket="balance"): 0 → nothing; new = cur + amount < 0 → "{bucket} for {address} property {pid} would go negative";
update or insert (all four buckets 0 except this). debit = credit(-amount). Rows never deleted.
_move_to_reserve(addr,pid,amt,col): balance -= amt, col += amt (no check). _move_from_reserve: balance += amt, col = max(0, col-amt).

## 4. Property ids
next(eco): floor 3 (eco 1) or 0x80000001 (eco 2); top = MAX(id) in eco; none → floor; else top+1 skipping 1,2; max(candidate, floor).
Testnet tokens are ecosystem 1: ids 3, 4, 5, ...

## 5. Token handlers (checks in order, exact strings)
- **0**: no reference → "simple send has no reference (recipient) address"; amount not in 1..MAX → "amount {a} out of range";
  no property → "property {pid} does not exist"; type 5 → "property {pid} is non-fungible; use send type 5";
  balance < amount → "insufficient balance: {sender} holds {available} of property {pid}, needs {amount}". debit, credit.
- **4**: no reference → "send all has no reference (recipient) address"; eco ∉ {1,2} → "invalid ecosystem {eco}";
  rows = sender balances > 0 in eco, ORDER BY property_id; none → "{sender} holds nothing in ecosystem {eco}". Free balance only.
- **validate issuance (50, 54)**: "invalid ecosystem {eco}"; "invalid property type {t}"; empty name → "property name must not be empty";
  name_complaint: strip → empty "a token name must not be empty."; first char outside 0x20..0x7E →
  "a token name uses ordinary letters, digits and punctuation -- {odd!r} is not one of them. Characters from other alphabets can look
  identical to these on a page, and a name that looks like another token's is the one thing a name must not do.";
  name_key empty → "a token name needs at least one letter or digit."
  name_key = lower(), keep a-z and str.isdigit() chars.
- **50**: validate; "amount {a} out of range"; name_key unique in the SAME ecosystem → "the name {name!r} is already a token on this chain";
  insert (issuer sender, managed 0, total amount, strings raw); credit sender.
- **54**: validate; unique; insert (managed 1, total 0). Reference and previous id ignored.
- **55**: range; missing → "property {pid} does not exist"; not managed → "property {pid} is not managed; its supply was fixed at creation";
  sender ≠ issuer → "{sender} is not the issuer of property {pid} (issuer is {issuer})"; amount > MAX - total →
  "granting {a} would take property {pid} past the 9223372036854775807 tokens that can ever exist"; credit (reference or sender); total += a.
- **56**: range; managed checks; type 5 → "property {pid} is non-fungible"; held < a →
  "insufficient balance: {sender} holds {held} of property {pid}, cannot revoke {a}"; debit; total -= a. No issuer check.
- **70**: missing; type 5 non-fungible; not issuer; no reference → "change issuer has no reference (new issuer) address"; issuer = reference.
- **65534/65533**: no authority on any net; past block → "activation block {ab} is in the past (current height {h})"; never-activated →
  "feature {fid} was never activated". Nothing reads the table.

## 6. MetaDEx
book_order(txid PK, height, position, address, sale_property, sale_amount (left), want_property, want_amount (for what's left), reserved).
- **25**: sale == want → "an order has two different sides"; both tokens: before token_pairs_from (test 1,529,092) →
  "one side of an order is this chain's coin (property 0)", else _pair_order. Range sale then want ("amount {x} out of range");
  token = want if sale==0 else sale; missing → does not exist; sale≠0: available < amt → "{sender} holds {available} of property {sale}, not {amt}",
  move to metadex_reserve, reserved = amt (coin bid: reserved 0). INSERT.
- **_pair_order**: ranges; both props exist; available check; move a to reserve; rows = book where sale=want(ours) and want=sale(ours),
  address ≠ sender, amounts > 0; sort by (Fraction(want_amount, sale_amount), height, position, txid). left=a, seq=0. For r:
  ma=r.want, mb=r.sale; stop if ma*b > a*mb; got = min(mb, left*mb//ma); gave = ceil(got*ma/mb); while got>0 and gave>left: got-=1, recompute;
  got<=0: continue (no seq). debit sender sale gave (metadex_reserve), credit maker sale gave; debit maker want got (metadex_reserve), credit sender want got;
  rest = mb-got: 0 → delete maker; else sale=rest, want=max(1, ceil(rest*ma/mb)), reserved -= got.
  INSERT pair_trade(taker txid, seq, h, pos, taker, maker, maker_txid, gave_property=sale, gave, got_property=want, got); seq += 1; left -= gave.
  rest_want = ceil(left*b/a); left>0 and rest_want>0 → INSERT taker order (sale, left, want, rest_want, reserved left).
- **26/27**: rows = sender's orders sale/want; 26 keeps rows with price[0]*row.sale == price[1]*row.want where price=(want_amt, sale_amt);
  none → "no order of yours matches that"; each: unreserve, delete.
- **29 take**: before take_from (test 1,496,133) → "an order is taken with its maker's signature until this block";
  range; missing; type 5 → "property {pid} is non-fungible -- a take moves a balance the way a fill does, not one piece by its id";
  row = book[order.hex()] (payload bytes as displayed); none and h >= cancels_last_from → engine._emptied[order];
  none → "that order is not on the book -- filled, cancelled, or its block has not reached this node";
  own → "an order is a promise to somebody else; taking your own withdraws it, and type 26 or 27 says that";
  row.sale ≠ pid or row.want ≠ 0 → "that order sells {sale} for {want}, and holds nothing of property {pid}";
  graceful = h >= cancels_last_from; from_book = min(amount, reserved); short and not graceful →
  "that order holds {reserved} of property {pid}, not {amount}"; from_free = max(0, min(amount-from_book, maker free));
  need = want*amount//sale; need < 1 → "that much of this order is worth less than a satoshi; take more";
  paid = paid_to(maker) (net); paid < need → "{maker} is paid {paid} satoshis by this transaction; that order's price makes it {need}";
  from_book → _fill_order(row, from_book, want*from_book//sale); moved = from_book+from_free; debit maker, credit sender;
  INSERT take_trade(txid, pid, tokens=moved, coins=paid, maker).
- **_fill_order(row, taken, want)**: unreserve taken; left = sale - taken; ≤0 → delete, _emptied[txid] = row with reserved 0;
  else sale=left, want=max(1, want_amount - want), reserved -= taken.

## 7. index_digest
Records `table|v1|v2|...\n` (None → "", ints decimal, valid 0/1, payload_hex lowercase, "|" not escaped):
arcade_tx(txid, position, encoding_class, message_type, message_version, sender, reference, payload_hex, valid);
inscription(txid, number, creator, position, content_type, content_len, sha256, chunks);
inscription_move(txid, inscription, from_address, to_address, position, how);
pair_trade(txid, seq, position, taker, maker, maker_txid, gave_property, gave, got_property, got);
nft_offer(txid, position, inscription, buyer, take_kind, take_property, take_amount);
nft_ask(txid, position, inscription, seller, take_kind, take_property, take_amount);
property by creation_block (property_id, ecosystem, property_type, issuer, category, subcategory, name, url, data, managed, creation_txid).
**Each block's records are sorted as strings**, joined, sha256 (utf-8). range_digest(lo,hi) = sha256(f"{lo}-{hi}\n" + f"{h}:{digest}\n" ...).
checkpoint = (indexed-6)//100*100 if >= floor (test 1,496,133).

## 8. consensus_hash
sha256 over records with no separators, ascii: balances (address bytewise, pid; skip all-zero) f"{a}|{pid}|{bal}|{sell}|{accept}|{metadex}";
book_order sale_amount>0 by txid f"{txid}|{address}|{sale}|{sale_amt}|{want}|{want_amt}|{reserved}"; properties eco 1 then 2 by id f"{pid}|{issuer}".
Vector (testnet, last arcade tx 1,561,561): a03774bfa3811e032f67e72d1d0626fddd74dc0adc6477fece8df502bad1b33c; 577 records;
balances f7732455…, metadex 51c083e6…, properties dd05f80c….

## 9. indexcheck (ix)
tip {nets:{net:{h,at,d,s,n}}}; q {t:"ix",op:"q",id,net,what:"d",lo,hi} → {op:"a",id,d}; q what:"rows", h → {op:"a",id,rows:[≤200, each ≤600 chars, "\n"-ended]}.

## 10. Testnet vectors
- 1496149 digest 8e722d69e99b2125091c437803d99e6f867a5a60ec91bce481cd3182110f8c64 (first issuance, property 3 "Arcade", class B padded).
- 1512583 digest 27bdc2d5455035d27b8f2dffc0997371f13ebed228bb22d89928e8cb0d8c082e (three type-25 asks; rows sort 1,3,2).
- 1513150 digest 0d4ed8bdb9d595afea6a9b5113a147f83b7e0cd3a8fd09b8789a3542af7cc6e9 (take; take_trade tokens 60000000000 coins 600000000).
- 1529099 digest 2298037cf45949c7a72fc58cfb7310e0411f028428829ae469714e311c77db17 (pair trade 26 for 71).
- Counts (valid/invalid): 0 3245/5, 25 69/0, 26 1/0, 27 4/6, 29 107/0, 50 16/0, 54 102/0, 55 3179/0, 56 2/0, 70 6/0, 200 31877/2069.
