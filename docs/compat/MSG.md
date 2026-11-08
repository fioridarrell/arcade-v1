# Messaging (old arcade, c69f944e0) — what V1.0 must match

From the Arcade v1.0 session's read-only reading of the old source (messaging/envelope.py, seed.py
175-273, derive.py, keys.py, sender.py, scanner.py, content.py, store.py, group.py, feed.py,
feedview.py, broadcast.py, release.py, instance.py, mesh/announce.py, gamestate.py,
web/templates/messaging_js.js, signin.js 141-163, web/app.py 25841-26060, 29349), 2026-11-08.

## 0. Scope
- Encrypted messaging is **Pepecoin testnet only** (the tag chain). Public types may be read on other
  chains (public_only). **Nothing is relayed over the mesh**: private messages exist only on chain.
  The only off-chain piece is the browser's encrypted mailbox history (§7).

## 1. Keys
- BIP39 seed = PBKDF2-HMAC-SHA512(NFKD phrase, "mnemonic"+pass, 2048, 64).
- Tree (NOT BIP32): master I = HMAC-SHA512("Bitcoin seed", seed), k = I[:32], c = I[32:].
  Child (hardened only, index |= 0x80000000): I = HMAC-SHA512(c, 0x00 || k || ser32_be(index)); k = I[:32], c = I[32:].
  The left half IS the child key (no "+ k mod n").
- Messaging X25519 secret = k at m/24946'/0'/0', used as-is (X25519 clamps in scalarmult).
- Login Ed25519 seed = k at m/24946'/1'/0' (pub = account id). Messages are not signed with it.
- Fingerprint = hex(sha256(x25519_pub)[:8]) in four groups of 4 separated by spaces.
- Node identity (derive.py): secret = HKDF-SHA256(salt 32×00, IKM = base64dec(Core signmessage(identity_address,
  "DogecoinArcade messaging identity v1")), info "arcade-x25519-identity-v1", 32).

## 2. Envelopes ("arcm" 01 type; big-endian; version ≠ 1 rejected)
| type | layout after "arcm" 01 type | notes |
|---|---|---|
| 1 SINGLE | clen u16 · ciphertext[clen] · (Class B NULs) | bound header = first 6 bytes |
| 2 CHUNK | msg_id8 · clen u16 · countdown u16 · piece | bound = 14 bytes (with msg_id); countdown not bound |
| 3 KEY_ANNOUNCE | x25519 pub32 · tail | tail: 0x01 h160+nlen+name (first, always) · 0x02 h160 · 0x03 len+tag · 0x04 pfp txid32 · 0x05 len+bio ≤160 · 0x06 len+url ≤120 http(s) · 0x07 doge h160 · 0x08 nlen(1..16)+net+h160 (repeatable, sorted) · stop at unknown, NUL = padding |
| 4 GROUP (public post) | chan_len chan(≤24, "" → "main") · nick_len nick(≤24) · rest | rest = UTF-8 text (rstrip NULs) or "\x00ARCF" text_len u16 · text · name_len name(≤120) · type_len type(≤60) · file bytes (≤1 MiB) |
| 5 GROUP_CHUNK | msg_id8 · **countdown u16 · clen u16** · piece | order differs from type 2; rejoined = full type-4 payload |
| 6 API | as SINGLE | plaintext = stamp8 ("D","A",1, api fp4, 0) + JSON |
| 7 FEED_ACT | kind1 · target32 · text (kinds 3,4,5,10; ≤2000) | target = txid bytes as displayed (not reversed) |
| 8 RELEASE | ASCII git revision ≤40 | newest kept; no authority |
| 9 INSTANCE | "arcade-instance <domain> <rev>" | |
| 10 MESH | "arcade-mesh <host> <port> <key64>" or "arcade-mesh-ws <wss-url> <key64>" | |
| 11 GAMESTATE | JSON {"f": family, "u": [[piece_txid, seq, state], …]} | publisher's own, highest seq |
| 12 BROADCAST | JSON {"t": topic, "b": body}, ≤7000 | confirmed only |

Chunking (types 2, 5): capacity 7650−4−18 = 7628; count = ceil(len/7628); even = ceil(len/count);
countdown = n−1−i (last = 0). Private: sealed once, ciphertext split; reassembled by msg_id per sender.

## 3. Crypto (libsodium)
- inner = crypto_box(sender_sk, rcpt_pk, bound_header || message), nonce24 random, prepended (nonce || mac+ct).
- envelope = sender_pub32 || inner; ciphertext = crypto_box_seal(rcpt_pk, envelope); payload = header(clen) || ciphertext.
- SINGLE overhead 134 bytes. Open: seal_open → sender = env[:32] → box_open(env[32:]) → must start with bound header.
- The sender's own copy is not on chain (V1.0 adds a second SINGLE sealed to the sender's own key).
- Body (content.py): plain UTF-8, or b"\x01ARCB" ver 1 · json_len u16 · JSON {"text"?, "file"?: {name,type,size},
  "profile"?: {...}} · file bytes. Unknown/bad → shown as plain.

