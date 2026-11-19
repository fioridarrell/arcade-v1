#!/usr/bin/env python3
"""Fog-of-war storage (rules version 5, docs/FOGSTORE.md) on a closed local ARCADE chain of nine arcades.

A writes the chain and publishes. B to I keep a share of the chain by choice (arc.keep). Before the switch-on height
every arcade takes every blob's bytes; from it, after the grace window, only the publisher, the segments' keepers and
the arcades it was viewed through hold them:

  f  a Temporary blob published before the height: every arcade still holds it after the grace window
  a  a Temporary blob published after: once the grace window ends, A (its publisher) and its segment's keepers (3,
     its floor) hold it, the others don't
  b  an arcade that doesn't hold it views it: it holds it and serves it; another fetches it from that one with A and
     the segment's keepers stopped
  c  idle fade: the viewer's copy goes once nobody views it for the idle time; the keepers' copies stay
  e  the publisher keeps its own past the idle time
  d  a Permanent blob: its segment is kept by 7 arcades (its floor), all of them keepers by choice, none other
  g  one of those 7 stops past the stale time: an eighth with room takes the segment, back to 7; when the stopped one
     comes back, one lets go again (7). A keeper stopped for less than the stale time moves nothing.

Copies of a segment are counted from the registry's audited replicas once it is listed; a closed test net doesn't
complete a 10,000-block range, so here they come from the keepers' announcements (crate::fogstore::Counted::Announced).

Run inside: unshare -Urn sh -c "ip link set lo up; python3 fognet.py ARCADED WORKDIR"
"""
import hashlib, json, os, shutil, socket, subprocess, sys, time

EXE, WORK = sys.argv[1], sys.argv[2]
NAMES = "ABCDEFGHI"
PORTS = {n: (18740 + i, 18760 + i) for i, n in enumerate(NAMES)}
KEEPERS = NAMES[1:]
FOG_FROM = 180
GRACE, IDLE, STALE = 60, 90, 120
ENV = {"ARCADE_FOG_GRACE_SECS": str(GRACE), "ARCADE_FOG_IDLE_SECS": str(IDLE), "ARCADE_FOG_STALE_SECS": str(STALE), "ARCADE_SHARE_EVERY": "20"}
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


def start(n, settings=None):
    http, mesh = PORTS[n]
    args = ["nice", "-n", "19", EXE, "--home", home(n), "--http-port", str(http), "--mesh-port", str(mesh), "--stun-port", "0", "--no-seeds"]
    # Everyone links to A and to the one before it, so holds and blobs travel more than one way.
    for peer in {"A", NAMES[max(0, NAMES.index(n) - 1)]} - {n}:
        args += ["--mesh-peer", f"127.0.0.1:{PORTS[peer][1]}"]
    if settings:
        args += ["--arc-local", settings]
    log = open(os.path.join(WORK, f"{n}.log"), "a")
    procs[n] = subprocess.Popen(args, stdout=log, stderr=subprocess.STDOUT, env=dict(os.environ, **ENV))
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
        time.sleep(3)
    print(f"  timed out: {what}")
    return False


def h(n="A"):
    return call(n, "arc.status")["height"]


def fog(n):
    return call(n, "arc.fog")


def holds(n, blob):
    return blob in fog(n).get("held_ids", [])


def who_holds(blob, among=NAMES):
    return [n for n in among if n in procs and holds(n, blob)]


def kept_by(first, family, among=KEEPERS):
    out = []
    for n in among:
        if n in procs and any(k["family"] == family and k["first"] == first for k in fog(n).get("kept", [])):
            out.append(n)
    return out


def publish(cls, text):
    r = call("A", "arc.blob_publish", hex=text.encode().hex(), type="text/plain", **{"class": cls}, send=True)
    return r["blobs"][0]["blob"], r["txid"]


