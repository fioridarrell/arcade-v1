#!/usr/bin/env python3
"""Node storage management (fog.keep, fog.forget, fog.undo, fog.mine; docs/STORAGEAPP.md) on a closed local ARCADE chain
with a copy floor of 2 (ARCADE_FOG_FLOOR_MAX=2).

  W  writes the chain and publishes; keeps nothing
  A  the operator under test: keeps by choice, forgets, undoes
  B  a keeper with a 50 MB share (takes the short segments by itself)
  C  a keeper whose share is set later: steps in when A hands a segment over; fetches a blob A forgot from B
  D  a viewer linked to A alone (UDP off): what A refuses, D can't get; after the undo it can
  T  borrows the ARCADE chain (never installs it): no keep share, its Keep is refused with the reason; Forget works on
     its viewer cache

A closed net never completes a 10,000-block range, so no segment is ever listed, registered or audited here. The test
hook ARCADE_FOG_TEST_AUDIT_SECS (local chains only, crate::fogstore::test_counted) stands in for the registry: an
announced keeper counts as registered at once and as audited once it has kept the segment for AUDIT seconds holding
every blob of it. Everything else (hand-over, forget list, serve, fetch, bloom, undo) runs as on the real network.

  k  A keeps P#0 by choice (preview card first) and is counted only after its first (stand-in) audit
  f  A forgets P#0: it announces leaving, C steps in; A keeps it ("handing over n/2") until C is counted, then drops it
  r  A forgets one blob of T#0 (which it keeps): it holds the bytes but answers "not here" (D gets nothing), leaves it
     out of its bloom, and C still fetches it from B
  u  undo: D gets the blob from A again; fog.mine lists no forget
  t  T (borrowed): fog.keep refused ("borrows everything"); fog.forget drops its viewer copy
  s  the Storage app's Settings (A, as an app page with its pass): GET /r/fog/settings; POST /r/fog/keep_share files a
     card and changes nothing until approved, then arc.keep says the new share; POST /r/fog/cache refused on the card
     leaves arc.cache as it was, approved sets it

Run inside: unshare -Urn sh -c "ip link set lo up; python3 fogkeepnet.py ARCADED WORKDIR"
"""
import hashlib, hmac, json, os, shutil, signal, socket, subprocess, sys, time, urllib.parse, urllib.request

EXE, WORK = sys.argv[1], sys.argv[2]
NAMES = "WABCDT"
PORTS = {n: (19040 + i, 19060 + i) for i, n in enumerate(NAMES)}
PEERS = {"W": [], "A": ["W"], "B": ["W", "A"], "C": ["W", "B"], "D": ["A"], "T": ["W", "B"]}
FLOOR, AUDIT = 2, 25
GRACE, IDLE, STALE = 30, 60, 60
ENV = {"ARCADE_FOG_GRACE_SECS": str(GRACE), "ARCADE_FOG_IDLE_SECS": str(IDLE), "ARCADE_FOG_STALE_SECS": str(STALE),
       "ARCADE_FOG_FLOOR_MAX": str(FLOOR), "ARCADE_SHARE_EVERY": "10", "ARCADE_FOG_TEST_AUDIT_SECS": str(AUDIT)}
procs = {}
# A's node identity, imported at its first start, so this test can make the pass an app page of A's would carry
# (crate::http::page_pass: HMAC-SHA256 under sha256("arcade page pass\0" + the identity seed), its first 16 bytes).
A_SEED = bytes(range(1, 33))
PAGE = "5" * 64
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


def err(n, cmd, **p):
    try:
        call(n, cmd, **p)
        return None
    except RuntimeError as e:
        return str(e)


def start(n, settings=None, extra=()):
    http, mesh = PORTS[n]
    args = ["nice", "-n", "19", EXE, "--home", home(n), "--http-port", str(http), "--mesh-port", str(mesh), "--stun-port", "0", "--no-seeds", *extra]
    for peer in PEERS[n]:
        args += ["--mesh-peer", f"127.0.0.1:{PORTS[peer][1]}"]
    if settings:
        args += ["--arc-local", settings]
    log = open(os.path.join(WORK, f"{n}.log"), "a")
    env = dict(os.environ, **ENV)
    if n == "D":
        env["ARCADE_MESH_UDP"] = "0"
    procs[n] = subprocess.Popen(args, stdout=log, stderr=subprocess.STDOUT, env=env)
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


