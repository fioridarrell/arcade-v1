#!/usr/bin/env python3
"""Write docs/API.md from a running arcade's command table (GET /api/v1/routes).
   python3 scripts/apidocs.py [http://127.0.0.1:8420] > docs/API.md"""
import json, sys, urllib.request

base = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:8420"
routes = json.load(urllib.request.urlopen(base + "/api/v1/routes"))["result"]

print("""# Arcade V1.0 API

Everything the desktop app does goes through one set of commands. Programs reach them three ways:

| Way | Who | Address | Auth | What |
|---|---|---|---|---|
| Local socket | the app, the `arcade` CLI | `<home>/arcaded.sock` | file permissions | every command; one JSON request per line: `{"cmd": "msg.me"}` |
| JSON-RPC 2.0 | sidecar apps on this computer | `POST http://127.0.0.1:8420/rpc` | Basic auth with the cookie file `<home>/.cookie` (`__cookie__:<secret>`, new each start) | every command |
| Web API | websites (their server code) and other programs | `http://127.0.0.1:8420/api/v1/<group>/<name>` | none for **public**; otherwise `Authorization: Bearer ak_...` (keys made in the app's API tab) | commands allowed by the key |

**Only this computer.** `/rpc` and `/api/v1` answer only requests from this computer that didn't come
through a proxy (requests carrying `X-Forwarded-For`, `Forwarded` or `X-Real-IP` are refused). The
operator can open the HTTP port (`arcaded --http-bind 0.0.0.0`) and serve a website from it
(`--site DIR`): visitors then get the site's pages and inscription content (`/content/<id>`,
`/inscription/<id>`), never the API. A public website calls the API from its own server code.

## JSON-RPC

```sh
curl -s -u "$(cat <home>/.cookie)" -X POST http://127.0.0.1:8420/rpc \\
  -d '{"jsonrpc":"2.0","id":1,"method":"msg.send","params":{"to":"@apple","text":"hi"}}'
```

Named parameters only (`params` is an object). Batches work. Errors: `-32700` parse, `-32600` bad
request, `-32601` unknown method, `-32602` bad params, `-32000` the command's own error (message says why).
`rpc.help` lists every method.

## Web API

```sh
curl -s "http://127.0.0.1:8420/api/v1/feed?order=new&limit=5"                 # public
curl -s -H "Authorization: Bearer ak_..." http://127.0.0.1:8420/api/v1/msg/unread
curl -s -H "Authorization: Bearer ak_..." -X POST http://127.0.0.1:8420/api/v1/msg/send \\
  -H "Content-Type: application/json" -d '{"to":"@apple","text":"hi"}'
```

`GET` takes parameters from the query string (numbers and `true`/`false` are read as such);
`POST` takes a JSON object (use it for lists and files). Answers are `{"ok": true, "result": ...}` or
`{"ok": false, "error": "..."}` with status 400 (the command refused), 401 (no or bad key),
403 (the key lacks the permission, or not from this computer), 404 (no such command).
CORS allows any origin, so a page opened in a browser on this computer can call public reads.

### Permissions

| needs | meaning |
|---|---|
| public | anyone who can reach the API (this computer) |
| read | private reads: your messages, groups, contacts, wallet addresses, activity |
| message | send and manage private messages, groups, contacts, mutes |
| post | public posts and feed acts (not tips) |
| tip | tips, up to the key's daily limit (in satoshis) |
| wallet | claim @tags, ask the faucet, link wallets, publish your messaging key |
| admin | chains, indexes, mining |
| local | never over the web API (the 12 words, making a wallet, API keys): socket and cookie RPC only |

Files are `{"name": "...", "type": "image/png", "data": "<base64>"}`. Amounts are satoshis.

## Commands
""")
groups = {}
for r in routes:
    g = r["method"].split(".")[0] if "." in r["method"] else "general"
    groups.setdefault(g, []).append(r)
names = {"general": "General", "wallet": "Wallet", "tag": "Tags", "faucet": "Faucet", "msg": "Messages", "contacts": "Contacts",
         "mute": "Mutes", "group": "Groups", "feed": "Feed", "explorer": "Explorer", "chains": "Chains", "mine": "Mining",
         "apikey": "API keys", "rpc": "RPC", "ord": "Ord"}
for g, rs in groups.items():
    print(f"\n### {names.get(g, g.title())}\n")
    print("| method | route | needs | params | what |\n|---|---|---|---|---|")
    for r in rs:
        params = r["params"].replace("|", "\\|") or "—"
        print(f"| `{r['method']}` | `{r['route']}` | {r['needs']} | {params} | {r['summary']} |")
