#!/usr/bin/env python3
"""Keep shares by machine, and a lost keeper replaced automatically (fog storage, docs/FOGSTORE.md), on a closed local
ARCADE chain of six arcades with a copy floor that fits them (ARCADE_FOG_FLOOR_MAX=2). Each machine has its own made-up
disk budget (crate::diskbudget, ARCADE_DISK_BUDGET_TEST): free space, what Chains & Coins uses and will grow to.

  A  writes the chain and publishes; keeps no share
  T  tiny: 8 GB free, borrows every chain (ARCADE included, never installed); keeps nothing, is never asked to
  M  medium: 120 GB free, 30 GB of chains growing to 60; chooses a custom 3 MB share (too small for the big segment)
  L  frontier: 4 TB free, 900 GB of chains growing to 2.4 TB; chooses 25% of its free space (past the chains' growth)
  X, Y  in between: 300 GB free; 50 MB shares

A publishes small Permanent, Release and Temporary blobs and a 6 MB large Release blob (parts and a manifest), so
there are two segments: "blobs" (6 MB) and "temporary blobs" (1 kB).

  b  arc.keep on each: its budget, a suggestion that follows the machine (none for T, more for bigger machines,
     always leaving what the chains will grow to and 20 GB), keep Off until the operator chooses
  t  T works as an arcade (wallet) and reads the fog map through the mesh, keeping nothing
  k  each segment kept by exactly the floor, those with the most room first: L takes both, M can't take the big one
  s  arc.fog's short list: copies counted are registered, audited replicas only (0 here), announced keepers apart
  m  explorer.arc_blobs answers on A, also as GET /r/explorer/arc_blobs: the large blob is one item with its parts,
     holders marked, copies counted 0 with the announced keepers apart, totals per publisher; filters and paging
  x  L, the big keeper, is killed for good (SIGKILL, never started again)
  r  once its announcement is older than the stale time, the next with room takes each of its segments: back to the
     floor, the newcomer holding every blob (the large blob's parts too); nobody else moves

Segments are listed only once their 10,000-block range is complete (params::RANGE, consensus), which a closed test net
doesn't reach in minutes: keep decisions here go by the keepers' announcements (fogstore::SegHolders::for_decision),
the path every segment takes before it is listed. The timeline is printed (seconds after the kill).

Run inside: unshare -Urn sh -c "ip link set lo up; python3 keepernet.py ARCADED WORKDIR"
"""
import hashlib, json, os, shutil, signal, socket, subprocess, sys, time, urllib.parse, urllib.request

EXE, WORK = sys.argv[1], sys.argv[2]
NAMES = "ATMLXY"
KEEPERS = "MLXY"
PORTS = {n: (18940 + i, 18960 + i) for i, n in enumerate(NAMES)}
FLOOR = 2
GB = 1 << 30
# Each machine's disk budget (crate::diskbudget, made up on a closed test net): free space, what Chains & Coins uses
# now and will grow to. T is tiny and borrows everything; M is medium; L is a frontier machine; X and Y in between.
BUDGETS = {
    "T": {"free": 8 * GB, "chains_now": 0, "chains_expected": 0},
    "M": {"free": 120 * GB, "chains_now": 30 * GB, "chains_expected": 60 * GB},
    "L": {"free": 4096 * GB, "chains_now": 900 * GB, "chains_expected": 2400 * GB},
    "X": {"free": 300 * GB, "chains_now": 50 * GB, "chains_expected": 80 * GB},
    "Y": {"free": 300 * GB, "chains_now": 50 * GB, "chains_expected": 80 * GB},
}
# Their keep choices: M a custom 3 MB (room for the small segment only), L 25% of its free space, X and Y 50 MB each.
KEEPS = {"M": {"mb": 3}, "L": {"pct": 25}, "X": {"mb": 50}, "Y": {"mb": 50}}
GRACE, IDLE, STALE = 40, 60, 60
ENV = {"ARCADE_FOG_GRACE_SECS": str(GRACE), "ARCADE_FOG_IDLE_SECS": str(IDLE), "ARCADE_FOG_STALE_SECS": str(STALE),
       "ARCADE_FOG_FLOOR_MAX": str(FLOOR), "ARCADE_SHARE_EVERY": "10"}
