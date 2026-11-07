# Compatibility spec: the existing (Python) arcade

Received 2026-11-07 from the "Arcade v1.0" session, read from `~/.dogecoinarcade/src/arcade/` on the GX10
(commit 6f4b62314), Pepecoin Core master 4fb5a0cd93 and the GX10's live testnet node. Paths are relative to
`arcade/`. Points marked **!!** are traps that need a deliberate V1.0 decision, not a silent copy.

## 1. Pepecoin testnet (encoded in `crates/arcade-chain/src/params.rs`)

- **The Python arcade does NOT speak Bitcoin P2P.** It reads chains via local `pepecoind` RPC, raw blocks over the
  mesh (`cr` coreshare), or blk*.dat. `corelink` (`cs`) only tunnels raw Core P2P bytes. V1.0's P2P client talks to
  Pepecoin Core peers directly.
- Magic `fec1dbcc` (main `c0a0f0e0`); P2P 44874 (main 33874); protocol 70016; services 0x05; UA `/pepetoshi:1.1.0/`.
- DNS seeds `seeds-testnet.pepecoin.org`, `seeds-testnet.pepeblocks.com`. Live peers: 3.216.226.159, 34.198.62.248,
  135.181.168.83, 207.244.232.43, 147.124.204.26 (all :44874); Martin's LAN node 192.168.1.25.
- Genesis `f9f4ea4a…6fc52`, header `01000000…940d0000` (verified in tests). Checkpoint 42001 `1b111a1e…5cd1`.
  nMinimumChainWork 0x4813725c089e. Tip ~1,561,523.
- Addresses test: P2PKH 0x71, P2SH 0xc4, WIF 0xf1, BIP44 coin 1. Main: 56 / 22 / 0x9e / 3. No bech32.
  Signed-message magic `"Pepecoin Signed Message:\n"`.
- Consensus (Core): Digishield, 60 s spacing; min-difficulty blocks allowed at 0-999 and from 1250; AuxPoW allowed
  from 42000, chain id 0x3f, not strict. BIP34/65/66 at 1000. No SegWit. Halving every 100000. Coinbase maturity 240.
- Live testnet headers: version 0x003f0004 (chain id, **no** AuxPoW flag) — plain scrypt headers. Tip bits 0x1e0fffff.
- Arcade header check (rawchain.py): scrypt PoW; block hash sha256d; **MAX_EASIER = 4** (target ≤ 4× parent;
  min-difficulty chains: any target ≤ powLimit is OK); prev link. No checkpoints, MTP, future-time, min-chainwork.
- AuxPoW (main): Dogecoin layout. Arcade checks: flag⇒data, chain id via mask, coinbase index 0, coinbase in parent
  merkle, chain branch ≤ 30, root after `fabe6d6d` (or ≤ offset 20 without it), scrypt(parent) ≤ target.
- Test vectors: `tests/data/rawchain/blocks.json.gz` (test 1550240-1, main 1255107-8 with AuxPoW).

## 2. Mesh protocol

Traps: signatures cover `json.dumps(h, separators=(",",":"), sort_keys=True)` with ensure_ascii (non-ASCII as
`\uXXXX`); frame heads are `json.dumps(head, separators=(",",":"))` **unsorted**; unknown `t` kinds are ignored.

- Transports: TCP port **8421**; WebSocket path `/mesh` (binary messages, client-masked, max 128 KiB);
  optional UDP (AUDP1, XChaCha20-Poly1305) only after a link is up.
- Identity: Ed25519 seed in `~/.dogecoinarcade/mesh.key` (hex + newline, 0600). Node id = hex verify key.
- Network string `arcade-{name}`. **Verified live 2026-11-07: every public arcade answers `arcade-main`,
  and that one mesh carries all chains (SUB `res` lists `c:test`, `c:dgb`, `c:kot`, ...). V1.0 joins `arcade-main`.**