def confirmed(txid):
    return (call("A", "explorer.tx", chain="arctest", txid=txid, local=True).get("height") or 0) > 0


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
        "rules": [[3, 0], [4, 1], [5, FOG_FROM]],
        "blob_window": 40,
        "blob_temporary": 3600,
    }
    sp = os.path.join(WORK, "settings.json")
    json.dump(settings, open(sp, "w"))
    res = {}
    for n in NAMES:
        start(n, settings=sp)
        call(n, "chains.add", key="arctest")
    for n in KEEPERS:
        call(n, "arc.keep", mb=100)

    print("1. A writes until it can spend (coinbase maturity)")
    if not wait("A can publish", lambda: call("A", "wallet.send", chain="arctest", to=a_arc, amount=50_000_000) is not None, 1500, show=lambda: f"height A {h()}"):
        return 1
    if h() >= FOG_FROM - 20:
        print(f"  A reached {h()} before publishing; raise FOG_FROM")
        return 1

    print("f. a Temporary blob before the switch-on height")
    old_blob, old_tx = publish(2, "before the fog " * 20)
    wait("its block", lambda: confirmed(old_tx), 300)
    print(f"2. wait for the switch-on height {FOG_FROM} (now {h()})")
    if not wait("rules version 5", lambda: h() >= FOG_FROM + 2, 900, show=lambda: f"height {h()}"):
        return 1

    print("a. a Temporary and a Permanent blob after it")
    t_blob, t_tx = publish(2, "a short-lived clip " * 30)
    p_blob, p_tx = publish(0, "a deed kept for good " * 30)
    wait("their block", lambda: confirmed(t_tx) and confirmed(p_tx), 300)
    p_first = (call("A", "explorer.tx", chain="arctest", txid=p_tx, local=True)["height"] // 10_000) * 10_000
    wait("every arcade holds them at first (grace)", lambda: len(who_holds(t_blob)) == len(NAMES), 60)
    time.sleep(GRACE + 30)
    wait("the fog settles", lambda: len(who_holds(t_blob)) <= 4, 240, show=lambda: f"Temporary held by {who_holds(t_blob)}")
    t_holders = who_holds(t_blob)
    t_keepers = kept_by(p_first, "temporary blobs")
    print(f"   Temporary held by {t_holders}; its segment kept by {t_keepers}")
    res["a: publisher + 3 keepers hold it, the rest don't"] = "A" in t_holders and len(t_keepers) == 3 and set(t_holders) == {"A", *t_keepers}
    old_holders = who_holds(old_blob)
    res["f: before the height every arcade holds it"] = len(old_holders) == len(NAMES)
    print(f"   the blob from before the height held by {old_holders}")

    print("d. the Permanent blob's segment: 7 keepers by choice")
    wait("7 keepers", lambda: len(kept_by(p_first, "blobs")) == 7, 300)
    p_keepers = kept_by(p_first, "blobs")
    p_holders = who_holds(p_blob)
    print(f"   Permanent kept by {p_keepers}; held by {p_holders}")
    res["d: 7 keepers, all by choice, each holding it"] = len(p_keepers) == 7 and set(p_keepers) <= set(KEEPERS) and set(p_keepers) <= set(p_holders)

    print("b. an arcade that doesn't hold the Temporary blob views it; another fetches it from that one")
    others = [n for n in KEEPERS if n not in t_holders]
    # Two linked to each other (each arcade links to the one before it): a closed net has no relay to open a link to an
    # arcade it only knows from its announcement.
    pairs = [(v, f) for v in others for f in others if NAMES.index(f) == NAMES.index(v) + 1]
    viewer, fetcher = pairs[0] if pairs else (others[0], others[1])
    print(f"   viewer {viewer}, fetcher {fetcher}")
    got = call(viewer, "arc.blob", id=t_blob, timeout=120)
    res["b: the viewer gets it and then holds it"] = bool(got) and wait("viewer holds", lambda: holds(viewer, t_blob), 60)
    off = ["A", *t_keepers]
    for n in off:
        stop(n)
    try:
        got2 = call(fetcher, "arc.blob", id=t_blob, timeout=120)
        res["b: fetched from the viewer with A and the keepers stopped"] = bool(got2) and holds(fetcher, t_blob)
    except Exception as e:
        print("   fetch:", e)
        res["b: fetched from the viewer with A and the keepers stopped"] = False
    for n in off:
        start(n, settings=sp)

    print("c+e. idle fade: the viewers' copies go, the keepers' and the publisher's stay")
    wait("viewers' copies gone", lambda: not holds(viewer, t_blob) and not holds(fetcher, t_blob), IDLE + 240, show=lambda: f"held by {who_holds(t_blob)}")
    left = who_holds(t_blob)
    print(f"   after the idle time: held by {left}")
    res["c: viewer copies dropped after the idle time"] = viewer not in left and fetcher not in left
    res["c: the keepers' copies stay (at the floor)"] = set(t_keepers) <= set(left)
    res["e: the publisher keeps its own"] = "A" in left

    print("g. a Permanent keeper stopped for less than the stale time: nothing moves")
    p_keepers = kept_by(p_first, "blobs")
    gone = p_keepers[0]
    stop(gone)
    time.sleep(STALE // 3)
    during = kept_by(p_first, "blobs")
    start(gone, settings=sp)
    res["g: a short stop moves nothing"] = len(during) == 6 and gone not in during and not (set(during) - set(p_keepers))
    print(f"   while {gone} was stopped: {during}")

    print("g. stopped past the stale time: another takes it over, back to 7")
    time.sleep(30)
    stop(gone)
    ok = wait("back to 7 without it", lambda: len(kept_by(p_first, "blobs")) == 7, STALE + 300, show=lambda: f"kept by {kept_by(p_first, 'blobs')}")
    now_keepers = kept_by(p_first, "blobs")
    new = [n for n in now_keepers if n not in p_keepers]
    res["g: a new keeper steps in (7 again)"] = ok and len(new) == 1 and all(n in who_holds(p_blob) for n in now_keepers)
    print(f"   kept by {now_keepers}, new: {new}")
    start(gone, settings=sp)
    ok = wait("one lets go when it comes back", lambda: len(kept_by(p_first, "blobs")) == 7 and gone in kept_by(p_first, "blobs") or len(kept_by(p_first, "blobs")) == 7, 300, show=lambda: f"kept by {kept_by(p_first, 'blobs')}")
    time.sleep(60)
    final = kept_by(p_first, "blobs")
    res["g: back to 7 after it returns (not 8)"] = len(final) == 7
    print(f"   after it came back: {final}")

    fails = [k for k, v in res.items() if not v]
    for k, v in res.items():
        print(("ok   " if v else "FAIL ") + k)
    print("RESULT:", "PASS" if not fails else "FAIL", fails)
    return 1 if fails else 0


if __name__ == "__main__":
    try:
        rc = main()
    finally:
        for n in list(procs):
            try:
                stop(n)
            except Exception:
                pass
    sys.exit(rc)
