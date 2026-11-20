#!/usr/bin/env python3
"""ARCADE rules version 6 (docs/FOGSTORE.md "Rules version 6") on a closed local ARCADE chain of three arcades.

A writes the chain and publishes; B and C follow it and get ARCADE from A. Version 6 starts at V6_FROM.

  1  a Temporary blob t (A publishes it before V6_FROM)
  2  before V6_FROM an upgrade is refused (quote and pool)
  3  from V6_FROM B, a different wallet, quotes it: the difference is 352,500 base units per started kB
  4  B upgrades it: every arcade has it in the same block, class Permanent, upgraded by B at that height and paid the
     difference; the Permanent copy floor is 3 everywhere; the blob is still A's
  5  a second upgrade of t is refused (Permanent already)
  6  an underpaid upgrade of another Temporary blob u (built by C with ARCADE_TEST_UPGRADE_SHORT) is refused by the
     pool of every arcade, and u stays Temporary

Run inside: unshare -Urn sh -c "ip link set lo up; python3 upgradenet.py ARCADED WORKDIR"
"""
import hashlib, json, os, shutil, socket, subprocess, sys, time

EXE, WORK = sys.argv[1], sys.argv[2]
NAMES = "ABC"
PORTS = {n: (18840 + i, 18860 + i) for i, n in enumerate(NAMES)}
V6_FROM = 170
DIFF_PER_KB = 394_800 - 42_300
ENV = {"C": {"ARCADE_TEST_UPGRADE_SHORT": "100000000000"}}
procs = {}
B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"


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


def home(n):
    return os.path.join(WORK, n)


def call(n, cmd, timeout=60, **p):
    p["cmd"] = cmd
    s = socket.socket(socket.AF_UNIX)
    s.settimeout(timeout)
    s.connect(os.path.join(home(n), "arcaded.sock"))
    s.sendall((json.dumps(p) + "\n").encode())
    r = json.loads(s.makefile().readline())
    s.close()
    if not r.get("ok"):
        raise RuntimeError(f"{n} {cmd}: {r.get('error')}")
    return r["result"]


def refused(n, cmd, **p):
    """The error text when `cmd` is refused, None when it went through."""
    try:
        call(n, cmd, **p)
        return None
    except RuntimeError as e:
        return str(e)


def start(n, settings=None):
    http, mesh = PORTS[n]
    args = ["nice", "-n", "19", EXE, "--home", home(n), "--http-port", str(http), "--mesh-port", str(mesh), "--stun-port", "0", "--no-seeds"]
    for peer in {"A", NAMES[max(0, NAMES.index(n) - 1)]} - {n}:
        args += ["--mesh-peer", f"127.0.0.1:{PORTS[peer][1]}"]
    if settings:
        args += ["--arc-local", settings]
    log = open(os.path.join(WORK, f"{n}.log"), "a")
    procs[n] = subprocess.Popen(args, stdout=log, stderr=subprocess.STDOUT, env=dict(os.environ, **ENV.get(n, {})))
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


def h(n="A"):
    return call(n, "arc.status")["height"]


def arc_addr(n):
    return b58check(0x53, b58decode(call(n, "start.status")["address"]))


def tx_height(n, txid):
    return call(n, "explorer.tx", chain="arctest", txid=txid, local=True).get("height") or 0


def blob_row(n, blob):
    r = call(n, "explorer.arc_blobs", id=blob)
    items = r.get("items") or r.get("blobs") or []
    return items[0] if items else {}


