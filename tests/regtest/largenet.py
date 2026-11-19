#!/usr/bin/env python3
"""Large blobs (crate::largeblob): one blob of up to 100 MB on a closed local ARCADE chain of two arcades.

A writes the chain and publishes; B only follows. B keeps new blobs only for a short grace window and a short idle
time, so once the large blob is in, B drops the parts and has to fetch them back over the mesh to read it.

  q  the quote: parts, fee, blocks
  p  A publishes it with arc.blob_publish_large (parts over successive blocks, then the manifest)
  r  A (and B, so nobody keeps the waiting transactions) is stopped midway and started again: the job resumes, and nothing is paid twice (every part and the manifest
     are in exactly one mined transaction, the job's own)
  f  fog: A's storage summary and arc.fog list it as one item with its part count; B keeps its manifest
  m  B, holding none of the parts, serves a byte range from the middle over HTTP (fetched over the mesh), the bytes
     right, then the whole file, its sha256 right
  s  a range across a part boundary (a video seeking)

Run inside: unshare -Urn sh -c "ip link set lo up; python3 largenet.py ARCADED WORKDIR [MB]"
"""
import hashlib, json, os, shutil, socket, subprocess, sys, time, urllib.request

EXE, WORK = sys.argv[1], sys.argv[2]
MB = int(sys.argv[3]) if len(sys.argv) > 3 else 100
SIZE = MB * 1_000_000
PART = 3_999_992
NAMES = "AB"
PORTS = {n: (18840 + i, 18860 + i) for i, n in enumerate(NAMES)}
GRACE, IDLE = 20, 40
ENV = {"ARCADE_FOG_GRACE_SECS": str(GRACE), "ARCADE_FOG_IDLE_SECS": str(IDLE), "ARCADE_SHARE_EVERY": "20"}
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


def call(n, cmd, timeout=120, **p):
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
    for peer in set(NAMES) - {n}:
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


def wait(what, cond, secs, show=None, every=2):
    t, last = time.time(), 0
    while time.time() - t < secs:
        try:
            if cond():
                return True
        except Exception as e:
            print("   ", what, ":", e)
        if show and time.time() - last > 30:
            last = time.time()
            try:
                print("   ", show(), flush=True)
            except Exception as e:
                print("   ", e)
        time.sleep(every)
    print(f"  timed out: {what}")
    return False


def h(n="A"):
    return call(n, "arc.status")["height"]


def get(n, bid, rng=None, timeout=600):
    req = urllib.request.Request(f"http://127.0.0.1:{PORTS[n][0]}/content/{bid}")
    if rng:
        req.add_header("Range", f"bytes={rng[0]}-{rng[1]}")
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.status, dict(r.headers), r.read()