def wait(what, cond, secs, show=None, every=3):
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
        time.sleep(every)
    print(f"  timed out: {what}")
    return False


def h(n="W"):
    return call(n, "arc.status")["height"]


def fog(n):
    return call(n, "arc.fog")


def keeps(n, seg):
    fam = "blobs" if seg[0] == "P" else "temporary blobs"
    return any(k["family"] == fam and k["first"] == 0 for k in fog(n).get("kept", []))


def held(n):
    return set(fog(n).get("held_ids", []))


def seg_of(n, seg):
    return next((s for s in call(n, "fog.mine")["kept_segments"] if s["segment"] == seg), None)


def counted(n, seg):
    s = seg_of(n, seg)
    return bool(s and s["counted_here"])


def page_pass(chain="arctest", txid=PAGE):
    key = hashlib.sha256(b"arcade page pass\0" + A_SEED).digest()
    return f"{chain}:{txid}:{hmac.new(key, f'{chain}/{txid}'.encode(), hashlib.sha256).hexdigest()[:32]}"


def page_req(n, method, path, body=None, as_page=True):
    """A request as A's app frame sends it (Origin null, its pass): (status, answer)."""
    headers = {"Content-Type": "application/json"}
    if as_page:
        headers.update({"Origin": "null", "X-Arcade-Page": page_pass()})
    req = urllib.request.Request(f"http://127.0.0.1:{PORTS[n][0]}{path}", method=method, headers=headers,
                                 data=json.dumps(body).encode() if body is not None else None)
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return r.status, json.loads(r.read() or b"null")
    except urllib.error.HTTPError as e:
        try:
            return e.code, json.loads(e.read() or b"null")
        except ValueError:
            return e.code, None


def ask_and_answer(n, path, body, yes):
    """The app asks, the operator answers its card (wallet.approve / wallet.deny), the app sees where it stands."""
    code, r = page_req(n, "POST", path, body)
    if code != 202:
        raise RuntimeError(f"{path} {body}: {code} {r}")
    before = (call(n, "arc.keep")["mb"], call(n, "arc.cache")["mb"])
    pv = call(n, "wallet.preview", id=r["id"])
    call(n, "wallet.approve" if yes else "wallet.deny", id=r["id"])
    code, st = page_req(n, "GET", f"/r/fog/req/{r['id']}")
    return r, pv, before, st


def log_has(n, text):
    return text in open(os.path.join(WORK, f"{n}.log"), errors="replace").read()


def confirmed(txid):
    return (call("W", "explorer.tx", chain="arctest", txid=txid, local=True).get("height") or 0) > 0