def main():
    shutil.rmtree(WORK, ignore_errors=True)
    os.makedirs(WORK)
    start("A")
    ida = call("A", "status")["node_id"]
    call("A", "wallet.create")
    a_arc = arc_addr("A")
    stop("A")
    settings = {
        "net": "arctest",
        "genesis": {"time": int(time.time()) - 5, "closed_list": "01" * 32, "final_list": "02" * 32, "beacon_1": "03" * 32, "beacon_2": "04" * 32, "difficulty": "1099511627776"},
        "holders": [{"key": ida, "bytes": 1 << 30, "pay": a_arc}],
        "steps_per_second": 1_000_000,
        "block_seconds": 2,
        "rules": [[3, 0], [4, 1], [5, 2], [6, V6_FROM]],
        "blob_window": 40,
        "blob_temporary": 3600,
    }
    sp = os.path.join(WORK, "settings.json")
    json.dump(settings, open(sp, "w"))
    res = {}
    for n in NAMES:
        start(n, settings=sp)
        call(n, "chains.add", key="arctest")
    for n in "BC":
        call(n, "wallet.create")
    b_arc, c_arc = arc_addr("B"), arc_addr("C")

    print("1. A writes until it can spend (coinbase maturity), pays B and C, publishes two Temporary blobs")
    if not wait("A can spend", lambda: call("A", "wallet.send", chain="arctest", to=a_arc, amount=50_000_000) is not None, 1500, show=lambda: f"height A {h()}"):
        return 1
    if h() >= V6_FROM - 25:
        print(f"  A reached {h()} before publishing; raise V6_FROM")
        return 1
    pay_b = call("A", "wallet.send", chain="arctest", to=b_arc, amount=10 * 100_000_000, send=True)["txid"]
    wait("A's payment to B in a block", lambda: tx_height("A", pay_b) > 0, 120)
    pay_c = call("A", "wallet.send", chain="arctest", to=c_arc, amount=10 * 100_000_000, send=True)["txid"]
    t_text, u_text = "a clip worth keeping " * 120, "another clip " * 200
    pub = call("A", "arc.blob_publish", hex=t_text.encode().hex(), type="text/plain", **{"class": 2}, send=True)
    t_blob, t_tx = pub["blob"], pub["txid"]
    pub2 = call("A", "arc.blob_publish", hex=u_text.encode().hex(), type="text/plain", **{"class": 2}, send=True)
    u_blob, u_tx = pub2["blob"], pub2["txid"]
    wait("the payments and blobs in blocks", lambda: all(tx_height("A", x) > 0 for x in (pay_c, t_tx, u_tx)), 180)
    print(f"   t {t_blob[:16]} ({len(t_text)} bytes), u {u_blob[:16]} ({len(u_text)} bytes), height {h()}")

    print(f"2. before the version 6 height ({V6_FROM}): refused")
    err = refused("B", "arc.blob_upgrade", id=t_blob)
    print(f"   B: {err}")
    res["2: an upgrade is refused before version 6"] = err is not None and "rules version 6" in err

    print(f"3. wait for the version 6 height (now {h()}); B quotes")
    if not wait("rules version 6", lambda: all(h(n) >= V6_FROM + 1 for n in NAMES), 900, show=lambda: f"heights {[h(n) for n in NAMES]}"):
        return 1
    q = call("B", "arc.blob_upgrade", id=t_blob)
    kb = -(-len(t_text) // 1000)
    print(f"   quote: {json.dumps({k: q[k] for k in ('bytes', 'already_paid', 'permanent_fee', 'difference', 'fee', 'floor', 'sent')})}")
    res["3: the quote is the difference (352,500 per started kB), nothing sent"] = (
        q["difference"] == kb * DIFF_PER_KB and q["already_paid"] == kb * 42_300 and q["permanent_fee"] == kb * 394_800
        and q["fee"] > q["difference"] and q["bytes"] == len(t_text) and q["sent"] is False and q["floor"] == 3)

    print("4. B upgrades A's Temporary blob")
    up = call("B", "arc.blob_upgrade", id=t_blob, send=True)
    wait("the upgrade in a block at every arcade", lambda: all(tx_height(n, up["txid"]) > 0 for n in NAMES), 180)
    at = {n: tx_height(n, up["txid"]) for n in NAMES}
    time.sleep(6)
    rows = {n: call(n, "arc.blob", id=t_blob) for n in NAMES}
    for n in NAMES:
        print(f"   {n}: class {rows[n].get('class')} upgraded {rows[n].get('upgraded')}")
    ups = [rows[n].get("upgraded") or {} for n in NAMES]
    res["4: every arcade has the upgrade in the same block"] = len(set(at.values())) == 1 and all(u.get("at_height") == at["A"] for u in ups)
    res["4: Permanent at every arcade, upgraded by B, paid the difference"] = all(
        rows[n].get("class") == 0 and ups[i].get("by") == b_arc and ups[i].get("paid") == kb * DIFF_PER_KB for i, n in enumerate(NAMES))
    fog = {n: blob_row(n, t_blob) for n in NAMES}
    for n in NAMES:
        print(f"   {n} fog map: class {fog[n].get('class')} floor {fog[n].get('floor')} publisher {fog[n].get('publisher')} upgraded {fog[n].get('upgraded')}")
    floors = {n: call(n, "explorer.arc_storage_summary").get("floors", {}).get("permanent") for n in NAMES}
    print(f"   Permanent floor: {floors}")
    res["4: the fog map shows it Permanent, floor 3, still A's"] = all(
        fog[n].get("class") == "permanent" and fog[n].get("floor") == 3 and (fog[n].get("publisher") or {}).get("address") == a_arc
        and (fog[n].get("upgraded") or {}).get("by") == b_arc for n in NAMES)
    res["4: the Permanent copy floor is 3 at every arcade"] = all(f == 3 for f in floors.values())

    print("5. a second upgrade of t")
    err = refused("C", "arc.blob_upgrade", id=t_blob)
    print(f"   C: {err}")
    res["5: refused (Permanent already)"] = err is not None and "Permanent or Release already" in err

    print("6. an underpaid upgrade of u (C pays only the transaction's own fee)")
    short = call("C", "arc.blob_upgrade", id=u_blob)
    print(f"   C's transaction pays {short['fee']} (the difference is {short['difference']})")
    errs = {n: refused(n, "arc.submit", hex=short["hex"]) for n in NAMES}
    for n in NAMES:
        print(f"   {n}: {errs[n]}")
    res["6: every arcade's pool refuses it"] = short["fee"] < short["difference"] and all(e is not None and "upgrade" in e for e in errs.values())
    time.sleep(10)
    res["6: u stays Temporary everywhere"] = all(call(n, "arc.blob", id=u_blob).get("class") == 2 and call(n, "arc.blob", id=u_blob).get("upgraded") is None for n in NAMES)
    res["the arcades agree on the chain"] = wait("same tip", lambda: len({json.dumps(call(n, "arc.status").get("tip")) for n in NAMES}) == 1, 60)

    fails = [k for k, v in res.items() if not v]
    for k, v in res.items():
        print(("ok   " if v else "FAIL ") + k)
    print("RESULT:", "PASS" if not fails else "FAIL", fails)
    return 1 if fails else 0


if __name__ == "__main__":
    rc = 1
    try:
        rc = main()
    finally:
        for n in list(procs):
            try:
                stop(n)
            except Exception:
                pass
    sys.exit(rc)