## 4. Addressing
- Nothing names the recipient: readers **trial-decrypt** every type 1/2/6 payload.
- Browser sends add a 1,000,000-sat output to the recipient (node sends don't; V1.0 doesn't).
- Recipient key = newest key announcement for the address. **V1.0: a stated hash160 counts only if it equals the
  announcement's sender** (the old arcade accepts any claim; see the spec's warning).

## 5. Carrying
- Always Omni AnyData type 200. Class C (OP_RETURN "arcd" + payload, ≤76 bytes) when it fits, else Class B
  (marker P2PKH 0.01 from MARKER_SEED "DogecoinArcade Class B marker v1"; 1-of-3 bare multisig 0.01 each,
  2×31-byte packets (seq + 30), ≤255 packets = 7650 bytes; XOR with iterated sha256 over the sender address's
  uppercase hex; the sender's real pubkey in the first slot).
- Fee: MIN_FEE_PER_KB; size ≈ 148 + outs×113 + 34 + 34 + 10.

## 6. Sender
- Inner sender X25519 key (authenticated by the box; repudiable). On-chain sender = Omni sender (Class C first
  input; Class B largest input total per address, ties → smaller address). Inputs must be P2PKH/P2SH.

## 7. Groups, feed, store
- Private group = fresh X25519 keypair; id = hex(sha256(public)[:8]); invite = SINGLE to each member and self with
  "\u0000arcade-group/1\n" + {id, root, name ≤60, secret, public, members:[{tag,key,address}], creator, rotated_from,
  created, invited}. Line = SINGLE sealed to the group key, credited to from_address. Leave =
  "\u0000arcade-group-left/1\n" {"at"} from the member's own address. Removal = new keypair with rotated_from.
- Feed act kinds: 1 LIKE 2 UNLIKE 3 REPLY 4 SHARE 5 EDIT 6 DELETE 7 TIP 8 DISLIKE 9 UNDISLIKE 10 TIP_TOKEN.
  Confirmed acts only. DELETE/EDIT count only from the post's author (latest wins). Opinions: latest per person.
  SHARE counts unless its author deleted the share txid. TIP = sum of outputs to addresses ≠ sender, confirmed,
  not self. TIP_TOKEN = a valid token send from the act's author to the post's author. Replies threaded by target.
  Score = likes − dislikes + shares + Σ(2 + log10(1 + sats/1e8)); hot = (1 + score)/((age_h + 2)^1.5).
- Mute hides words only (counts/order kept). seen_mark "board:<net>[:<chan>]" = last read post id.

## 8. Vectors
- A = "abandon ×11 about": x25519 secret 101194f1…1cd0, pub b4947c1f…3544, fp "ef63 b457 a79f c4ea";
  ed25519 seed 3c26a9f3…1e2d → pub 07c4f407…5f4e.
- B = "legal winner thank year wave sausage worth useful legal winner thank yellow": x25519 secret
  2f1d6de7bf8fd27f13c23838bb8683d6d39f2282f928c4a0aa808d069a493a70, pub 19e6c1f4c3c0c270d3dab8a136eac869513acb7750a9c061b077c1b03c4ecf27.
- A full: secret 101194f123531ce709dafe62ade9b218cb2059437fd8b2eac9dc38c8ef3b1cd0,
  pub b4947c1ff461daea13aebca4da38e441bd07cf755f70e2131a0e74606ace3544.
- SINGLE B→A "hello from B" (opens with A's secret):
  6172636d0101008ac5916add42fcef56fc986fe956ec8b0e8619b834453b619d7d99604e8683643be5bf4be4a2447020497f474964f04e2fa3342df90a140c102e57f581fcd85393f8e8ad79abd812a105b55d2c3f38fada1a5faa39c78eab5dea4518f2c2eef0805cdcf526a2aae66c63d8a7ee99de208a0971f206c8580022af674059dfa14fa765169933cf87ff39aa21
- Announcement (A, h160 = 0, name "Alice", tag "alice"):
  6172636d0103b4947c1ff461daea13aebca4da38e441bd07cf755f70e2131a0e74606ace354401000000000000000000000000000000000000000005416c6963650305616c696365
- Post "gm": 6172636d0104046d61696e00676d
- Post with nick "bob", "hi", file a.txt text/plain "XYZ":
  6172636d0104046d61696e03626f6200415243460002686905612e7478740a746578742f706c61696e58595a
- LIKE of 2687c607…4ec6: 6172636d0107012687c60738028c944f3a4947eaa4c63c374a248c064846b78264ce82da164ec6
- REPLY "nice": 6172636d0107032687c60738028c944f3a4947eaa4c63c374a248c064846b78264ce82da164ec66e696365
- Testnet: SINGLE 2687c607…4ec6 (1,561,537), 64d853bf…418d (4,736 bytes); API 0febdab8…6725; posts 260031be…fe10,
  0c87d0ca…6fed; LIKE 2dee0901…4f49; REPLY d54d8759…a770; SHARE fe5589f3…d6ec; DELETE f415ebbe…8b4d;
  TIP 182ddfec…ef4e (10 tPEP); TIP_TOKEN 210ebea7…4982; BROADCAST 2b0f3fd6…bc40; INSTANCE d147090e…964c; MESH 13819e88…7567.
