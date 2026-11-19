#!/usr/bin/env python3
"""Borrowed = installed, batch 6 (build locally, broadcast through lenders), on a closed local ARCADE chain. A writes
it, claims @alice and issues an arcade token; C never installs ARCADE (it borrows it from A) and claims @carol. A
sends C some of the token (token.send on its own copy); C, which only borrows the chain, sends part of it back
(token.send chain arctest: its holdings from A's ledger, its coins from what it follows and A's word checked against
the blocks, signed on C, sent through A). A sees C's send confirmed in a block and in its own ledger. ord.send on the
borrowed chain says plainly that ARCADE has no ord inscriptions (the ord path is covered by unit tests: no closed
chain here carries ords).

Batch 7 (swaps on a borrowed chain): A makes a swap order (ARCADE coins for ARCADE coins) and C takes it: C locks on
the chain it only borrows, A checks that lock on its own copy and locks, C reads A's lock from A (checked against its
headers) and claims, A reads the secret and claims. Then C makes and A takes: C reads A's claim of its lock (the
spender A names, read by txid and found in a checked block) and claims A's lock with its secret. Last, a refund: A
takes its order back before C tells of its lock, and C's lock comes back to it after its time. BORROW_SWAPS=0 skips
these.

Run inside: unshare -Urn sh -c "ip link set lo up; python3 borrowbuilds.py ARCADED WORKDIR"
"""
import hashlib, json, os, shutil, socket, subprocess, sys, time

EXE, WORK = sys.argv[1], sys.argv[2]
PORTS = {"A": (18781, 18791), "C": (18782, 18792)}
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
# Swap locks this long (s): a maker's lock 1 time, a taker's 4 times on the ARCADE chain (blocks every ~2 s here).
SWAP_ENV = {"ARCADE_SWAP_TEST_SECS": "120"}
COIN = 100_000_000


def swap_of(n, sid):
    return next((s for s in call(n, "swap.list") if s["id"] == sid), None)


def swap_steps(sid):
    a, c = swap_of("A", sid) or {}, swap_of("C", sid) or {}
    return f"A {a.get('step')} / C {c.get('step')} (A: {a.get('last')}; C: {c.get('last')})"


