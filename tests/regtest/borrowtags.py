#!/usr/bin/env python3
"""Borrowed = installed, batch 2 (other people's @tags), on a closed local ARCADE chain. A writes it and claims @alice;
C never installs ARCADE (it borrows it from A, so its tag tables are borrowed too). On C: msg.who @alice, explorer.search
@alice, the feed's tag filter, follow @alice, follows @alice and a wallet.send to @alice all find alice's address, and a
vault.put for @alice from another address is refused (the tag isn't taken for free).

Run inside: unshare -Urn sh -c "ip link set lo up; python3 borrowtags.py ARCADED WORKDIR"
"""
import hashlib, json, os, shutil, socket, subprocess, sys, time

EXE, WORK = sys.argv[1], sys.argv[2]
PORTS = {"A": (18641, 18651), "C": (18642, 18652)}
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
    print("1. A writes until it has ARCADE to spare, then claims @alice on ARCADE")
    if not wait("A has 3 ARCADE", lambda: call("A", "wallet.send", chain="arctest", to=a_arc, amount=300_000_000) is not None, 1500, show=lambda: f"height A {h('A')}"):
        return 1
    call("A", "tag.claim", tag="alice", chain="arctest")
    if not wait("@alice confirmed on A", lambda: tag_status("A", "alice") == "confirmed", 600, show=lambda: f"A height {h('A')} claim {tag_status('A', 'alice')}"):
        fails.append("A's claim of @alice didn't confirm")
        return 1
    alice_addr = call("A", "explorer.tag", chain="arctest", tag="alice")["home_address"]
    print("   on A, @alice =", alice_addr)
    # msg.who answers once alice's messaging key is published.
    try:
        call("A", "msg.announce")
    except Exception as e:
        print("   msg.announce:", e)
    wait("A's msg.who @alice", lambda: call("A", "msg.who", name="@alice") is not None, 300)
    print("2. A posts on the feed")
    post = call("A", "feed.post", text="hello from alice", channel="borrowtags")
    post_tx = post.get("txid")
    start("C", peer="A", settings=sp)
    call("C", "wallet.create")
    print("3. C (ARCADE not installed) borrows it")
    if not wait("C borrows arctest", lambda: borrowed("C")[0] and (borrowed("C")[1].get("tip") or 0) > 0, 300, show=lambda: json.dumps(borrowed("C")[1])[:200]):
        fails.append("C doesn't borrow")
        return 1
    c_addr = [k["address"] for k in call("C", "wallet.keys", chain="arctest")][0]
    call("A", "wallet.send", chain="arctest", to=c_addr, amount=200_000_000, send=True)
    if not wait("C sees 2 ARCADE", lambda: arc_balance("C") >= 200_000_000, 300, show=lambda: f"C balance {arc_balance('C')}"):
        fails.append("C doesn't see its ARCADE")
        return 1
    # Other arcades' index rows (posts, profiles, the tag table) are shown only to an arcade with its own @handle
    # (crate::handlegate): C claims @carol first, as a new operator does.
    call("C", "tag.claim", tag="carol", chain="arctest")
    if not wait("@carol confirmed on C", lambda: tag_status("C", "carol") == "confirmed", 600, show=lambda: f"C claim {tag_status('C', 'carol')}"):
        fails.append("C's claim of @carol didn't confirm")
    same = lambda a: a and b58decode(a) == b58decode(alice_addr)
    print("4. On C: msg.who @alice")
    if not wait("msg.who @alice on C", lambda: same(call("C", "msg.who", name="@alice").get("address")), 120, show=lambda: json.dumps(call("C", "ledger.lent"))[:300]):
        fails.append("msg.who @alice doesn't find alice on C")
    lent_tags = lambda: ((call("C", "ledger.lent").get("arctest") or {}).get("tables", {}).get("tag") or {}).get("rows", 0)
    if not wait("C holds A's tag table (lent rows)", lambda: lent_tags() >= 2, 300, show=lambda: f"lent tag rows {lent_tags()}"):
        fails.append("C's lent tag table stays empty")
    print("   lent tag rows on C:", lent_tags())
    print("5. On C: explorer.search @alice")
    try:
        sr = call("C", "explorer.search", chain="arctest", q="@alice")
        print("   search:", json.dumps(sr)[:200])
        if "alice" not in json.dumps(sr) or "not claimed" in json.dumps(sr):
            fails.append("explorer.search @alice on C")
    except Exception as e:
        fails.append(f"explorer.search @alice: {e}")
    print("6. On C: the feed's tag filter")
    def feed_alice():
        return [p.get("txid") for p in call("C", "feed", tag="alice", order="new", channel="borrowtags").get("posts", [])]
    if not wait("C's feed for @alice lists A's post", lambda: post_tx in feed_alice(), 180, show=lambda: feed_alice()):
        fails.append("feed tag=alice on C doesn't list alice's post")
    print("7. On C: wallet.send to @alice")
    try:
        r = call("C", "wallet.send", chain="arctest", to="@alice", amount=10_000_000, send=True)
        tx = r.get("txid") or (r.get("broadcast") or {}).get("txid")
        print("   sent", tx)
        if not wait("the send to @alice is in a block on A", lambda: (call("A", "explorer.tx", chain="arctest", txid=tx).get("height") or 0) > 0, 300):
            fails.append("the send to @alice didn't confirm")
        else:
            outs = call("A", "explorer.tx", chain="arctest", txid=tx).get("outputs") or []
            if not any(same(o.get("address")) and o.get("value") == 10_000_000 for o in outs):
                fails.append(f"the send to @alice didn't pay alice's address: {outs}")
    except Exception as e:
        fails.append(f"wallet.send @alice: {e}")
    print("8. On C: follow @alice, then follows @alice")
    try:
        call("C", "follow", who="@alice")
        if not wait("@alice has C as a follower", lambda: (call("C", "follows", who="@alice").get("followers") or 0) >= 1, 300,
                    show=lambda: json.dumps(call("C", "follows", who="@alice"))[:200]):
            fails.append("follow @alice from C doesn't show in follows")
    except Exception as e:
        fails.append(f"follow @alice: {e}")
    print("9. On C: vault.put for @alice from C's address is refused (alice holds it)")
    blob = {"kdf": "PBKDF2-SHA512", "iterations": 1000, "salt": "00", "nonce": "00", "sealed": "00"}
    try:
        call("C", "vault.put", tag="alice", address=c_addr, pubkey="02" + "11" * 32, blob=blob, signature="x")
        fails.append("vault.put for someone else's tag was accepted")
    except RuntimeError as e:
        print("   refused:", e)
        if "not the one" not in str(e) and "can't tell" not in str(e):
            fails.append(f"vault.put refused for the wrong reason: {e}")
    print("RESULT:", "PASS" if not fails else "FAIL", fails)
    return 0 if not fails else 1


try:
    rc = main()
finally:
    for n in list(procs):
        stop(n)
sys.exit(rc)
