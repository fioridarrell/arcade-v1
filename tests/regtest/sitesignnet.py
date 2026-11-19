#!/usr/bin/env python3
"""ARCADE ID signing with the home key (ARCADE Git #23), on a closed local ARCADE chain.

A writes the chain and claims @alice. A fake website page (this script: X25519 + AES-256-GCM as crate::sitesign
does) pairs with A's app, learns the home key from the hello, signs in with key "home" (checked with msg.verify),
has A's server build a post (psbt.feed_post, channel txtok) and a like (psbt.feed_act), each approved in the app
(id.sign_decide) and broadcast with tx.broadcast; the post and the like show in A's feed. Requests the app must
refuse: another chain, a home-chain PSBT spending a coin that isn't the home chain's.

Run inside: unshare -Urn sh -c "ip link set lo up; python3 sitesignnet.py ARCADED WORKDIR"
Needs Python's `cryptography` package.
"""
import base64, hashlib, json, os, shutil, socket, subprocess, sys, time, urllib.request

from cryptography.hazmat.primitives.asymmetric.x25519 import X25519PrivateKey, X25519PublicKey
from cryptography.hazmat.primitives.ciphers.aead import AESGCM
from cryptography.hazmat.primitives import serialization

EXE, WORK = sys.argv[1], sys.argv[2]
PORTS = {"A": (18661, 18671)}
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


def web(n, cmd, **p):
    """The web API, as a website's server (or its page, for the public box commands) calls it."""
    url = f"http://127.0.0.1:{PORTS[n][0]}/api/v1/{cmd.replace('.', '/')}"
    rq = urllib.request.Request(url, data=json.dumps(p).encode(), headers={"Content-Type": "application/json"})
    try:
        body = urllib.request.urlopen(rq, timeout=60).read()
    except urllib.error.HTTPError as e:
        body = e.read()
    r = json.loads(body)
    if not r.get("ok"):
        raise RuntimeError(f"{n} web {cmd}: {r.get('error')}")
    return r["result"]


def start(n, settings=None):
    http, mesh = PORTS[n]
    args = ["nice", "-n", "19", EXE, "--home", home(n), "--http-port", str(http), "--mesh-port", str(mesh), "--stun-port", "0", "--no-seeds"]
    if settings:
        args += ["--arc-local", settings]
    log = open(os.path.join(WORK, f"{n}.log"), "a")
    procs[n] = subprocess.Popen(args, stdout=log, stderr=subprocess.STDOUT)
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
            r = cond()
            if r:
                return r
        except Exception as e:
            print("   ", what, ":", e)
        if show and time.time() - last > 30:
            last = time.time()
            print("   ", show())
        time.sleep(2)
    print(f"  timed out: {what}")
    return None


def tag_status(n, tag):
    for t in call(n, "tag.list"):
        if t["tag"] == tag:
            return t["status"]
    return None