def swap_round(maker, taker, maker_id, fails, label):
    """maker places ARCADE coins for ARCADE coins, taker takes it whole; both sides must end "claimed"."""
    give = {"chain": "arctest", "kind": "coin", "asset": "", "amount": str(COIN // 2)}
    get = {"chain": "arctest", "kind": "coin", "asset": "", "amount": str(COIN * 4 // 10)}
    try:
        o = call(maker, "swap.place", give=give, get=get)
        s = call(taker, "swap.take", node=maker_id, order=o["id"], give=o["give"], get=o["get"])
    except Exception as e:
        fails.append(f"{label}: {e}")
        return
    sid = s["id"]
    print("   swap", sid[:16], "taken")
    if not wait(f"{label}: both sides claimed", lambda: (swap_of(maker, sid) or {}).get("step") == "claimed" and (swap_of(taker, sid) or {}).get("step") == "claimed",
                900, show=lambda: swap_steps(sid)):
        fails.append(f"{label}: not claimed on both sides: {swap_steps(sid)}")
        return
    m, t = swap_of(maker, sid), swap_of(taker, sid)
    print("   maker claimed", m.get("claim_tx"), "/ taker claimed", t.get("claim_tx"))
    # Both claims in a block, as the arcade keeping ARCADE (A) reads them.
    for who, tx in ((maker, m.get("claim_tx")), (taker, t.get("claim_tx"))):
        if not wait(f"{label}: {who}'s claim in a block", lambda: (call("A", "explorer.tx", chain="arctest", txid=tx, local=True).get("height") or 0) > 0, 300):
            fails.append(f"{label}: {who}'s claim never confirmed")


def swap_refund(fails):
    """C takes A's order; A takes the order back before C tells of its lock, so A never locks: C's lock on the chain it
    only borrows comes back to it after its time (built and signed on C, sent through A)."""
    amt = {"chain": "arctest", "kind": "coin", "asset": "", "amount": str(COIN // 5)}
    ida = call("A", "status")["node_id"]
    o = call("A", "swap.place", give=amt, get=amt)
    s = call("C", "swap.take", node=ida, order=o["id"], give=o["give"], get=o["get"])
    sid = s["id"]
    call("A", "swap.release", order=o["id"])
    if not wait("C's lock is made", lambda: ((swap_of("C", sid) or {}).get("taker_lock") or {}).get("step") == "locked", 300, show=lambda: swap_steps(sid)):
        fails.append(f"refund: C never locked: {swap_steps(sid)}")
        return
    if not wait("C refunds its lock after its time", lambda: (swap_of("C", sid) or {}).get("step") == "refunded", 1500, show=lambda: swap_steps(sid)):
        fails.append(f"refund: C didn't refund: {swap_steps(sid)}")
        return
    tx = swap_of("C", sid).get("refund_tx")
    print("   refund", tx)
    if not wait("C's refund in a block", lambda: (call("A", "explorer.tx", chain="arctest", txid=tx, local=True).get("height") or 0) > 0, 300):
        fails.append("refund: C's refund never confirmed")


def issuance(name, amount):
    # Fixed issuance (type 50): version 0, ecosystem 1, indivisible (type 1), no previous property, then
    # category, subcategory, name, url and data as zero-ended strings, then the amount.
    p = (0).to_bytes(2, "big") + (50).to_bytes(2, "big") + bytes([1]) + (1).to_bytes(2, "big") + (0).to_bytes(4, "big")
    for s in ("", "", name, "", ""):
        p += s.encode() + b"\0"
    return (p + amount.to_bytes(8, "big")).hex()


def holding(n, address, pid):
    r = call(n, "explorer.address", chain="arctest", address=address, history=False, inscriptions=False)
    return next((t.get("units") for t in r.get("ledger_tokens") or [] if t.get("propertyid") == pid), 0)


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
    start("A", settings=sp, env=SWAP_ENV)
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

    start("C", peer="A", settings=sp, env=SWAP_ENV)
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

    print("3. A sends C 100 of the token (token.send on its own copy of ARCADE)")
    a_tok = [k["address"] for k in call("A", "wallet.keys", chain="arctest")][0]
    try:
        r = call("A", "token.send", chain="arctest", propertyid=pid, units=100, to=c_addr, send=True)
        print("   A's send:", json.dumps(r)[:200])
    except Exception as e:
        fails.append(f"A's token.send: {e}")
        return 1
    if not wait("A's ledger has C holding 100", lambda: holding("A", c_addr, pid) == 100, 300):
        fails.append("A's token send to C didn't land")
        return 1
    if not wait("C reads its 100 (from A's ledger)", lambda: holding("C", c_addr, pid) == 100, 120):
        fails.append("C doesn't read its tokens")

    print("4. C, which only borrows ARCADE, sends 40 back: built and signed on C, sent through A")
    try:
        pre = call("C", "token.send", chain="arctest", propertyid=pid, units=40, to=a_tok)
        print("   preview:", json.dumps(pre)[:240])
        if pre.get("sent") is not False or not pre.get("fee"):
            fails.append("C's preview isn't a built, unsent transaction")
        r = call("C", "token.send", chain="arctest", propertyid=pid, units=40, to=a_tok, send=True)
        back = r.get("txid")
        print("   sent", back)
    except Exception as e:
        fails.append(f"C's token.send: {e}")
        back = None
    if back:
        try:
            mp = call("C", "mempool", chain="arctest")
            waiting = any(t["txid"] == back for t in mp["txs"])
            print("   in C's mempool view:", waiting)
        except Exception as e:
            print("   mempool:", e)
        if not wait("A has C's send in a block", lambda: (call("A", "explorer.tx", chain="arctest", txid=back, local=True).get("height") or 0) > 0, 300):
            fails.append("C's token send didn't confirm")
        elif not wait("A's ledger: C 60, A's address 940", lambda: holding("A", c_addr, pid) == 60 and holding("A", a_tok, pid) == 940, 300,
                      show=lambda: f"C {holding('A', c_addr, pid)} A {holding('A', a_tok, pid)}"):
            fails.append("A's ledger doesn't show C's send")
        # More than it holds: refused before anything is built.
        try:
            call("C", "token.send", chain="arctest", propertyid=pid, units=500, to=a_tok, send=True)
            fails.append("C sent more than it holds")
        except RuntimeError as e:
            print("   too much:", str(e)[:120])

    print("5. ord.send on the borrowed chain")
    try:
        call("C", "ord.send", chain="arctest", inscription="0", to=a_arc)
        fails.append("ord.send on ARCADE didn't refuse")
    except RuntimeError as e:
        print("   ", str(e)[:160])
        if "no ord inscriptions" not in str(e):
            fails.append(f"ord.send on borrowed ARCADE: {e}")
    if os.environ.get("BORROW_SWAPS", "1") != "0":
        idc = call("C", "status")["node_id"]
        print("6. Swap: A (ARCADE installed) makes, C (ARCADE only borrowed) takes: ARCADE coins for ARCADE coins")
        swap_round("A", "C", ida, fails, "A makes, C takes")
        print("7. Swap the other way: C makes on the chain it borrows, A takes (C reads A's claim from A's word, checked)")
        swap_round("C", "A", idc, fails, "C makes, A takes")
        print("8. A refund on the borrowed chain: C's lock comes back after its time")
        swap_refund(fails)
    print("RESULT:", "PASS" if not fails else "FAIL", fails)
    return 0 if not fails else 1


try:
    rc = main()
finally:
    for n in list(procs):
        stop(n)
sys.exit(rc)
