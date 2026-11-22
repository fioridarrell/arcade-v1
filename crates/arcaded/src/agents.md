# Arcade V1.0 for agents and scripts

One set of commands, three ways in. Each command has the same name, arguments, answer shape and error codes on all of them:

| Way | Address | Auth |
|---|---|---|
| Unix socket | `<home>/arcaded.sock`, one JSON line per request: `{"cmd": "status"}` | file permissions |
| JSON-RPC 2.0 | `POST http://127.0.0.1:8420/rpc`, `{"jsonrpc":"2.0","id":1,"method":"status","params":{}}` | Basic auth with `<home>/.cookie` (new at each start) |
| Web API | `POST` (JSON body) or `GET` (query) `http://127.0.0.1:8420/api/v1/<name, "." as "/">`, e.g. `/api/v1/tx/wait` | none for public commands; else `Authorization: Bearer ak_<id>_<secret>` |

**Describe it to yourself first.** `GET /api/v1/openapi.json` (or `rpc.openapi`) is an OpenAPI 3.1 document. `GET /api/v1/routes` and `rpc.help` give the same list. Each command comes with typed parameters (`schema`: name, type, required, default, enum, maximum), its answer shape (`result`), the permission it needs (`needs`) and whether it signs (`sends`). All three are made from one table in the arcade, so they always agree with the commands.

**Read only what you need.** The whole list is hundreds of KB. `rpc.help {method: "tx.wait"}` answers one command, `rpc.help {prefix: "ledger.item"}` the commands starting so, and `rpc.openapi {prefix: "tx."}` only those paths (the components come whole). To follow an issue or pull request, read `ledger.items {repo, summary: true}` once: each item has `last_height` and `last_by`. Then read one item again only when its `last_height` moved, with `{repo, number, since: <the last height you saw + 1>}`, which keeps only the newer events.

## Keys and permissions

Make an API key on this computer: in the app's API tab, or `apikey.create {name, perms, tip_limit?}` over the socket. The secret is shown once. Each command needs one of these permissions:

- `public`: no key.
- `read`: private reads such as wallet keys, messages and tx.status.
- `message`, `post`, `tip`, `wallet`, `admin`: what the names say.
- `local`: never over a key. Only the socket and the cookie RPC can call these: the 12 words, making or importing wallets, API keys, wallet.send, and anything that names a file on this computer.

A command a key may not call answers `403` with code `forbidden`. It is still listed, under the same name, so you can see it exists.

**Private commands.** A few stay off the network surfaces altogether. They answer on the socket only and are not in the lists: `service.stop` (it stops arcaded). That's the whole list; a test in the arcade fails if any other command is missing from a surface.

## Chains, units, decimals

- `chain` is a chain key: `arctest` (ARCADE testnet), `arc`, `peptest`, `pep`, `doge`, `dgb`, and so on. `chains.list` names them all.
- Leave `chain` out and the home chain is used. The answer's `chain` (and `chain_note`) says which one.
- Amounts are integers in the chain's smallest unit (satoshis). Answers say `decimals` (8) and `unit`. 1 ARCADE = 100000000.
- Explorer and status reads say where they were answered from in `source`:
  - `local`: this arcade's own copy;
  - `borrowed:<node>`: from another arcade, with its node id when known;
  - `copy`: from a shared copy still being imported;
  - `none`: the chain isn't kept here.
- Explorer and status reads also give `as_of {height, hash}`, the tip they were counted against.

## Answers and error codes

The web API answers `{"ok": true, "result": ...}` or `{"ok": false, "error": "...", "code": "..."}`. JSON-RPC puts the code in `error.data.code`. Match on `code`, never on the `error` text. The codes:

- `unknown_command`, `unknown_route` (404)
- `not_found` (404)
- `bad_args`
- `forbidden` (401/403)
- `unknown_chain`, `not_installed`
- `insufficient_funds`, `not_enough_arcade`
- `client_ref_used`
- `starting`, `stopping` (503 with `Retry-After`: retry, the arcade is opening or closing its chains)
- `error`: anything else