def records(n, h0, h1):
    """Blob records ("ablb" + root + length) in the mined transactions of blocks h0..h1: [(txid, root hex, length)]."""
    out = []
    for height in range(h0, h1 + 1):
        try:
            b = call(n, "arc.block", height=height, raw=True)
        except RuntimeError:
            break
        for txid, raw in zip(b["txs"][1:], b["raw"][1:]):
            raw = bytes.fromhex(raw)
            at = raw.find(b"ablb")
            while at >= 0:
                out.append((txid, raw[at + 4:at + 36].hex(), int.from_bytes(raw[at + 36:at + 44], "little")))
                at = raw.find(b"ablb", at + 44)
    return out


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
        call(n, "chains.add", key="arctest")

    print(f"0. a {MB} MB file")
    path = os.path.join(WORK, "film.bin")
    with open(path, "wb") as f:
        seed = hashlib.sha256(b"large blob").digest()
        left, i = SIZE, 0
        while left > 0:
            # Varied bytes that don't compress, made fast: a hash-seeded block repeated with its index.
            chunk = hashlib.sha256(seed + i.to_bytes(8, "little")).digest() * 32768
            chunk = chunk[:min(left, len(chunk))]
            f.write(chunk)
            left -= len(chunk)
            i += 1
    data = open(path, "rb").read()
    sha = hashlib.sha256(data).hexdigest()

    print("1. A writes until it can spend (coinbase maturity) and has enough")
    if not wait("A can spend", lambda: call("A", "wallet.send", chain="arctest", to=a_arc, amount=50_000_000) is not None, 1500, show=lambda: f"height A {h()}"):
        return 1

    print("q. the quote")
    q = call("A", "arc.blob_publish_large", path=path, type="video/mp4", **{"class": 2}, timeout=600)
    quote = q["quote"]
    print(f"   id {q['blob'][:16]}…, {q['parts']} parts, fee {quote['fee'] / 1e8:.4f} ARCADE (blobs {quote['blob_fee'] / 1e8:.4f}), "
          f"{quote['blocks']} blocks, {quote['seconds']} s at {quote['block_seconds']} s blocks")
    want_parts = -(-SIZE // PART)
    res["q: the parts, fee and blocks are quoted"] = q["parts"] == want_parts and quote["blocks"] == want_parts + 1 and quote["fee"] > quote["blob_fee"] > 0 and not q["sent"]
    bid = q["blob"]
    # Coins for the fee: each block matures one more coinbase (50 ARCADE), faster than a part a block spends.
    h_spend = h()
    wait("a few more coins", lambda: h() >= h_spend + 5, 300)

    print("p. publish")
    h0 = h()
    t0 = time.time()
    r = call("A", "arc.blob_publish_large", path=path, type="video/mp4", **{"class": 2}, send=True, timeout=600)
    res["p: the job starts"] = r["sent"] and r["blob"] == bid
    st = lambda: call("A", "arc.blob_publish_status", id=bid)
    show = lambda: (lambda s: f"height {h()}: {s['parts_in_blocks']}/{s['parts']} in blocks, {s['parts_waiting']} waiting, manifest {s['manifest']}, resent {s['resent']} {s['note']}")(st())

    print("r. stop A midway and start it again")
    stop_at = max(1, want_parts // 3)
    wait(f"{stop_at} parts in blocks", lambda: st()["parts_in_blocks"] >= stop_at, 1800, show=show, every=0.3)
    before = st()
    print(f"   stopping A with {before['parts_in_blocks']} parts in blocks, {before['parts_waiting']} waiting")
    # B too, so no arcade keeps A's waiting transactions: A has to send them again from its journal.
    stop("B")
    stop("A")
    time.sleep(5)
    start("A", settings=sp)
    start("B", settings=sp)
    if not wait("done", lambda: st()["state"] == "done", 3600, show=show):
        return finish(res)
    t1 = time.time()
    done = st()
    h1 = h()
    print(f"   done in {t1 - t0:.0f} s, blocks {h0}..{h1}; resent {done['resent']}, rebuilt {done['rebuilt']}, paid {done['paid'] / 1e8:.4f} ARCADE")
    res["r: it resumed after the restart"] = done["state"] == "done" and done["parts_in_blocks"] == want_parts
    res["r: the waiting parts were sent again from the journal"] = before["parts_waiting"] == 0 or done["resent"] >= 1
    recs = records("A", h0, h1 + 2)
    roots = [x[1] for x in recs]
    job_txids = set(done["txids"])
    print(f"   {len(recs)} blob records mined in {len(set(x[0] for x in recs))} transactions; {len(set(roots))} distinct")
    res["r: nothing paid twice (one record per part and the manifest, in the job's own transactions)"] = (
        len(recs) == want_parts + 1 and len(set(roots)) == len(recs) and set(x[0] for x in recs) == job_txids)
    res["r: paid about the quote"] = abs(done["paid"] - quote["fee"]) <= quote["fee"] // 10

    print("f. fog lists it as one item")
    wait("A's fog round sees it", lambda: any(x["id"] == bid for x in call("A", "arc.fog").get("large", [])), 120)
    fa = [x for x in call("A", "arc.fog").get("large", []) if x["id"] == bid]
    summ = call("A", "explorer.arc_storage_summary")
    lb = summ.get("large_blobs", {})
    short_large = summ.get("short", {}).get("large", [])
    print(f"   arc.fog: {fa}; summary large_blobs count {lb.get('count')}, short large {[(x['id'][:12], x['parts'], x['copies'], x['floor']) for x in short_large]}")
    res["f: arc.fog lists it once with its parts"] = len(fa) == 1 and fa[0]["parts"] == want_parts and fa[0]["parts_here"] == want_parts
    res["f: the storage summary lists it as one item"] = lb.get("count") == 1 and lb["list"][0]["parts"] == want_parts
    res["f: short of copies lists it once (no keepers on a closed net)"] = [x["id"] for x in short_large] == [bid] and short_large[0]["parts"] == want_parts
    ids_short = summ.get("short", {}).get("blob_ids", [])
    res["f: its parts aren't listed one by one"] = not any(p in ids_short for p in r["part_ids"])

    print("m. B drops the parts (grace and idle passed), keeps the manifest")
    held_parts = lambda: [p for p in r["part_ids"] if p in call("B", "arc.fog").get("held_ids", [])]
    wait("B holds none of the parts", lambda: not held_parts(), GRACE + IDLE + 600, show=lambda: f"B holds {len(held_parts())} parts")
    res["m: B holds no part before reading"] = not held_parts()
    res["m: B keeps the manifest"] = bid in call("B", "arc.fog").get("held_ids", [])
    mid = SIZE // 2
    a, z = mid, mid + 999_999
    t = time.time()
    code, hdr, body = get("B", bid, (a, z))
    tr = time.time() - t
    print(f"   B: range {a}-{z}: {code}, {len(body)} bytes in {tr:.1f} s; Content-Range {hdr.get('Content-Range')}")
    res["m: a range from the middle, fetched over the mesh"] = code == 206 and body == data[a:z + 1] and hdr.get("Content-Range") == f"bytes {a}-{z}/{SIZE}"
    res["m: B holds the parts it fetched"] = len(held_parts()) >= 1
    print("s. a range across a part boundary")
    a, z = 3 * PART - 1000, 3 * PART + 999
    code, hdr, body = get("B", bid, (a, z))
    res["s: across a part boundary"] = code == 206 and body == data[a:z + 1]
    t = time.time()
    code, hdr, body = get("B", bid)
    tw = time.time() - t
    print(f"   B: the whole: {code}, {len(body)} bytes in {tw:.1f} s, type {hdr.get('Content-Type')}")
    res["m: the whole from B, its sha256 right"] = code == 200 and len(body) == SIZE and hashlib.sha256(body).hexdigest() == sha and hdr.get("Content-Type") == "video/mp4"
    code2, _, body2 = get("B", bid, (SIZE - 10, SIZE - 1))
    res["m: checked against its root (served again, not refused)"] = code2 == 206 and body2 == data[-10:]
    blob = call("B", "arc.blob", id=bid)
    res["m: arc.blob tells it's large"] = blob.get("large", {}).get("parts") == want_parts and blob["large"]["bytes"] == SIZE
    print(f"TIMES: publish {t1 - t0:.0f} s for {h1 - h0} blocks; range read {tr:.1f} s; whole read {tw:.1f} s")
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
