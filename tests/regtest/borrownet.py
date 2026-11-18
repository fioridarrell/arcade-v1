#!/usr/bin/env python3
"""Borrowed until installed (Martin 2026-11-18), on a closed local ARCADE chain. A writes it. C never installs it, D
installs it but takes no blocks for 240 s. Both see, send and claim ARCADE through A at once; D hands over to its own
copy once that has caught up.

Run inside: unshare -Urn sh -c "ip link set lo up; python3 borrownet.py ARCADED WORKDIR"
"""
import hashlib, json, os, shutil, socket, subprocess, sys, time

EXE, WORK = sys.argv[1], sys.argv[2]
PORTS = {"A": (18621, 18631), "C": (18622, 18632), "D": (18623, 18633)}
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

    print("1. A writes until it has ARCADE to spare (its writer share)")
    if not wait("A has 3 ARCADE", lambda: call("A", "wallet.send", chain="arctest", to=a_arc, amount=300_000_000) is not None, 1500, show=lambda: f"height A {h('A')}"):
        return 1
    print("   A at height", h("A"))

    # C never installs ARCADE; D installs it at once but takes no blocks for 240 s (far behind meanwhile).
    start("C", peer="A", settings=sp)
    start("D", peer="A", settings=sp, env={"ARCADE_ARC_TEST_SYNC_AFTER": "240"})
    call("D", "chains.add", key="arctest")
    for n in "CD":
        call(n, "wallet.create")
    print("   C installed?", [c["installed"] for c in call("C", "borrow.status")["chains"] if c["key"] == "arctest"])

    print("2. C (not installed) and D (installed, far behind) borrow ARCADE")
    for n in "CD":
        if not wait(f"{n} borrows arctest", lambda: borrowed(n)[0], 300, show=lambda: json.dumps(borrowed(n)[1])[:300]):
            fails.append(f"{n} doesn't borrow")
            return 1
    print("   D's own copy at", h("D"), "A at", h("A"))
    if h("D") + 30 > h("A"):
        fails.append("D isn't behind: the test proves nothing for D")

    addr = {}
    for n in "CD":
        wait(f"{n}'s handle goes on ARCADE (borrowed copy has blocks)", lambda: (call(n, "start.status").get("chain") or {}).get("key") == "arctest", 300,
             show=lambda: json.dumps({k: borrowed(n)[1].get(k) for k in ("tip", "state", "lenders")})[:300])
        st = call(n, "start.status")
        print(f"   {n} start.status chain:", st.get("chain"), "address", st.get("address"), "synced", st.get("synced"), "borrowed", st.get("borrowed"))
        if (st.get("chain") or {}).get("key") != "arctest":
            fails.append(f"{n}'s handle isn't claimed on ARCADE")
        addr[n] = st["address"]

    print("3. A pays each 2 ARCADE; they see it through the borrowed copy")
    for n in "CD":
        r = call("A", "wallet.send", chain="arctest", to=addr[n], amount=200_000_000, send=True)
        print("   to", n, r.get("broadcast") or r.get("txid"))
    for n in "CD":
        if not wait(f"{n} sees 2 ARCADE", lambda: arc_balance(n) >= 200_000_000, 300, show=lambda: f"{n} balance {arc_balance(n)}"):
            fails.append(f"{n} doesn't see its ARCADE")

    print("4. Each sends 0.5 ARCADE back to A (signed here, sent by the lender)")
    sent = {}
    for n in "CD":
        try:
            r = call(n, "wallet.send", chain="arctest", to=a_arc, amount=50_000_000, send=True)
            sent[n] = r.get("txid") or (r.get("broadcast") or {}).get("txid")
            print("   ", n, "sent", sent[n], "sent flag", r.get("sent"))
        except Exception as e:
            fails.append(f"{n} can't send: {e}")
    for n, tx in sent.items():
        if not wait(f"A has {n}'s send in a block", lambda: (call("A", "explorer.tx", chain="arctest", txid=tx).get("height") or 0) > 0, 300):
            fails.append(f"{n}'s send didn't confirm")

    print("5. Each claims a handle on ARCADE")
    for n in "CD":
        call(n, "tag.claim", tag="borrow" + n.lower())
    for n in "CD":
        ok = wait(f"@borrow{n.lower()} confirmed", lambda: (call(n, "start.status").get("claim") or {}).get("status") == "confirmed", 600,
                  show=lambda: call(n, "start.status").get("claim"))
        if not ok:
            fails.append(f"{n}'s claim didn't confirm")
            continue
        t = call("A", "explorer.tag", chain="arctest", tag="borrow" + n.lower(), local=True)
        print(f"   A: @borrow{n.lower()} ->", t["home_address"], "claimed at", t["claimed_at"])
        if b58decode(t["home_address"]) != b58decode(addr[n]):
            fails.append(f"@borrow{n.lower()} isn't {n}'s key")
    print("   D's own copy at", h("D"), "A at", h("A"), "(D still borrowing:", borrowed("D")[0], ")")
    if not borrowed("D")[0]:
        fails.append("D stopped borrowing before its own copy caught up")

    print("6. C posts on the feed (written to ARCADE through the lender)")
    try:
        post = call("C", "feed.post", text="hello from a borrowed ARCADE", channel="")
        print("   post:", post)
        if not wait("A has C's post", lambda: (call("A", "explorer.tx", chain="arctest", txid=post["txid"]).get("height") or 0) > 0, 300):
            fails.append("C's post didn't confirm on ARCADE")
    except Exception as e:
        fails.append(f"C can't post: {e}")

    print("7. D's own copy syncs (after 240 s) and takes over; borrowing stops")
    if not wait("D stops borrowing", lambda: not borrowed("D")[0], 600, show=lambda: f"D own {h('D')} A {h('A')} borrowed {borrowed('D')[0]}"):
        fails.append("D never stopped borrowing")
    else:
        print("   D own", h("D"), "A", h("A"))
        if h("D") + 5 < h("A"):
            fails.append("D stopped borrowing while behind")
        time.sleep(5)
        b = arc_balance("D")
        st = call("D", "start.status")
        print("   D balance from its own copy:", b, "tag:", st.get("tag"), "synced:", st.get("synced"), "borrowed:", st.get("borrowed"))
        if b < 100_000_000:
            fails.append("D's own copy lost its balance")
        if st.get("tag") != "borrowd":
            fails.append("D's handle not shown after the handover")
    print("RESULT:", "PASS" if not fails else "FAIL", fails)
    return 0 if not fails else 1


try:
    rc = main()
finally:
    for n in list(procs):
        stop(n)
sys.exit(rc)