class Page:
    """The website's page end of an ARCADE ID signing pairing (crate::sitesign)."""

    def __init__(self, n, host):
        self.n, self.host = n, host
        self.sk = X25519PrivateKey.generate()
        self.pk = self.sk.public_key().public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)
        self.pairbox = os.urandom(16).hex()
        self.k = None

    def code(self):
        return f"arcadesign://{self.host}/{self.pairbox}?k={self.pk.hex()}&a=http://127.0.0.1:{PORTS[self.n][0]}"

    def box(self, what):
        return hashlib.sha256(f"ARCADE/sign/{what}\0".encode() + self.k).digest()[:16].hex()

    def seal(self, v):
        nonce = os.urandom(12)
        return {"n": nonce.hex(), "c": base64.b64encode(AESGCM(self.k).encrypt(nonce, json.dumps(v).encode(), None)).decode()}

    def open(self, sealed):
        return json.loads(AESGCM(self.k).decrypt(bytes.fromhex(sealed["n"]), base64.b64decode(sealed["c"]), None))

    def hello(self):
        msgs = web(self.n, "id.box_take", box=self.pairbox)["msgs"]
        h = json.loads(msgs[0])
        apk = bytes.fromhex(h["apk"])
        shared = self.sk.exchange(X25519PublicKey.from_public_bytes(apk))
        self.k = hashlib.sha256(b"ARCADE/sign\0" + shared + apk + self.pk).digest()
        return self.open(h)

    def ask(self, req):
        web(self.n, "id.box_put", box=self.box("req"), msg=json.dumps(self.seal(req)))

    def answers(self):
        return [self.open(json.loads(m)) for m in web(self.n, "id.box_take", box=self.box("res"))["msgs"]]


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
    h = lambda: call("A", "arc.status")["height"]

    print("1. A writes until it has ARCADE to spare, then claims @alice on ARCADE testnet")
    if not wait("A has 3 ARCADE", lambda: call("A", "wallet.send", chain="arctest", to=a_arc, amount=300_000_000) is not None, 1500, show=lambda: f"height {h()}"):
        return 1
    call("A", "tag.claim", tag="alice.arctest")
    if not wait("@alice confirmed", lambda: tag_status("A", "alice") == "confirmed", 600, show=lambda: f"height {h()} claim {tag_status('A', 'alice')}"):
        return 1

    print("2. The page pairs; the hello names the home key beside the Dogecoin one")
    page = Page("A", "txtok.example")
    shown = call("A", "id.sign_pair", qr=page.code())
    print("   the app shows:", shown)
    call("A", "id.sign_pair", qr=page.code(), send=True)
    hello = page.hello()
    print("   hello:", hello)
    hk = hello.get("home") or {}
    if hk.get("chain") != "arctest" or hk.get("address") != a_arc or len(hk.get("pubkey", "")) != 66:
        fails.append(f"the hello's home key: {hk}, want arctest {a_arc}")
    if hello.get("chain") != "doge" or not hello.get("address", "").startswith("D") or hello.get("tag") != "alice":
        fails.append(f"the hello's Dogecoin fields: {hello}")
    if (shown.get("home") or {}).get("address") != a_arc:
        fails.append(f"id.sign_pair doesn't show the home address: {shown}")

    def approve(rid):
        """Approve in the app the one request waiting (the page's `rid`) and read the page's answer."""
        pend = wait(f"request {rid} reaches the app", lambda: call("A", "id.sign_pending"), 60)
        if not pend:
            return None, next((a for a in page.answers() if a.get("id") == rid), None)
        preview = pend[0]
        call("A", "id.sign_decide", id=preview["id"], ok=True)
        ans = wait(f"answer to {rid}", lambda: next((a for a in page.answers() if a.get("id") == rid), None), 60)
        return preview, ans

    print("3. Sign-in: a message with key home, checked with msg.verify (ARCADE's prefix)")
    nonce = "txtok sign-in " + os.urandom(8).hex()
    page.ask({"id": "s1", "type": "message", "key": "home", "message": nonce, "label": "Sign in to TxTOK"})
    preview, ans = approve("s1")
    print("   preview:", preview)
    print("   answer:", ans)
    if not preview or preview.get("key") != "home" or preview.get("address") != a_arc or preview.get("prefix") != "Pepecoin Signed Message:\n":
        fails.append(f"the sign-in preview: {preview}")
    if not ans or not ans.get("ok") or ans.get("address") != a_arc:
        fails.append(f"the sign-in answer: {ans}")
    else:
        v = web("A", "msg.verify", address=a_arc, message=nonce, signature=ans["signature"])
        bad = web("A", "msg.verify", address=a_arc, message=nonce + "x", signature=ans["signature"])
        print("   msg.verify:", v, "tampered:", bad["valid"])
        if v["valid"] is not True or bad["valid"] is not False:
            fails.append(f"msg.verify {v} {bad}")

    print("4. A post: psbt.feed_post (txtok), signed in the app, broadcast, in the feed")
    built = web("A", "psbt.feed_post", address=hk["address"], pubkey=hk["pubkey"], chain="arctest", text="hello from txtok", channel="txtok")
    print("   built:", json.dumps(built["summary"]))
    page.ask({"id": "p1", "type": "psbt", "chain": "arctest", "psbt": built["psbt"], "finalize": True, "label": "Post on TxTOK"})
    preview, ans = approve("p1")
    print("   preview:", json.dumps(preview)[:500] if preview else None)
    if not preview or preview.get("chain") != "arctest" or preview.get("unit") in (None, "DOGE") or any(o["to"].startswith("D") for o in preview.get("outputs", [])):
        fails.append(f"the post's preview isn't in ARCADE testnet's terms: {preview}")
    post_txid = None
    if not ans or not ans.get("ok") or not ans.get("txHex"):
        fails.append(f"the post's answer: {ans}")
    else:
        fin = web("A", "psbt.finalize", psbt=ans["psbtHex"], chain="arctest")
        if fin["hex"] != ans["txHex"]:
            fails.append("psbt.finalize of the signed PSBT isn't the app's txHex")
        r = call("A", "tx.broadcast", hex=ans["txHex"], chain="arctest")
        post_txid = r["txid"]
        print("   broadcast:", r)
        if post_txid != fin["txid"]:
            fails.append(f"txid {post_txid} vs finalize's {fin['txid']}")
        got = wait("the post in the feed", lambda: next((p for p in call("A", "feed", order="new", channel="txtok")["posts"] if p.get("txid") == post_txid), None), 300)
        print("   feed:", json.dumps(got)[:400] if got else None)
        if not got or got.get("text") != "hello from txtok" or (got.get("author") or {}).get("tag") not in ("alice", "alice.arctest"):
            fails.append(f"the post in the feed: {got}")

    if post_txid:
        print("5. A like on it: psbt.feed_act, signed, broadcast, counted")
        wait("the post in a block", lambda: (call("A", "explorer.tx", chain="arctest", txid=post_txid, local=True).get("height") or 0) > 0, 300)
        # A's own wallet is the same account and writes too (its profile, say): a coin it spent meanwhile is
        # refused at the broadcast, and the site builds again, as a website does.
        for attempt in range(3):
            built = web("A", "psbt.feed_act", address=hk["address"], pubkey=hk["pubkey"], kind="like", target=post_txid)
            page.ask({"id": f"l{attempt}", "type": "psbt", "chain": "arctest", "psbt": built["psbt"], "finalize": True})
            preview, ans = approve(f"l{attempt}")
            err = None
            if not ans or not ans.get("ok"):
                err = f"the like's answer: {ans}"
            else:
                try:
                    call("A", "tx.broadcast", hex=ans["txHex"], chain="arctest")
                    break
                except RuntimeError as e:
                    err = str(e)
            print("   built again:", err, "| it spent", [i["coin"] for i in built["summary"]["inputs"]])
            for what, f in [("tx.coins", lambda: call("A", "tx.coins", addresses=[hk["address"]], chain="arctest")["addresses"]),
                            ("pool", lambda: call("A", "arc.pool")), ("events", lambda: call("A", "events", n=8))]:
                try:
                    print(f"     {what}:", json.dumps(f())[:900])
                except Exception as e:
                    print(f"     {what}: {e}")
            if attempt == 2:
                fails.append(err)
            time.sleep(3)
        got = wait("the like counted", lambda: next((p for p in call("A", "feed", order="new", channel="txtok")["posts"] if p.get("txid") == post_txid and (p.get("likes") or 0) >= 1), None), 300)
        print("   likes:", got and got.get("likes"))
        if not got:
            fails.append("the like isn't counted")

    print("6. Refused: another chain; a home-chain PSBT spending a coin that isn't ARCADE testnet's")
    page.ask({"id": "x1", "type": "psbt", "chain": "ltc", "psbt": built["psbt"], "finalize": True})
    ans = wait("x1 refused", lambda: next((a for a in page.answers() if a.get("id") == "x1"), None), 60)
    print("   another chain:", ans)
    if not ans or ans.get("ok") is not False or "only" not in ans.get("error", ""):
        fails.append(f"a PSBT on another chain: {ans}")
    # A transaction of the same key whose coin no ARCADE block holds (a coin chain's, say): its previous transaction
    # is made up, so the PSBT is well formed but the coin isn't the home chain's.
    script = bytes.fromhex("76a914") + b58decode(a_arc) + bytes.fromhex("88ac")
    def tx(ins, outs):
        o = (1).to_bytes(4, "little") + bytes([len(ins)])
        for txid, vout in ins:
            o += txid + vout.to_bytes(4, "little") + b"\0" + b"\xff" * 4
        o += bytes([len(outs)])
        for v, s in outs:
            o += v.to_bytes(8, "little") + bytes([len(s)]) + s
        return o + b"\0" * 4
    prev = tx([(b"\x11" * 32, 0)], [(500_000_000, script)])
    ptxid = hashlib.sha256(hashlib.sha256(prev).digest()).digest()
    unsigned = tx([(ptxid, 0)], [(499_000_000, script)])
    def kv(k, v):
        return bytes([len(k)]) + k + (bytes([len(v)]) if len(v) < 253 else b"\xfd" + len(v).to_bytes(2, "little")) + v
    fake = b"psbt\xff" + kv(b"\0", unsigned) + b"\0" + kv(b"\0", prev) + b"\0" + b"\0"
    page.ask({"id": "x2", "type": "psbt", "chain": "arctest", "psbt": base64.b64encode(fake).decode(), "finalize": True})
    ans = wait("x2 refused", lambda: next((a for a in page.answers() if a.get("id") == "x2"), None), 60)
    print("   a coin that isn't the home chain's:", ans)
    if not ans or ans.get("ok") is not False or "isn't an unspent coin" not in ans.get("error", ""):
        fails.append(f"a home-chain PSBT spending another chain's coin: {ans}")
    if call("A", "id.sign_pending"):
        fails.append("a refused request is still waiting in the app")

    print("RESULT:", "PASS" if not fails else "FAIL", fails)
    return 0 if not fails else 1


rc = 1
try:
    rc = main()
finally:
    for n in list(procs):
        stop(n)
sys.exit(rc)