- Handshake (mesh/link.py:287-334), VERSION 2, MAGIC `ARCM`, 10 s timeout:
  1. Both send HELLO in clear: u32 BE len + `ARCM` + [2, len(net)] + net + 32-byte X25519 ephemeral pk.
  2. shared = X25519. (first, second) = (mine, theirs) if dialer else (theirs, mine);
     transcript = blake2b(first+second, 32) where first/second are the **full HELLO bodies**
     (magic..ephemeral), dialer's first — verified against a live arcade 2026-11-07.
  3. derive(label) = blake2b(shared+transcript, 32, person=label padded to 16 with NUL).
     k_out = derive(`arcm-dialer`), k_in = derive(`arcm-listener`); dialer sends with k_out, listener with k_in.
  4. Encrypted AUTH `{"t":"auth","key":hex,"sig":hex,"listen":port|null}`;
     sig = Ed25519(`arcade-mesh-auth-v1` + role(`D`/`L`) + transcript). Refuse own key. Lower id dials on IPv6 meets.
- Framing: u32 BE len + body (max 64 KiB). body = XChaCha20-Poly1305-IETF(plain, no AAD, nonce = counter as 24-byte BE).
  Plain = u16 BE head len + JSON head + blob. Binary `pub` form starts `FF FF 01` (link.py:240-280).
  SEND_TIMEOUT 30 s, DEAD_AFTER 45 s.
- Core frames, sent on link-up in order: addrs, own sub, latest SUBs, extension link_up frames.
  - `ping {n}` after 1 s then every 15 s → `pong {n}`.
  - `addrs {a:[{h,p,id}]}` ≤ 50.
  - `sub {o,q,rooms,hops,res?,sig}` flooded; room id = blake2b(room, 8) hex; signed over canon(all but sig/hops/d);
    drop at hops ≥ 16; 4/s, burst 10 per origin.
  - `pub {r,o,q,m,k,hops,c?,d?,sig}` signed over canon(signed part)+data, routed back along SUB path.
  - `dialback`, `rl` relay (RELAY_HOPS 3), `ka`, `ud`.
  - q = µs since epoch, strictly increasing, |q−now| ≤ 60 s; dedup `o:q`.
  - Limits: MAX_PAYLOAD 512, MAX_HERE 60 KB, MAX_ROOM 128, MAX_MEMBER 64, MAX_INBOUND 32.
- Extension kinds: `cs` corelink, `cr` coreshare, `ix` indexcheck, `lx` ledgershare, `wl` welcome, `op`, `cb`, `dx`,
  `kc`, `pk`, `iw`, `lm`, `fr`, `st`, `pu`, `ui`, `tu`, `cov`.
- **Seeds: none in code.** Installer default first peer `wss://app.dogecoinarcade.com/mesh`. Known: 156.67.28.112:8421,
  207.244.232.43:8421, 147.124.204.26:8421, `wss://pepecoinarcade.com/mesh`, `wss://app.dogecoinarcade.com/mesh`,
  192.168.1.25:8421 (LAN). Peer file kept 30 days, ≤ 200. Backoff min(600, 5·2^min(fail,7)) s.
  OUTBOUND_TARGET 6, EVERYONE_MAX 48.
- On-chain announcements (class B/C Omni tx, marker `arcd`, AnyData type 200, envelope `arcm` v1):
  - mesh (type 10): `arcade-mesh {ip} {port} {key}` or `arcade-mesh-ws wss://host/mesh {key}`; key `^[0-9a-f]{64}$`;
    IP must be global; signer = tx sender.
  - instance (type 9): `arcade-instance {domain} {rev}`.
- Coreshare `cr`: offer/sub/q/a/tx/gone; JSON-RPC batches (`[[method,params],...]`), zlib answers, CHUNK 48 KB.
  Serves getblock(h,0)/getblockheader/getrawtransaction/gettxout/mempool/sendrawtransaction/... Borrower verifies
  txid, block hash, merkle, PoW, MAX_EASIER.

## 3. Bootstraps, snapshots, fingerprints

- `bootstrap/<net>.sqlite.gz` + `.json`: **HTTP only** (`GET https://app.dogecoinarcade.com/bootstrap/{name}`),
  ledger index with private tables emptied, gzip 6. JSON: network, height, floor, consensus_hash, index_digest
  (**!! a different algorithm from index_digest.py**), sha256, bytes, made, file, what.
- `.snap` + `.snap.json` over mesh `lx`: plain SQLite; manifest {net, floor, activation, layout 1, height, hash, cp, d,
  s, n, size, piece 1 MiB, pieces[sha256], at}; id = sha256(sorted compact JSON of manifest). Fetched only when
  ≥ 50000 blocks behind. Install checks pieces, describe(copy) == manifest, local block hash, ≥ 2 indexcheck votes
  and a majority.
