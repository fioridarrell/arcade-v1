#!/usr/bin/env python3
"""The agent guide, walked end to end (ARCADE #25 part 2), on a closed local ARCADE chain with one arcade.

An agent reads /api/v1/openapi.json and /api/v1/agents.md. It opens an issue with a client_ref over /api/v1 and drops
the HTTP call mid-way, then asks tx.status by client_ref (signed or further), and sends the same call again: one
transaction, no second signature. It waits with tx.wait until the item is confirmed, opens two more, sees a pending
one in ledger.items, pages ledger.items with cursor/next, follows events.since and the block feed (explorer.blocks
order=asc, chains.wait), and checks that every error it can provoke carries a code.

Run inside: unshare -Urn sh -c "ip link set lo up; python3 agentnet.py ARCADED WORKDIR"
"""
import hashlib, http.client, json, os, shutil, socket, subprocess, sys, time

EXE, WORK = sys.argv[1], sys.argv[2]
HTTP, MESH = 18661, 18671
procs = {}
B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
CODES = {"unknown_command", "unknown_route", "not_found", "bad_args", "forbidden", "unknown_chain", "not_installed",
         "insufficient_funds", "not_enough_arcade", "client_ref_used", "starting", "stopping", "error"}


def b58decode(s):
    n = 0
    for ch in s:
        n = n * 58 + B58.index(ch)
    return n.to_bytes(25, "big")[1:21]


def b58check(ver, payload):
    raw = bytes([ver]) + payload
    raw += hashlib.sha256(hashlib.sha256(raw).digest()).digest()[:4]
    n, out = int.from_bytes(raw, "big"), ""
    while n:
        n, r = divmod(n, 58)
        out = B58[r] + out
    return "1" * (len(raw) - len(raw.lstrip(b"\0"))) + out


HOME = os.path.join(WORK, "A")


def call(cmd, **p):
    p["cmd"] = cmd
    s = socket.socket(socket.AF_UNIX)
    s.settimeout(120)
    s.connect(os.path.join(HOME, "arcaded.sock"))
    s.sendall((json.dumps(p) + "\n").encode())
    r = json.loads(s.makefile().readline())
    s.close()
    if not r.get("ok"):
        raise RuntimeError(f"{cmd}: {r.get('error')} [{r.get('code')}]")
    return r["result"]


def web(method, path, body=None, key=None, timeout=120):
    """One /api/v1 call: (status, parsed JSON or text)."""
    c = http.client.HTTPConnection("127.0.0.1", HTTP, timeout=timeout)
    h = {"Host": f"127.0.0.1:{HTTP}", "Content-Type": "application/json"}
    if key:
        h["Authorization"] = f"Bearer {key}"
    c.request(method, path, body=json.dumps(body) if body is not None else None, headers=h)
    r = c.getresponse()
    data = r.read().decode()
    c.close()
    try:
        return r.status, json.loads(data)
    except ValueError:
        return r.status, data


def dropped_post(path, body, key, after=0.05):
    """POST and hang up before the answer: the agent's call killed mid-way."""
    s = socket.create_connection(("127.0.0.1", HTTP))
    raw = json.dumps(body).encode()
    s.sendall((f"POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{HTTP}\r\nAuthorization: Bearer {key}\r\nContent-Type: application/json\r\n"
               f"Content-Length: {len(raw)}\r\n\r\n").encode() + raw)
    time.sleep(after)
    s.close()


def start(settings):
    args = ["nice", "-n", "19", EXE, "--home", HOME, "--http-port", str(HTTP), "--mesh-port", str(MESH), "--stun-port", "0", "--no-seeds"]
    if settings:
        args += ["--arc-local", settings]
    log = open(os.path.join(WORK, "A.log"), "a")
    procs["A"] = subprocess.Popen(args, stdout=log, stderr=subprocess.STDOUT)
    for _ in range(600):
        try:
            call("status")
            return
        except Exception:
            time.sleep(0.2)
    raise RuntimeError("A didn't start")


def stop():
    p = procs.pop("A", None)
    if not p:
        return
    p.terminate()
    try:
        p.wait(60)
    except subprocess.TimeoutExpired:
        p.kill()
        p.wait()


def wait(what, cond, secs, show=None):
    t, last = time.time(), 0
    while time.time() - t < secs:
        try:
            if cond():
                return True
        except Exception as e:
            print("   ", what, ":", str(e)[:160])
        if show and time.time() - last > 30:
            last = time.time()
            print("   ", show())
        time.sleep(2)
    print(f"  timed out: {what}")
    return False


def confirmed(txid):
    return (call("explorer.tx", chain="arctest", txid=txid, local=True).get("height") or 0) > 0