LARGE_MB = 6
procs = {}
B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
SEGS = ("blobs", "temporary blobs")


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
    # Everyone links to A and to the one before it.
    for peer in {"A", NAMES[max(0, NAMES.index(n) - 1)]} - {n}:
        args += ["--mesh-peer", f"127.0.0.1:{PORTS[peer][1]}"]
    if settings:
        args += ["--arc-local", settings]
    log = open(os.path.join(WORK, f"{n}.log"), "a")
    env = dict(os.environ, **ENV)
    if n in BUDGETS:
        env["ARCADE_DISK_BUDGET_TEST"] = json.dumps(BUDGETS[n])
    procs[n] = subprocess.Popen(args, stdout=log, stderr=subprocess.STDOUT, env=env)
    for _ in range(600):
        try:
            call(n, "status")
            return
        except Exception:
            time.sleep(0.2)
    raise RuntimeError(f"{n} didn't start")


def stop(n, kill=False):
    p = procs.pop(n)
    if kill:
        p.send_signal(signal.SIGKILL)
    else:
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


def h(n="A"):
    return call(n, "arc.status")["height"]


def fog(n):
    return call(n, "arc.fog")


def kept_by(family, among=KEEPERS):
    return [n for n in among if n in procs and any(k["family"] == family and k["first"] == 0 for k in fog(n).get("kept", []))]


def holds_all(n, ids):
    held = set(fog(n).get("held_ids", []))
    return all(i in held for i in ids)


def page(n, args, read="arc_blobs"):
    url = f"http://127.0.0.1:{PORTS[n][0]}/r/explorer/{read}?args=" + urllib.parse.quote(json.dumps(args))
    with urllib.request.urlopen(url, timeout=60) as r:
        v = json.loads(r.read())
    if not v.get("ok"):
        raise RuntimeError(v.get("error"))
    return v["result"]


def confirmed(txid):
    return (call("A", "explorer.tx", chain="arctest", txid=txid, local=True).get("height") or 0) > 0