- **index_digest** (per block): rows from arcade_tx, inscription, inscription_move, pair_trade, nft_offer, nft_ask,
  property(by creation_block) as `table|v1|v2…\n` (None → ""), **sorted as strings**, sha256 hex. Empty blocks have
  no entry. range_digest(lo,hi) = sha256(`{lo}-{hi}\n` + `{h}:{digest}\n` for heights with rows).
  EVERY 100, SETTLED 6, checkpoint = (indexed−6)//100*100.
- **consensus_hash**: sha256 over records, no separators: balances (`address|prop|bal|sell|accept|metadex`,
  skip all-zero, order address, prop), book_order with sale_amount > 0 (`txid|address|sale_p|sale_a|want_p|want_a|reserved`
  by txid), properties `id|issuer` (eco 1 then 2).
- **names_digest**: sha256 over `{tag}|{address}|{block_height}\n` ORDER BY tag.
- indexcheck `ix`: tip/q/a; binary search for the first differing block.

## 4. Arcade ledger (Omni port)

- Per block ≥ activation height, txs in order: extract, then Engine.process. From cancels_last_from, types 26/27/28
  deferred to end of block.
  - **!!** A payload decode error / unknown / out-of-scope type **halts** indexing.
  - **!!** No per-tx rollback: writes before an InvalidTransaction persist (e.g. inscription chunk row).
  - Invalid txs are recorded with valid=0. Names-only blocks skip non-tag txs.
- Class C: first OP_RETURN push starting `arcd`; multiple marked OP_RETURNs concatenated in output order; max 76 bytes.
- Class B: P2PKH marker to hash160(`DogecoinArcade Class B marker v1`) + multisig outputs; data keys skip keys[0];
  packet = key[1:32] XOR sha256-chain[seq][:31], packet[0] == seq; chain seed = SHA256(sender address ascii),
  next = SHA256(UPPERHEX(prev)).
- Sender: class C = owner of the first input. Class B = largest summed input address (ties → lexicographically smaller).
- Reference: outputs with addresses minus the marker; if > 1, skip the first back to the sender, then take the last.
- Payload header: version u16 BE, type u16 BE. Type 200 AnyData carries `INSC` (inscriptions, tags) or `arcm`
  (messaging).
- Activation (test): 1,496,133 for everything; cancels_last_from 1,513,300; bundles 1,527,064; token pairs 1,529,092.
  Main: 1,208,000, extras off. Property ids start 0x80000001 on test.

### 4c. Tag claim (what V1.0 needs first)

- Wire: AnyData (version u16, type 200) + `INSC` + [1, kind, len] + utf-8 tag. kind 3 = claim, 4 = transfer.
- Valid tag: `^[a-z0-9_]+$`, normalised, length 2-24, not reserved: admin, administrator, arcade, dogecoinarcade,
  support, help, official, system, root, moderator, mod, staff, team, pepecoin, dogecoin, null, none, anonymous, me, you.
- One tag per address, one address per tag. Claim on a held tag is invalid; a sender with another tag loses the old one.
- Binds to the **sender address** (first input owner for class C). No fee or burn beyond the tx fee.
  Built as class C when it fits.

## 5. Faucet

- Local faucet: 100 coins once per account pubkey on signup; daily top-off on testnet.
- **Between arcades: the "welcome" protocol (`wl`)**, off-chain.
  - Records ask/sent/closed, Ed25519-signed by the node key over canonical JSON.
  - AMOUNT 1 test PEP; one ask per arcade; ASK_DAYS 14; 30 asks/hour per link.
  - **!!** Paying is an admin action (`POST /admin/api/welcome/send`), not automatic.

## 6. Identity

- Account key: Ed25519 from the BIP39 seed via a hardened-only HMAC-SHA512 tree (not BIP32) at m/24946'/1'/0'.
  Messaging X25519 at m/24946'/0'/0'.
- Coin address: BIP44 m/44'/<coin>'/0'/0/0 (test coin 1), P2PKH.
- **!!** Nothing on chain links the Ed25519 key to the coin address. The @tag binds on chain to the coin address only.
- key_announcement: `arcm` 01 03 + X25519 pk + tagged tail sections (0x01 hash160+name, 0x03 tag, 0x04 pfp,
  0x05 bio, 0x06 url, ...). **!!** The stated hash160 is not checked against the sender.
