#!/usr/bin/env python3
"""Borrowed = installed, batch 5 (token, ord and explorer reads), on a closed local ARCADE chain. A writes it, claims
@alice and issues an arcade token; C never installs ARCADE (it borrows it from A) and claims @carol (index reads go
only to operators with a handle). On C, explorer.search of the borrowed chain finds A's block by its hash, a
transaction, @alice, an address, the plain word alice (as @alice) and the token by its name (from A's ledger, which C
lacks), and explorer.address lists A's token from A's ledger.

Run inside: unshare -Urn sh -c "ip link set lo up; python3 borrowreads.py ARCADED WORKDIR"
"""
import hashlib, json, os, shutil, socket, subprocess, sys, time

EXE, WORK = sys.argv[1], sys.argv[2]
PORTS = {"A": (18681, 18691), "C": (18682, 18692)}
procs = {}
B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"


def b58decode(s):
    n = 0
    for ch in s:
        n = n * 58 + B58.index(ch)
    raw = n.to_bytes(25, "big")
    return raw[1:21]


def b58check(ver, payload):
    raw = bytes([ver]) + payload
    raw += hashlib.sha256(hashlib.sha256(raw).digest()).digest()[:4]
    n, out = int.from_bytes(raw, "big"), ""
    while n:
        n, r = divmod(n, 58)
        out = B58[r] + out
    return "1" * (len(raw) - len(raw.lstrip(b"\0"))) + out


def home(n):
    return os.path.join(WORK, n)


def call(n, cmd, **p):
    p["cmd"] = cmd
    s = socket.socket(socket.AF_UNIX)
    s.settimeout(60)
    s.connect(os.path.join(home(n), "arcaded.sock"))
    s.sendall((json.dumps(p) + "\n").encode())
    r = json.loads(s.makefile().readline())
    s.close()
    if not r.get("ok"):
        raise RuntimeError(f"{n} {cmd}: {r.get('error')}")
    return r["result"]


def start(n, peer=None, settings=None, env=None):
    http, mesh = PORTS[n]
    args = ["nice", "-n", "19", EXE, "--home", home(n), "--http-port", str(http), "--mesh-port", str(mesh), "--stun-port", "0", "--no-seeds"]
    if peer:
        args += ["--mesh-peer", f"127.0.0.1:{PORTS[peer][1]}"]
    if settings:
        args += ["--arc-local", settings]
    log = open(os.path.join(WORK, f"{n}.log"), "a")
    procs[n] = subprocess.Popen(args, stdout=log, stderr=subprocess.STDOUT, env=dict(os.environ, **(env or {})))
    for _ in range(600):
        try:
            call(n, "status")
            return
        except Exception:
            time.sleep(0.2)
    raise RuntimeError(f"{n} didn't start")


def stop(n):
    p = procs.pop(n)
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
            print("   ", what, ":", e)
        if show and time.time() - last > 30:
            last = time.time()
            print("   ", show())
        time.sleep(2)
    print(f"  timed out: {what}")
    return False



def borrowed(n):
    for c in call(n, "borrow.status")["chains"]:
        if c["key"] == "arctest":
            return c.get("borrowed") is True, c
    return False, None


def arc_balance(n):
    ks = call(n, "wallet.keys", chain="arctest")
    return sum((k.get("balance") or 0) for k in ks)


def tag_status(n, tag):
    for t in call(n, "tag.list"):
        if t["tag"] == tag:
            return t["status"]
    return None


TOKEN = "Borrowtoken"


def issuance(name, amount):
    # Fixed issuance (type 50): version 0, ecosystem 1, indivisible (type 1), no previous property, then
    # category, subcategory, name, url and data as zero-ended strings, then the amount.
    p = (0).to_bytes(2, "big") + (50).to_bytes(2, "big") + bytes([1]) + (1).to_bytes(2, "big") + (0).to_bytes(4, "big")
    for s in ("", "", name, "", ""):
        p += s.encode() + b"\0"
    return (p + amount.to_bytes(8, "big")).hex()