def main():
    shutil.rmtree(WORK, ignore_errors=True)
    os.makedirs(WORK)
    res = {}
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
        "rules": [[3, 0], [4, 1], [5, 2]],
        "blob_window": 40,
        "blob_temporary": 3600,
    }
    sp = os.path.join(WORK, "settings.json")
    json.dump(settings, open(sp, "w"))
    for n in NAMES:
        start(n, settings=sp)
        if n != "T":  # T knows the chain but never installs it: borrowed, like every other chain it uses
            call(n, "chains.add", key="arctest")
    print("b. each machine's budget, its suggestion, and nothing set by itself")
    sug = {}
    for n in NAMES:
        k = call(n, "arc.keep")
        b = k["budget"]
        sug[n] = k["suggested"]["bytes"]
        print(f"   {n}: free {b['free_bytes'] / GB:.0f} GB, chains {b['chains_bytes'] / GB:.0f} GB growing to {b['chains_expected_bytes'] / GB:.0f}, "
              f"suggested {k['suggested']['bytes'] / GB:.0f} GB, presets {[(p['id'], round(p['bytes'] / GB), p['fits']) for p in k['presets']]}, keep {k['mb']} MB, choice {k['choice']}, suggest prompt {k['suggest']}")
        res[f"b: {n} keeps nothing until its operator chooses"] = k["mb"] == 0 and not k["chosen"] and k["choice"] == "off"
    res["b: the tiny borrowed machine gets no suggestion and no prompt"] = sug["T"] == 0 and not call("T", "arc.keep")["suggest"]
    res["b: suggestions follow the machine (none < medium < large), leaving room for the chains"] = 0 < sug["M"] < sug["X"] < sug["L"] and sug["M"] == 17 * GB and sug["L"] == (4096 - 1500 - 20) // 4 * GB
    for n, a in KEEPS.items():
        call(n, "arc.keep", **a)
    kl = call("L", "arc.keep")
    print(f"   L chose 25% of its free space: {kl['mb'] / 1024:.0f} GB (choice {kl['choice']}); M {call('M', 'arc.keep')['mb']} MB")
    res["b: a share of free space leaves the chains' growth and 20 GB"] = kl["choice"] == "pct:25" and kl["mb"] == (4096 - 1500) * 1024 // 4

    print("t. the tiny borrowed machine is a working arcade")
    call("T", "wallet.create")
    tw = call("T", "status")

    path = os.path.join(WORK, "large.bin")
    with open(path, "wb") as f:
        for i in range(LARGE_MB * 1_000_000 // (32 * 32768) + 1):
            f.write(hashlib.sha256(b"keeper" + i.to_bytes(8, "little")).digest() * 32768)
        f.truncate(LARGE_MB * 1_000_000)

    print("1. A writes until it can spend (coinbase maturity)")
    if not wait("A can publish", lambda: call("A", "wallet.send", chain="arctest", to=a_arc, amount=50_000_000) is not None, 1500, show=lambda: f"height A {h()}"):
        return finish(res)
    hs = h()
    wait("a few more coins", lambda: h() >= hs + 5, 300)

    print("p. A publishes: Permanent, Release, Temporary, and a large Release blob")
    small = {}
    for cls, text in [(0, "a deed kept for good "), (0, "a second deed "), (1, "release notes 1.0 "), (2, "a short-lived clip ")]:
        r = call("A", "arc.blob_publish", hex=(text * 40).encode().hex(), type="text/plain", **{"class": cls}, send=True)
        small[r["blobs"][0]["blob"]] = (cls, r["txid"])
        time.sleep(2.5)
    wait("their blocks", lambda: all(confirmed(t) for _, t in small.values()), 300)
    r = call("A", "arc.blob_publish_large", path=path, type="video/mp4", **{"class": 1}, send=True, timeout=600)
    large = r["blob"]
    st = lambda: call("A", "arc.blob_publish_status", id=large)
    if not wait("the large blob is in", lambda: st()["state"] == "done", 1200, show=lambda: f"height {h()}: {st()['parts_in_blocks']}/{st()['parts']} parts"):
        return finish(res)
    print(f"   large blob {large[:16]}… done at height {h()}")

    print(f"k. each segment kept by {FLOOR} keepers (the floor), the ones with the most room first")
    def seg_ids(n="A"):
        # From A's arc_blobs: every blob id (parts included) by segment family.
        out = {s: set() for s in SEGS}
        for it in page(n, {"limit": 500})["items"]:
            fam = it["segment"]["family"]
            out[fam].add(it["id"])
            for p in it.get("parts", []):
                out[p["segment"]["family"]].add(p["id"])
        return out
    wait("A's fog round lists every blob, the large one as one item", lambda: sum(len(v) for v in seg_ids().values()) >= len(small) + 1 + 2
         and any(it["kind"] == "large" for it in page("A", {})["items"]), 120)
    ids = seg_ids()
    print(f"   blobs segment: {len(ids['blobs'])} blobs (parts included); temporary blobs segment: {len(ids['temporary blobs'])}")
    full = lambda fam: (lambda ks: len(ks) == FLOOR and all(holds_all(n, ids[fam]) for n in ks))(kept_by(fam))
    ok = wait("both segments at the floor and held", lambda: full("blobs") and full("temporary blobs"), 600,
              show=lambda: f"blobs kept by {kept_by('blobs')}, temporary by {kept_by('temporary blobs')}")
    before = {fam: kept_by(fam) for fam in SEGS}
    print(f"   kept: {before}")
    res["k: each segment kept by exactly the floor, every blob held"] = ok
    res["k: the large machine takes most (both segments)"] = all("L" in before[f] for f in SEGS)
    res["k: the medium one with a 3 MB share doesn't take the 6 MB segment"] = "M" not in before["blobs"]
    tb = None
    try:
        tb = call("T", "explorer.arc_blobs", timeout=120)
    except Exception as e:
        print("   T's arc_blobs:", e)
    print(f"   T (borrowing ARCADE) reads arc_blobs: {tb and tb.get('total')} items, from {tb and tb.get('_mesh')}")
    res["t: T reads the fog map through the mesh, keeps nothing, isn't asked to"] = bool(tb) and tb.get("total") == len(small) + 1 and call("T", "arc.keep")["mb"] == 0 and not call("T", "arc.keep")["suggest"]

    names = {call(n, "status")["node_id"]: n for n in NAMES}
    print(f"   A's ARCADE address {a_arc}; nodes {[(v, k[:8]) for k, v in names.items()]}")
    wait("A hears every keeper's keep", lambda: all(
        {x["node"] for x in it["holders"] if x["keeps"]} == {k for k, v in names.items() if v in before[it["segment"]["family"]]}
        for it in page("A", {"limit": 500})["items"]), 120,
        show=lambda: [(it["id"][:8], sorted(names.get(x["node"], "?") for x in it["holders"] if x["keeps"])) for it in page("A", {"limit": 500})["items"]])
    wait("A's fog round counts them", lambda: all(s["announced_not_counted"] == FLOOR for s in fog("A").get("short", [])), 60)

    print("s. arc.fog's short list on A: counted copies are registered, audited replicas only")
    fa = fog("A")
    sh = {s["family"]: s for s in fa.get("short", [])}
    print(f"   short: {[(s['family'], s['copies'], s['floor'], s['announced_not_counted'], s['listed']) for s in fa.get('short', [])]}; warnings {fa.get('warnings')}")
    res["s: copies 0 (nothing listed), the keepers announced apart, no warning"] = (
        all(sh.get(f, {}).get("copies") == 0 and sh.get(f, {}).get("announced_not_counted") == FLOOR and sh.get(f, {}).get("listed") is False for f in SEGS)
        and not fa.get("warnings"))

    print("m. explorer.arc_blobs")
    m = page("A", {"limit": 500})
    json.dump(m, open(os.path.join(WORK, "arc_blobs.json"), "w"), indent=1)
    items = {it["id"]: it for it in m["items"]}
    li = items.get(large, {})
    print(f"   {m['total']} items, totals {m['totals']}")
    print(f"   large: parts {len(li.get('parts', []))}, copies {li.get('copies')}/{li.get('floor')}, announced {li.get('announced_not_counted')}, "
          f"holders {[(names.get(x['node'], '?'), x['state'], x['keeps'], x.get('parts_held')) for x in li.get('holders', [])]}")
    keeper_ids = {call(n, "status")["node_id"] for n in before["blobs"]}
    res["m: the large blob is one item with its parts, none of them listed apart"] = (
        li.get("kind") == "large" and len(li.get("parts", [])) >= 2 and not any(p["id"] in items for p in li.get("parts", [])))
    res["m: holders marked announced, the blobs segment's keepers among them"] = (
        keeper_ids == {x["node"] for x in li.get("holders", []) if x["keeps"]} and all(x["state"] in ("announced", "here") for x in li.get("holders", [])))
    res["m: copies counted 0, announced keepers apart, at the floor"] = li.get("copies") == 0 and li.get("announced_not_counted") == FLOOR and li.get("floor") == FLOOR
    pubs = m["publishers"]
    # The publisher is the address the publishing transactions pay their change to (A's wallet on ARCADE).
    res["m: totals per publisher (one publisher, every item)"] = len(pubs) == 1 and pubs[0]["address"].startswith("a") and pubs[0]["items"] == m["total"] == len(small) + 1
    res["m: holders by role: the keepers, A as the publisher"] = (li.get("by_role", {}).get("keeper") == FLOOR and li.get("by_role", {}).get("publisher", 0) >= 1
                                                                 and any(x["role"] == "publisher" and names.get(x["node"]) == "A" for x in li.get("holders", [])))
    fnodes = page("A", {}, read="arc_fog_nodes")
    json.dump(fnodes, open(os.path.join(WORK, "arc_fog_nodes.json"), "w"), indent=1)
    bynode = {names.get(x["node"], "?"): x for x in fnodes["nodes"]}
    for n in "ALXM":
        x = bynode.get(n, {})
        print(f"   arc_fog_nodes {n}: share {x.get('keep_share')}, roles {x.get('blobs')}, held {x.get('held')}, fog {x.get('in_the_fog')}, "
              f"segments {[s_['label'] for s_ in x.get('segments_kept', [])]}, weight {x.get('earning_weight')}, overlap {[(names.get(o['node'], '?'), o['shared']) for o in x.get('overlap', [])]}")
    lx = bynode.get("L", {})
    res["n: arc_fog_nodes: L's share, its segments n/floor, its overlap, A as publisher"] = (
        lx.get("keep_share", {}).get("mb") == kl["mb"] and sorted(s_["label"] for s_ in lx.get("segments_kept", [])) == sorted(f"{x}#0·{FLOOR}/{FLOOR}" for x in "PT")
        and lx.get("blobs", {}).get("keeper") == len(small) + 1 and len(lx.get("overlap", [])) >= 1 and bynode.get("A", {}).get("blobs", {}).get("publisher") == len(small) + 1
        and bynode.get("M", {}).get("keep_share", {}).get("mb") == 3)
    t = page("A", {"class": "temporary"})
    p1 = page("A", {"limit": 2, "offset": 1, "sort": "newest"})
    so = page("A", {"short_only": True, "publisher": a_arc})
    res["m: filters and paging"] = (t["total"] == 1 and t["items"][0]["class"] == "temporary" and len(p1["items"]) == 2 and p1["total"] == m["total"]
                                    and so["total"] == m["total"])
    try:
        page("A", {"class": "nope"})
        res["m: a bad class is refused"] = False
    except Exception:
        res["m: a bad class is refused"] = True

    print("x. kill a keeper for good")
    # The big one is lost: it keeps both segments.
    victim = "L"
    spare = [n for n in KEEPERS if n not in before["blobs"] or n not in before["temporary blobs"]]
    t0 = time.time()
    stop(victim, kill=True)
    print(f"   t=0: {victim} killed (kept {[f for f in SEGS if victim in before[f]]}); not keeping both before: {spare}")
    timeline = [(0, f"{victim} killed")]
    last = {fam: before[fam] for fam in SEGS}
    lost = [f for f in SEGS if victim in before[f]]
    done_at = None
    deadline = t0 + STALE + 420
    held_at = {}
    while time.time() < deadline:
        now = {fam: kept_by(fam) for fam in SEGS}
        for fam in SEGS:
            if now[fam] != last[fam]:
                e = (round(time.time() - t0), f"{fam} kept by {now[fam]}")
                timeline.append(e)
                print(f"   t={e[0]}s: {e[1]}")
                last[fam] = now[fam]
        for fam in lost:
            for n in now[fam]:
                if n not in before[fam] and (fam, n) not in held_at and holds_all(n, ids[fam]):
                    held_at[(fam, n)] = round(time.time() - t0)
                    timeline.append((held_at[(fam, n)], f"{n} holds every blob of {fam} ({len(ids[fam])})"))
                    print(f"   t={held_at[(fam, n)]}s: {n} holds every blob of {fam}")
        if all(len(now[f]) == FLOOR and victim not in now[f] for f in SEGS) and all(any(k[0] == f for k in held_at) for f in lost):
            done_at = round(time.time() - t0)
            break
        time.sleep(3)
    after = {fam: kept_by(fam) for fam in SEGS}
    print(f"   after: {after}")
    new = {f: [n for n in after[f] if n not in before[f]] for f in SEGS}
    res["r: the floor is met again without the lost keeper"] = done_at is not None
    res["r: exactly one newcomer per segment it lost, nobody else moved"] = all(
        len(new[f]) == (1 if f in lost else 0) and set(after[f]) - set(new[f]) == set(before[f]) - {victim} for f in SEGS)
    # (At t=0 the killed one only drops out of the lists asked of the live arcades.)
    res["r: nobody steps in before the stale time"] = all(e[0] >= STALE - 15 for e in timeline if "victim" not in e[1] and e[0] > 0)
    fa = fog("A")
    res["r: A's fog sees no warning"] = not fa.get("warnings")
    m2 = page("A", {"limit": 500})
    li2 = {it["id"]: it for it in m2["items"]}.get(large, {})
    print(f"   A's arc_blobs, the large blob's keepers now: {sorted(names.get(x['node'], '?') for x in li2.get('holders', []) if x['keeps'])}")
    res["r: arc_blobs drops the lost keeper's keep"] = wait("A's arc_blobs shows the new keepers", lambda: (lambda it: {names.get(x["node"]) for x in it["holders"] if x["keeps"]} == set(after["blobs"]))(
        {it["id"]: it for it in page("A", {"limit": 500})["items"]}[large]), 90)
    # A's own rounds note keepers coming and going (every 10 s here).
    jr_ = lambda: page("A", {"id": large})["items"][0]["journey"]
    wait("A's rounds saw the newcomer join", lambda: any(e["event"] == "keeper joined" and names.get(e["node"]) in new["blobs"] for e in jr_()["events"]), 60)
    jr = jr_()
    json.dump(jr, open(os.path.join(WORK, "journey.json"), "w"), indent=1)
    evs = [(names.get(e["node"], "?"), e["event"].split(" (")[0], e["segment"]) for e in jr["events"]]
    print(f"   the large blob's journey: published at {jr['published']['height']}, grace until +{jr['grace_until'] - jr['published']['time']} s, "
          f"keepers {[(names.get(k['node'], '?'), k['segment'], k['registered']) for k in jr['keepers']]}, events {evs}, replaced {jr['replaced_keepers']}")
    res["j: the journey shows the lost keeper leaving and its replacement joining"] = (
        ("L", "keeper left", "P#0") in evs and any(e[1] == "keeper joined" and e[0] in new["blobs"] for e in evs) and jr["replaced_keepers"] >= 1)
    print("TIMELINE:")
    for t_, e in sorted(timeline):
        print(f"   t={t_:>4}s  {e}")
    print(f"   floor met again at t={done_at}s (stale time {STALE}s, fog rounds every 10 s, holds every 10 s)")
    json.dump({"timeline": sorted(timeline), "before": before, "after": after, "victim": victim, "done_at": done_at}, open(os.path.join(WORK, "timeline.json"), "w"), indent=1)
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