## Sends: client_ref, receipts and retries

Every command that signs takes an optional `client_ref`: 1 to 128 printable characters you choose, one per intended send. These include wallet.send, ledger.item_open and other ledger.* writes, a.*, msg.send, feed.post, feed.act, tag.claim, token.send, ord.send and arc.blob_publish (`sends: true` in the lists). Commands with a `send` parameter only sign with `send: true`; without it they show what they would do.

1. **Txid at signing.** The arcade writes a receipt the moment it signs, before any network step. If your HTTP call times out or the connection drops, the send still finishes, and `tx.status {client_ref}` (or `{chain, txid}`) answers at least `signed`.
2. **Retry with the same client_ref.** You never get a second signature:
   - the first send finished: you get its answer again, with `"replayed": true`;
   - it is still running: you get `"in_progress": true` with its txids;
   - it failed after signing: you get code `client_ref_used` and the txid it signed.
   To send again on purpose, use a new client_ref.
3. **Every send answer carries `receipt {chain, txids, client_ref, state}`.**

## Waiting without polling

- `tx.wait {chain, txid | client_ref, until?, timeout?}` answers once the transaction reaches `until` (default `confirmed`), or after `timeout` seconds (default 60, at most 600) with `timed_out: true`. Loop on it until you get what you want. The values of `until`:
  - `accepted_local`
  - `broadcast_ok`
  - `seen_in_peer_mempool`
  - `confirmed`
- tx.status states:
  - `signed`
  - `accepted_local`: `broadcast` gives `peers_linked`, `peers_sent`, `peers_holding`, `peers_acked` and `peers_refused`;
  - `broadcast_failed`: `broadcast.refusals [{peer, code, error}]`, where code is `fee_too_low`, `conflict`, `missing_coin`, `dust`, `not_yet_valid`, `blob_missing`, `pool_full`, `invalid`, `rules`, `refused_recently` or `unreachable`;
  - `confirmed {height, block, confirmations}`
  - `dropped`
  - `unknown`
- `tx.wait` also stops on `dropped` and `broadcast_failed`.
- `chains.wait {chain, height, timeout?}` answers once that chain's tip reaches `height`.
- `events.since {cursor?, kinds?, limit?, wait?}` follows the arcade's event log, oldest first:
  - with no cursor it starts from now; with `"0"`, from the oldest event kept;
  - pass the answer's `next` as the next `cursor`;
  - `wait` (seconds, at most 60) holds the answer until something new arrives.

## Paging

- `ledger.items {repo, cursor?, limit?}` pages by number. Pass `next` back as `cursor`; `next` is `null` on the last page.
- `ledger.items {repo, number, since}` trims each item's `events` to those at or after `since`; every item says its `last_height`.
- Items, comments and state changes still in the pool come with `pending: true`. A pending item has no number yet, and pending items come on the last page.
- `explorer.blocks {chain, order: "asc", from, limit}` is a block feed, oldest first. Ask again from `next_from`. At the tip, `chains.wait {chain, height: next_from}` waits for the next block.
- `explorer.blocks {offset, limit}` pages back from the tip.
- Other lists take `offset`/`limit` (or `cursor` where listed) and say `total` or `next`.

## A first session

```sh
K="Authorization: Bearer ak_..."
curl -s localhost:8420/api/v1/openapi.json > openapi.json
curl -s "localhost:8420/api/v1/status?chain=arctest"
curl -s -H "$K" -X POST localhost:8420/api/v1/ledger/item_open \
  -d '{"repo":"@me/app","kind":"issue","title":"Bug","body":"...","client_ref":"bug-2026-11-19-1"}'
curl -s -H "$K" "localhost:8420/api/v1/tx/wait?client_ref=bug-2026-11-19-1&until=confirmed&timeout=300"
curl -s -H "$K" "localhost:8420/api/v1/ledger/items?repo=@me/app&limit=20"
```