def main():
    shutil.rmtree(WORK, ignore_errors=True)
    os.makedirs(WORK)
    start(None)
    ida = call("status")["node_id"]
    call("wallet.create")
    a_home = call("start.status")["address"]
    stop()
    a_arc = b58check(0x53, b58decode(a_home))
    settings = {
        "net": "arctest",
        "genesis": {"time": int(time.time()) - 5, "closed_list": "01" * 32, "final_list": "02" * 32, "beacon_1": "03" * 32, "beacon_2": "04" * 32, "difficulty": "1099511627776"},
        "holders": [{"key": ida, "bytes": 1 << 30, "pay": a_arc}],
        "steps_per_second": 1_000_000,
        "block_seconds": 2,
    }
    sp = os.path.join(WORK, "settings.json")
    json.dump(settings, open(sp, "w"))
    fails = []
    start(sp)
    call("chains.add", key="arctest")
    h = lambda: call("arc.status")["height"]

    print("1. The agent reads the API's description and the guide (no key)")
    st, doc = web("GET", "/api/v1/openapi.json")
    paths = doc.get("paths", {}) if isinstance(doc, dict) else {}
    print("   openapi", doc.get("openapi") if isinstance(doc, dict) else st, "paths", len(paths))
    io = paths.get("/api/v1/ledger/item_open", {}).get("post", {})
    props = io.get("requestBody", {}).get("content", {}).get("application/json", {}).get("schema", {}).get("properties", {})
    if st != 200 or doc.get("openapi") != "3.1.0" or "client_ref" not in props or props.get("kind", {}).get("enum") != ["issue", "pull"]:
        fails.append(f"openapi.json: {st} {str(io)[:200]}")
    st_r, routes = web("GET", "/api/v1/routes")
    if st_r != 200 or len(routes["result"]) != len(paths):
        fails.append("routes and openapi.json disagree on the commands")
    st, guide = web("GET", "/api/v1/agents.md")
    if st != 200 or "client_ref" not in guide or "service.stop" not in guide:
        fails.append("agents.md")
    for route in ["/api/v1/arc/pool", "/api/v1/arc/block?height=0", "/api/v1/ledger/pairs", "/api/v1/code/status"]:
        st, r = web("GET", route)
        if not isinstance(r, dict) or r.get("code") in ("unknown_route", "unknown_command"):
            fails.append(f"{route} isn't on the web API: {st} {r}")

    print("2. A writes until it has ARCADE to spare, claims @agent, pushes @agent/app")
    if not wait("A has spendable ARCADE", lambda: call("wallet.send", chain="arctest", to=a_arc, amount=300_000_000) is not None, 1500, show=lambda: f"height {h()}"):
        return 1
    call("tag.claim", tag="agent", chain="arctest")
    if not wait("@agent confirmed", lambda: call("explorer.tag", chain="arctest", tag="agent", local=True).get("home_address"), 600):
        return 1
    wait("A's claim record confirmed", lambda: any(x.get("tag") == "agent" and x.get("status") == "confirmed" for x in call("tag.list")), 300)
    pub = call("arc.blob_publish", hex=b"agent repo manifest".hex(), type="text/plain", send=True)
    wait("the blob in a block", lambda: confirmed(pub["txid"]), 300)
    push = call("ledger.repo_push", name="app", manifest=pub["blob"])
    if not wait("the push in a block", lambda: confirmed(push["txid"]), 300):
        return 1
    key = call("apikey.create", name="agent", perms=["read", "wallet"])["key"]

    print("3. ledger.item_open with a client_ref, the HTTP call dropped mid-way")
    ref = "agent-issue-1"
    body = {"repo": "@agent/app", "kind": "issue", "title": "first", "body": "opened by an agent", "client_ref": ref}
    dropped_post("/api/v1/ledger/item_open", body, key)
    seen = {}

    def signed():
        st, r = web("GET", f"/api/v1/tx/status?client_ref={ref}", key=key)
        seen["status"] = r
        return st == 200 and r["result"]["state"] in ("signed", "accepted_local", "confirmed")
    if not wait("tx.status by client_ref says signed (or further)", signed, 120, show=lambda: str(seen.get("status"))[:200]):
        fails.append(f"no receipt after the dropped call: {seen.get('status')}")
    first = seen.get("status", {}).get("result", {})
    print("   after the drop:", first.get("state"), first.get("txid", "")[:16], "ever_signed_here", first.get("ever_signed_here"))
    st, again = web("POST", "/api/v1/ledger/item_open", body, key=key)
    print("   retry:", st, {k: again.get("result", {}).get(k) for k in ("txid", "replayed", "in_progress")} if isinstance(again, dict) else again)
    if st != 200 or again["result"].get("replayed") is not True:
        fails.append(f"the retry wasn't a replay: {st} {again}")
    elif first.get("txid") and again["result"]["receipt"]["txids"][-1] != first["txid"]:
        fails.append(f"the retry names another txid: {again['result']['receipt']} vs {first['txid']}")

    print("4. tx.wait until confirmed")
    st, w = web("GET", f"/api/v1/tx/wait?client_ref={ref}&until=confirmed&timeout=300", key=key, timeout=320)
    print("   ", st, w["result"]["state"] if st == 200 else w, w["result"].get("confirmed") if st == 200 else "")
    if st != 200 or w["result"]["state"] != "confirmed":
        fails.append(f"tx.wait: {st} {w}")

    print("5. Two more items, one seen pending; paging ledger.items with cursor/next")
    refs = []
    for i in (2, 3):
        r = f"agent-issue-{i}"
        refs.append(r)
        st, o = web("POST", "/api/v1/ledger/item_open", {"repo": "@agent/app", "kind": "issue", "title": f"item {i}", "body": "", "client_ref": r}, key=key)
        if st != 200:
            fails.append(f"item {i}: {o}")
    st, li = web("GET", "/api/v1/ledger/items?repo=@agent/app&limit=500", key=key)
    pend = [x for x in li["result"]["items"] if x.get("pending")]
    print("   pending now:", [(x.get("text") or {}).get("title") for x in pend])
    if not pend:
        print("   (both already in a block: pending not seen this run)")
    for r in refs:
        web("GET", f"/api/v1/tx/wait?client_ref={r}&until=confirmed&timeout=300", key=key, timeout=320)
    time.sleep(3)
    got, cursor, pages = [], None, 0
    while True:
        q = "/api/v1/ledger/items?repo=@agent/app&limit=1&summary=true" + (f"&cursor={cursor}" if cursor else "")
        st, page = web("GET", q, key=key)
        if st != 200:
            fails.append(f"paging: {page}")
            break
        pages += 1
        got += [(x["number"], x["text"]["title"]) for x in page["result"]["items"]]
        cursor = page["result"]["next"]
        if not cursor or pages > 10:
            break
    print("   pages", pages, got)
    if [n for n, _ in got] != [1, 2, 3] or [t for _, t in got].count("first") != 1:
        fails.append(f"paged items {got}: want 1..3 and one 'first' (the retry signed nothing)")

    print("6. events.since, the block feed and chains.wait")
    st, ev = web("GET", "/api/v1/events/since?cursor=0&limit=5", key=key)
    if st != 200 or not ev["result"]["events"] or not ev["result"]["next"]:
        fails.append(f"events.since: {ev}")
    st, nxt = web("GET", f"/api/v1/events/since?cursor={ev['result']['next']}&wait=1", key=key)
    if st != 200 or "next" not in nxt["result"]:
        fails.append(f"events.since with wait: {nxt}")
    st, feed = web("GET", "/api/v1/explorer/blocks?chain=arctest&order=asc&from=1&limit=3")
    hs = [b["height"] for b in feed["result"]["items"]] if st == 200 else []
    print("   feed", hs, "next_from", feed["result"].get("next_from") if st == 200 else feed, "source", feed["result"].get("source") if st == 200 else "")
    if hs != [1, 2, 3] or feed["result"]["next_from"] != 4 or feed["result"]["source"] != "local":
        fails.append(f"block feed: {feed}")
    tip = h()
    st, cw = web("GET", f"/api/v1/chains/wait?chain=arctest&height={tip + 1}&timeout=60", timeout=80)
    print("   chains.wait", tip + 1, "->", cw["result"] if st == 200 else cw)
    if st != 200 or not cw["result"]["reached"]:
        fails.append(f"chains.wait: {cw}")

    print("7. Every error carries a code")
    errs = [
        web("GET", "/api/v1/no/such"),
        web("GET", "/api/v1/ledger/items", key=key),
        web("GET", "/api/v1/tx/status?chain=nochain&txid=" + "00" * 32, key=key),
        web("GET", "/api/v1/tx/status?client_ref=never-used", key=key),
        web("GET", "/api/v1/ledger/items?repo=@agent/app"),
        web("POST", "/api/v1/wallet/send", {"chain": "arctest", "to": a_arc, "amount": 1}, key=key),
        web("GET", "/api/v1/ledger/items?repo=@agent/app&cursor=bogus", key=key),
        web("GET", "/api/v1/explorer/blocks?order=sideways"),
    ]
    for st, r in errs:
        code = r.get("code") if isinstance(r, dict) else None
        print("   ", st, code, (r.get("error") if isinstance(r, dict) else r)[:90])
        if st == 200 or code not in CODES:
            fails.append(f"an error without a known code: {st} {r}")
    want = ["unknown_route", "bad_args", "unknown_chain", "not_found", "forbidden", "forbidden", "bad_args", "bad_args"]
    if [r.get("code") for _, r in errs] != want:
        fails.append(f"codes {[r.get('code') for _, r in errs]}, want {want}")
    cookie = open(os.path.join(HOME, ".cookie")).read().strip()
    import base64
    c = http.client.HTTPConnection("127.0.0.1", HTTP, timeout=30)
    c.request("POST", "/rpc", body=json.dumps({"jsonrpc": "2.0", "id": 1, "method": "ledger.items", "params": {}}),
              headers={"Host": f"127.0.0.1:{HTTP}", "Authorization": "Basic " + base64.b64encode(cookie.encode()).decode()})
    rpc = json.loads(c.getresponse().read())
    print("   JSON-RPC:", rpc.get("error"))
    if (rpc.get("error") or {}).get("data", {}).get("code") != "bad_args":
        fails.append(f"JSON-RPC error code: {rpc}")
    print("RESULT:", "PASS" if not fails else "FAIL", fails)
    return 0 if not fails else 1


try:
    rc = main()
finally:
    stop()
sys.exit(rc)