def main():
    shutil.rmtree(WORK, ignore_errors=True)
    os.makedirs(WORK)
    res = {}
    start("W")
    idw = call("W", "status")["node_id"]
    call("W", "wallet.create")
    w_home = call("W", "start.status")["address"]
    stop("W")
    w_arc = b58check(0x53, b58decode(w_home))
    settings = {
        "net": "arctest",
        "genesis": {"time": int(time.time()) - 5, "closed_list": "01" * 32, "final_list": "02" * 32, "beacon_1": "03" * 32, "beacon_2": "04" * 32, "difficulty": "1099511627776"},
        "holders": [{"key": idw, "bytes": 1 << 30, "pay": w_arc}],
        "steps_per_second": 1_000_000,
        "block_seconds": 2,
        "rules": [[3, 0], [4, 1], [5, 2]],
        "blob_window": 40,
        "blob_temporary": 3600,
    }
    sp = os.path.join(WORK, "settings.json")
    json.dump(settings, open(sp, "w"))
    seed_file = os.path.join(WORK, "a-node.key")
    open(seed_file, "w").write(A_SEED.hex())
    for n in NAMES:
        start(n, settings=sp, extra=("--import-node-key", seed_file) if n == "A" else ())
        if n != "T":
            call(n, "chains.add", key="arctest")
    ids = {n: call(n, "status")["node_id"] for n in NAMES}
    names = {v: k for k, v in ids.items()}
    call("B", "arc.keep", mb=50)

    print("t. T borrows ARCADE: no keep share; A keeps none yet")
    e = err("T", "fog.keep", segment="P#0")
    print(f"   T fog.keep: {e}")
    res["t: a borrowed arcade's Keep is refused with the reason"] = bool(e) and "borrows" in e
    mine_t = call("T", "fog.mine")
    res["t: T's Mine says none · borrows everything"] = mine_t["borrowed"] and not mine_t["can_keep"] and "borrows everything" in mine_t["keep_share"]["label"]

    print("1. W writes until it can spend")
    if not wait("W can publish", lambda: call("W", "wallet.send", chain="arctest", to=w_arc, amount=50_000_000) is not None, 1500, show=lambda: f"height {h()}"):
        return finish(res)
    hs = h()
    wait("a few more coins", lambda: h() >= hs + 5, 300)

    print("p. W publishes: three Permanent blobs (P#0), two Temporary (T#0)")
    blobs = {}
    for cls, text in [(0, "a deed kept for good "), (0, "a second deed "), (0, "a third deed "), (2, "a short-lived clip "), (2, "another short clip ")]:
        r = call("W", "arc.blob_publish", hex=(text * 40).encode().hex(), type="text/plain", **{"class": cls}, send=True)
        blobs[r["blobs"][0]["blob"]] = (cls, r["txid"])
        time.sleep(2.5)
    wait("their blocks", lambda: all(confirmed(t) for _, t in blobs.values()), 300)
    perm = [b for b, (c, _) in blobs.items() if c == 0]
    temp = [b for b, (c, _) in blobs.items() if c == 2]
    wait("B keeps both segments and holds every blob", lambda: keeps("B", "P#0") and keeps("B", "T#0") and set(blobs) <= held("B"), 300,
         show=lambda: f"B kept {fog('B').get('kept')}, held {len(held('B'))}")

    e = err("A", "fog.keep", segment="P#0")
    print(f"   A (no share yet) fog.keep: {e}")
    res["k: no keep share: Keep refused with the reason"] = bool(e) and "no keep share" in e

    print("k. A keeps P#0 and T#0 by choice")
    call("A", "arc.keep", mb=50)
    card = call("A", "fog.keep", segment="P#0", preview=True)["card"]
    print(f"   card: {card['title']}: {card['lines']}")
    res["k: the card: size, copies before/after, pay, seal time; nothing changed"] = (
        card["size_bytes"] > 0 and card["copies_after"] == card["copies_before"] + 1 and "seal" in card["seal_text"] and "ARCADE" in card["pay_text"]
        and not call("A", "fog.mine")["chosen"])
    t0 = time.time()
    k = call("A", "fog.keep", segment="P#0")
    call("A", "fog.keep", segment="T#0")
    mine = call("A", "fog.mine")
    res["k: fog.mine lists both kept by choice, with Undo ids"] = sorted(c["key"] for c in mine["chosen"]) == ["P#0", "T#0"] and all(c["id"].startswith("k") for c in mine["chosen"])
    early = counted("A", "P#0")
    ok = wait("A counted for P#0 after its first audit", lambda: counted("A", "P#0") and counted("A", "T#0"), 300,
              show=lambda: f"A P#0 {seg_of('A', 'P#0')}")
    t_counted = time.time() - t0
    print(f"   A counted after {t_counted:.0f} s (stand-in audit after {AUDIT} s holding every blob)")
    res["k: A is counted only after its first audit"] = ok and not early and t_counted >= AUDIT - 1
    res["k: P#0 at the floor with A and B"] = wait("P#0 counted at its floor on A", lambda: seg_of("A", "P#0")["copies"] == FLOOR, 60)

    print("c. C gets a share: P#0 and T#0 are at their floor, so C doesn't take them")
    wait("C counts both segments at their floor", lambda: not fog("C").get("short"), 120, show=lambda: fog("C").get("short"))
    call("C", "arc.keep", mb=50)
    time.sleep(35)
    res["c: C doesn't take a segment at its floor"] = not keeps("C", "P#0") and not keeps("C", "T#0")

    print("f. A forgets P#0, which it keeps: hand-over")
    card = call("A", "fog.forget", segment="P#0", preview=True)["card"]
    print(f"   card: {card['lines']}")
    res["f: the forget card shows the hand-over and copies 2/2 → 1/2"] = card["handover"] and card["copies_before"] == 2 and card["copies_after"] == 1 and "stop earning" in card["pay_text"]
    f = call("A", "fog.forget", segment="P#0")
    tf = time.time()
    timeline, bad = [], []
    c_keep = c_counted = a_drop = None
    seen_label = None
    while time.time() - tf < 400:
        a_k, c_k = keeps("A", "P#0"), keeps("C", "P#0")
        c_c = c_k and counted("C", "P#0")
        ho = call("A", "fog.mine")["handing_over"]
        if ho and not seen_label and a_k:
            seen_label = ho[0]["label"] + " · " + ho[0]["note"]
        if c_k and c_keep is None:
            c_keep = time.time() - tf
            timeline.append((round(c_keep), "C keeps P#0"))
        if c_c and c_counted is None:
            c_counted = time.time() - tf
            timeline.append((round(c_counted), "C counted"))
        if not a_k and a_drop is None:
            a_drop = time.time() - tf
            timeline.append((round(a_drop), "A dropped P#0"))
        if c_k and not c_c and not a_k:
            bad.append(round(time.time() - tf))
        if a_drop is not None and c_counted is not None:
            break
        time.sleep(2)
    for t_, e in timeline:
        print(f"   t={t_}s {e}")
    print(f"   A's Mine while waiting: {seen_label}")
    res["f: C steps in once A announces leaving"] = c_keep is not None
    res["f: A shows handing over (n/2) while it waits"] = bool(seen_label) and "handing over" in seen_label
    res["f: A drops only after C is counted (registered + audited)"] = a_drop is not None and c_counted is not None and not bad and a_drop + 12 >= c_counted
    res["f: P#0 back at its floor without A"] = wait("B and C counted", lambda: seg_of("C", "P#0")["copies"] == FLOOR, 60)
    ho = call("A", "fog.mine")["handing_over"]
    res["f: the hand-over is listed done"] = bool(ho) and ho[0]["done"] is not None
    e = err("A", "fog.keep", segment="P#0")
    res["f: a forgotten segment isn't kept again until undone"] = bool(e) and "forgotten" in e

    print("t. T views a blob through its lenders, then forgets it")
    z = perm[1]
    v = call("T", "arc.blob", id=z, timeout=120)
    res["t: T viewed it (viewer cache)"] = v["bytes"] > 0
    f = call("T", "fog.forget", blob=z)
    res["t: T's forget drops its viewer copy"] = f["freed_bytes"] > 0 and bool(err("T", "arc.blob", id=z))
    print("r. A forgets one blob of T#0 (which it keeps): bytes stay, never served")
    y = temp[0]
    res["r: A holds the blob's bytes before"] = y in held("A")
    call("A", "fog.forget", blob=y)
    time.sleep(25)  # a hold announcement or two
    res["r: A keeps the bytes (T#0 is owed to the network)"] = keeps("A", "T#0") and y in held("A")
    res["r: A's own view refuses it"] = bool(err("A", "arc.blob", id=y))
    res["r: A is out of the holders others see (bloom)"] = wait("W sees A out", lambda: ids["A"] not in {x["node"] for x in call("W", "explorer.arc_blobs", id=y)["items"][0]["holders"] if x["holds_bytes"]}, 60)
    # Only A (refusing) and D up: D's one source is A, which answers "not here".
    for n in "WBC":
        stop(n)
    res["r: D holds no copy of its own"] = y not in held("D")
    e = err("D", "arc.blob", id=y)
    print(f"   D asks with only A up: {e}")
    res["r: A answers D 'not here' (it holds the bytes)"] = bool(e) and wait("D's log", lambda: log_has("D", f"not fetched from {ids['A'][:16]}: not here"), 30) and y in held("A")
    # B back (W, the publisher, still down): C fetches it, and B is the only one serving it.
    for n in "BC":
        start(n, settings=sp)
    c_had = y in held("C")
    got = wait("C fetches it", lambda: call("C", "arc.blob", id=y, timeout=90)["bytes"] > 0, 120)
    res["r: C still fetches it from B"] = not c_had and got

    print("u. undo: A serves it again")
    fid = next(x["id"] for x in call("A", "fog.mine")["forgotten"] if x["what"] == "blob")
    call("A", "fog.undo", id=fid)
    res["u: the forget is gone from fog.mine"] = not any(x["id"] == fid for x in call("A", "fog.mine")["forgotten"])
    for n in "BC":
        stop(n)
    res["u: D now gets it from A (the only one up)"] = wait("D views it", lambda: call("D", "arc.blob", id=y, timeout=90)["bytes"] > 0, 90)
    kid = next(x["id"] for x in call("A", "fog.mine")["chosen"] if x["key"] == "T#0")
    call("A", "fog.undo", id=kid)
    res["u: undoing a keep leaves the choice to the round"] = not any(x["key"] == "T#0" for x in call("A", "fog.mine")["chosen"])

    print("s. the Storage app's Settings on A: keep share and blob cache through the card")
    code, st = page_req("A", "GET", "/r/fog/settings")
    print(f"   settings: {code} keep {st and st['keep'].get('choice')} {st and st['keep'].get('mb')} MB, cache {st and st['cache']}")
    res["s: the app reads its settings (arc.keep's choice and presets, arc.cache's size)"] = (
        code == 200 and st["keep"]["mb"] == 50 and st["keep"]["choice"] == "custom" and any(p["id"] == "pct:10" for p in st["keep"]["presets"])
        and st["cache"]["mb"] == call("A", "arc.cache")["mb"] and not st["borrowed"])
    code, _ = page_req("A", "GET", "/r/fog/settings", as_page=False)
    res["s: not without the page's pass"] = code in (400, 403)
    r, pv, before, st = ask_and_answer("A", "/r/fog/keep_share", {"mb": 80}, yes=True)
    c = r["fog"]["card"]
    print(f"   keep share card: {c['title']}: {c['lines']} {c['facts']}")
    res["s: the keep share card: old → new, what it would hold, the disk left"] = (
        c["op"] == "keep_share" and "50.0 MB → 80.0 MB" in c["lines"][0] and ("hold" in c["lines"][1].lower()) and any("free" in f for f in c["facts"])
        and pv["fog"]["card"]["new_mb"] == 80)
    res["s: nothing changed before the card was approved"] = before[0] == 50
    k = call("A", "arc.keep")
    res["s: approved: arc.keep says the app's share"] = st["status"] == "sent" and k["mb"] == 80 and k["choice"] == "custom"
    code, e = page_req("A", "POST", "/r/fog/keep_share", {"mb": 80})
    res["s: the same share again is refused at once"] = code == 400 and "already" in (e or {}).get("error", "")
    cache0 = call("A", "arc.cache")["mb"]
    r, pv, before, st = ask_and_answer("A", "/r/fog/cache", {"mb": 0}, yes=False)
    print(f"   cache card: {r['fog']['card']['lines']} {r['fog']['card']['facts']}")
    res["s: a cache card refused leaves arc.cache as it was"] = st["status"] == "denied" and call("A", "arc.cache")["mb"] == cache0 and "→ off" in r["fog"]["card"]["title"]
    r, pv, before, st = ask_and_answer("A", "/r/fog/cache", {"mb": 1024}, yes=True)
    res["s: an approved cache card sets arc.cache"] = st["status"] == "sent" and call("A", "arc.cache")["mb"] == 1024 and before[1] == cache0
    json.dump({"timeline": timeline, "mine_A": call("A", "fog.mine")}, open(os.path.join(WORK, "fogkeep.json"), "w"), indent=1)
    return finish(res)


def finish(res):
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
