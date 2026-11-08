# Old arcade routes vs Arcade V1.0 (2026-11-07)

Source: the Arcade v1.0 session's read-only inventory of ~/.dogecoinarcade/src/arcade at c69f944e0
(737 HTTP routes: 491 json, 128 html, 66 form redirects, 51 files, 1 websocket; 489 reachable from
outside). A live old arcade lists them itself at `GET /api/v1/routes`.

V1.0 today:
- **HTTP** (127.0.0.1 only): `GET /content/<id>`, `GET /inscription/<id>`.
- **Local API** (unix socket): `arcade` and the desktop app use it, for status, wallet, tags, faucet, mempool, mining, ledger, profile, ord, names, maps, tokens, the explorer and chains.
- **Mesh**: HELLO/AUTH, ping/pong, addrs, wl, ix tip (received only), cr offer/q/a/tx, and the V1.0-only bk, pp, sz and sh.

Legend: **have** = V1.0 does it (maybe under another name), **part** = some of it, **missing**.

## Mesh frames

| Frame | What | V1.0 |
|---|---|---|
| auth, ping/pong, addrs | link layer, peer exchange | have |
| ka | keepalive | missing (ping covers it) |
| sub / pub | flooded room subscriptions, room traffic (msg/join/leave/here, ack, echo, ring) | missing |
| dialback | reachability check | missing |
| rl | relay frames through another arcade (rl-open/yes/shut, carries cr/dx/lx/ix) | missing |
| cs offer/open/data/close | raw Core P2P tunnelled over the link | missing |
| cr offer/q/a/tx | shared Core RPC | have (serve home chain, borrow any chain) |
| cr sub / gone | subscribe to nets / ids gone | missing |
| ix tip | ledger fingerprints | part: received, not sent |
| ix q/a (d, rows) | ledger digests and rows on request | missing |
| lx hi/sq/g/gb | token and name index copies (bootstraps) | missing |
| dx hi/ls/ld/dn/ng/nb/sq/g/gb | ord index fingerprints (chunk rows, notes) and snapshots | missing (our chunk hashes match, but we don't speak dx) |
| dx q/a | where, token, number, ticks, holders, kinds, typed, balances, name, on, numbers, note, ticklist, mesh, names_on, mesh_directory, sites_search, moves | missing |
| cb hi/sq/g/gb | Core datadir copies (direct links) | missing |
| kc | known collections | missing |
| iw | imported wallets per account | missing |
| wl | welcome records (test coins) | have |
| fr | operator fruit-name handout | missing |
| op me | who runs this arcade (tag, name, version, hosts, sites) | missing |
| lm | shared LLM service | missing |
| pk | themes | missing |
| st, ud, pu, tu, ui | STUN, UDP links, NAT punch, TURN credentials, introductions | missing |
| (V1.0 only) bk, pp, sz, sh | blocks, coin peers, sizes, shared storage | new |

## HTTP by area

| Area | Old routes | V1.0 |
|---|---|---|
| Access model (door.py): loopback = everything, outside = allowlist, API keys, admin password | middleware | missing (our HTTP is loopback-only) |
| `/content/{key}`, `/content/{key}/fit` | inscription bytes (INSC + ords), sandboxed | part: `/content` serves arcade INSC only, not ords yet; no `/fit` |
| WebSocket `/mesh` | the mesh over this arcade's own name | missing (we dial wss, we don't serve it) |
| Explorer: `/explore`, `/explore/api/{asset,assets,collection,collections,holders,inscription,lookup,ticks,typed}`, `/admin/api/ords/*` | public explorer | part: local `explorer.*` API + desktop tab; no HTTP routes, no arcade-asset (Omni) pages, no collections, no typed listing |
| Page API `/r/*` (≈70: address, balances, blockheight, blocktime, inscription(s), holders, collections, profile, tag, tx, token(s), book, mesh, search, sites, state, storage, send, ask, escrow, claimpools, prizepool, referee, verified, replies, written, tdeck, wallet + JS shims) | what inscribed pages call | missing |
| Accounts `/account/*` (≈190: build-offer → browser signs → `/sign`; coins, send, claim, profile, announce, mailbox, messages, NFTs, doginals send/list/buy, token create/send/manage, token20, orders/book/take, bundles, trade locks/swaps, escrow, pools/prizes, runs/inscribe, mintpad, llm, push, ring, segwit, digidollar, imported wallets, keys) | multi-account web wallet | missing (V1.0 has one local wallet: tag claim, faucet, balances) |
| Sign-in / sign-up `/auth/*`, `/signin/*`, `/signup*`, `/signin-import`, `/join*`, `/restore` | accounts that move between arcades | missing |
| Admin `/admin`, `/admin/api/*` (≈80: cores install/copy, litecoin, mine/mining/stratum, mesh/punch/tunnel, ords, sites, modules, keys, llm, lora, welcome, setup, settings, quotas, owner, password, send, imports, known-collections, utxoset rebuild, swaps, operators, affiliates, fruit, moderation) | operator panel | part: chains install/remove/indexes, mining (`mine.*`), welcome/faucet, mesh peers, sizes; the rest missing |
| Node wallet pages `/wallet*`, `/backup/*`, `/node/*`, `/fund*`, `/send`, `/scan`, `/publish*`, `/setup-identity`, `/settings/*` | operator's own wallet | part: wallet.*, tag.claim, faucet, mine; no backup/import-key/print, no multiple wallets, no split |
| Messaging `/messages*`, `/message/*`, `/compose`, `/contacts*`, `/inbox`, `/node/mail/*` | on-chain encrypted messages | missing (profiles are indexed, no sending/reading) |
| Feed `/feed*`, `/u/{tag}*`, `/account/post`, `/account/react`, `/r/replies`, `/r/written` | social feed | missing |
| Exchange `/exchange*` (≈35: book, pairs, swaps, cross-chain, offers, sell/unlist, trades, history, collections), `/listings`, `/doginals/listings` | markets | missing (the ledger indexes the book; no UI or order flow) |
| Tokens (arcade/Omni) `/tokens*` | create, send, grant, revoke, issuer, launchpad | missing (indexed by the ledger only) |
| Inscribing `/inscriptions/*`, `/create*`, `/account/inscribe*`, `/account/ord/*`, `/account/run/*`, `/mintpad*`, `/launch(es)` | make inscriptions and collections | missing |
| NFTs / collections pages `/nfts`, `/collections*`, `/apps`, `/games*`, `/play/*` | browse | missing |
| Sites `.mesh` web: `/meshweb*`, `/browser*`, `/site/*`, `/r/site*`, `/r/sites`, `/r/search`, `/r/mesh*` | sites on names | missing |
| DogeFeedX `/dogefeedx/*` (≈60 pages + `/dogefeedx/api/*` + network) | alternate UI | missing |
| Lite console `/lite*`, `/api/terminal/*`, `/api/v1/run`, `/api/v1/routes`, `/api/v1/whoami` | console over HTTP, self-description | part: the `arcade` CLI has its own commands; no HTTP console |
| JSON-RPC `/rpc/{which}` (omni_* and da_* methods), `/rpc/work/{net}` | bots, remote miners | missing (`cr` serves Core-style RPC to the mesh only) |
| LLM `/v1/*`, `/account/llm/*`, `/admin/api/llm*` | shared LLM | missing |
| Realtime `/realtime/*` | rooms, calls (ICE/STUN) | missing |
| Bootstraps `/bootstrap/{name}`, `/source*`, `/clone`, `/.well-known/dogecoinarcade.json`, `/instances*` | index copies, own source, directory | missing |
| Shared walks `/coins/walk/*`, `/peer/address/{address}/coins` | help another arcade's address walk | missing (V1.0 indexes every address itself) |
| Static: icons, css, js shims, `/vendor`, `/sw.js`, `/manifest.webmanifest`, `/docs*`, `/terms*`, `/offline`, `/desktop`, `/home` | web app shell | n/a (V1.0's UI is the desktop app) |
| `/events`, `/approvals*`, `/notifications`, `/push/*`, `/moderation/verdicts` | page polling, approvals, push, screening | missing |

## Local RPC / CLI

| Old | V1.0 |
|---|---|
| arcade-cli / Lite console: help, node (getnodeinfo, listchains), wallet (getbalances, getnewaddress, listtransactions, listunspent, send, confirm), messages, feed, market (listtokens, getbook, getpair, listdoginals), apps, modules, mining (mine, stratum), mesh (getmeshinfo, listarcades), whoami, themes | part: `arcade status/peers/wallet/tag/faucet/mempool/mine/ledger/profile/ord/name/map/token(s)/chain …`; no send, listunspent, messages, feed, market, apps, modules, stratum, themes |
| arcade-rpc (omni_* , da_*), arcade-msg, arcade-publish, blockminer, utxoset, service bot | missing |

## Suggested order (closest to what V1.0 already indexes)
1. Ord content over HTTP (`/content` for ords + `/fit`) and `dx` chunk rows and q/a: our ord index already matches.
2. `ix` tip (send) + `ix q/a`, `lx` (token/name copies): ledger and names are already indexed.
3. Explorer HTTP API (`/explore/api/*`) and the page API (`/r/*` read routes) on top of `explorer.*`.
4. Wallet send / listunspent / backup, then messaging (profiles already indexed).
5. Serving WebSocket `/mesh`; relays (rl), dialback, op me.
6. Exchange, inscribing, sites, feed, accounts: the large application layer.