def main():
    shutil.rmtree(WORK, ignore_errors=True)
    os.makedirs(WORK)
    start("A")
    ida = call("A", "status")["node_id"]
    call("A", "wallet.create")
    a_home = call("A", "start.status")["address"]
    stop("A")
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
    start("A", settings=sp)
    call("A", "chains.add", key="arctest")
    h = lambda n: call(n, "arc.status")["height"]
    print("1. A writes until it has ARCADE to spare, claims @alice and issues a token")
    if not wait("A has 3 ARCADE", lambda: call("A", "wallet.send", chain="arctest", to=a_arc, amount=300_000_000) is not None, 1500, show=lambda: f"height A {h('A')}"):
        return 1
    call("A", "tag.claim", tag="alice", chain="arctest")
    if not wait("@alice confirmed on A", lambda: tag_status("A", "alice") == "confirmed", 600, show=lambda: f"A claim {tag_status('A', 'alice')}"):
        return 1
    iss = call("A", "ledger.send", hex=issuance(TOKEN, 1000))
    print("   issuance:", json.dumps(iss)[:200])
    toks = lambda: call("A", "explorer.ledger_tokens", chain="arctest", local=True).get("items", [])
    if not wait("A's token in its ledger", lambda: any(t.get("name") == TOKEN for t in toks()), 300):
        fails.append("A's token never showed in A's ledger")
        return 1
    pid = [t for t in toks() if t.get("name") == TOKEN][0]["id"]
    print("   token id", pid)
    blk = call("A", "explorer.block", chain="arctest", id="5", local=True)
    tx5 = blk["txs"][0]["txid"]

    start("C", peer="A", settings=sp)
    call("C", "wallet.create")
    print("2. C (ARCADE not installed) borrows it, gets ARCADE and claims @carol")
    if not wait("C borrows arctest", lambda: borrowed("C")[0] and (borrowed("C")[1].get("tip") or 0) > 0, 300):
        fails.append("C doesn't borrow")
        return 1
    installed = [c["installed"] for c in call("C", "borrow.status")["chains"] if c["key"] == "arctest"]
    if installed != [False]:
        fails.append("C has ARCADE installed: the test proves nothing")
    c_addr = [k["address"] for k in call("C", "wallet.keys", chain="arctest")][0]
    call("A", "wallet.send", chain="arctest", to=c_addr, amount=200_000_000, send=True)
    if not wait("C sees 2 ARCADE", lambda: arc_balance("C") >= 200_000_000, 300):
        fails.append("C doesn't see its ARCADE")
        return 1
    call("C", "tag.claim", tag="carol", chain="arctest")
    if not wait("@carol confirmed on C", lambda: tag_status("C", "carol") == "confirmed", 600, show=lambda: f"C claim {tag_status('C', 'carol')}"):
        fails.append("C's claim of @carol didn't confirm")
        return 1

    print("3. On C: explorer.search of the borrowed chain")
    def search(q, check, label):
        def ok():
            r = call("C", "explorer.search", chain="arctest", q=q)
            good = check(r)
            if not good:
                print("     ", label, json.dumps(r)[:240])
            return good
        if not wait(f"C's search finds {label}", ok, 120):
            fails.append(f"search on C: {label}")
        else:
            print("  ", label, "ok")
    search(blk["hash"], lambda r: r.get("kind") == "block", "a block by its hash")
    search(tx5, lambda r: r.get("kind") == "tx", "a transaction")
    search("@alice", lambda r: r.get("kind") == "tag", "@alice")
    search(a_arc, lambda r: r.get("kind") == "address", "A's address")
    hits = lambda r: r.get("items") if r.get("kind") == "results" else [r]
    search("alice", lambda r: any(i.get("kind") == "tag" for i in hits(r)), "the word alice (as @alice)")
    search(TOKEN.lower(), lambda r: any(i.get("kind") == "ltoken" and str(i.get("id")) == str(pid) for i in hits(r)), "the token by its name (A's ledger)")

    print("4. On C: explorer.address lists A's token (from A's ledger)")
    def holds():
        r = call("C", "explorer.address", chain="arctest", address=a_arc, history=False, inscriptions=False)
        return any(t.get("propertyid") == pid and t.get("units") == 1000 for t in r.get("ledger_tokens") or [])
    if not wait("C reads A's token holding", holds, 120):
        fails.append("explorer.address on C doesn't list A's token")
    print("RESULT:", "PASS" if not fails else "FAIL", fails)
    return 0 if not fails else 1


try:
    rc = main()
finally:
    for n in list(procs):
        stop(n)
sys.exit(rc)
