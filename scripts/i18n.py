#!/usr/bin/env python3
"""The window's languages (docs/I18N.md). Python 3 standard library only.

  scripts/i18n.py extract          every English text in crates/arcade-desktop/dist -> crates/arcade-desktop/i18n/en.json
  scripts/i18n.py status [CODE]    how much of each language is translated, and what is missing or no longer used
  scripts/i18n.py build            crates/arcade-desktop/i18n/<code>.json -> dist/locales/<code>.js (what the window loads)

The keys follow i18n.js exactly: a text node's text with its whitespace folded; a sentence with markup
in it as a skeleton ("Use <0>Restore</0> on Home"); a JS template's ${...} as {0}, {1}, ... in order.
"""
import json, re, sys
from html.parser import HTMLParser
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DIST = ROOT / "crates/arcade-desktop/dist"
I18N = ROOT / "crates/arcade-desktop/i18n"
SKIP_FILES = {"jsqr.js", "i18n.js", "media-worklet.js"}

LETTER = re.compile(r"[A-Za-z]")
norm = lambda s: re.sub(r"\s+", " ", s).strip()
SKIP = set("SCRIPT STYLE TEXTAREA CODE PRE KBD SAMP SVG CANVAS IFRAME VIDEO AUDIO NOSCRIPT".split())
BLOCK = set(("DIV P SECTION ARTICLE ASIDE TABLE THEAD TBODY TFOOT TR TD TH UL OL LI DL DT DD H1 H2 H3 H4 H5 H6 "
             "SELECT OPTION OPTGROUP TEXTAREA NAV HEADER MAIN FOOTER FORM FIELDSET DETAILS SUMMARY FIGURE BLOCKQUOTE HR IMG").split())
VOID = set("AREA BASE BR COL EMBED HR IMG INPUT LINK META PARAM SOURCE TRACK WBR".split())
ATTRS = ("title", "placeholder", "aria-label", "alt")


def id_like(s):
    return " " not in s and (len(s) > 24 or re.match(r"^(@|0x|https?:)|^[0-9a-f]{8,}$", s, re.I))


# ---------- a small DOM, enough to walk HTML the way i18n.js walks the live one ----------
class El:
    def __init__(self, tag, attrs, line):
        self.tag, self.attrs, self.kids, self.line = tag.upper(), dict(attrs), [], line


class Tree(HTMLParser):
    def __init__(self, src):
        super().__init__(convert_charrefs=True)
        self.root = El("ROOT", {}, 1); self.stack = [self.root]
        self.feed(src); self.close()

    def handle_starttag(self, tag, attrs):
        el = El(tag, attrs, self.getpos()[0]); self.stack[-1].kids.append(el)
        if el.tag not in VOID:
            self.stack.append(el)

    def handle_startendtag(self, tag, attrs):
        self.stack[-1].kids.append(El(tag, attrs, self.getpos()[0]))

    def handle_endtag(self, tag):
        T = tag.upper()
        for i in range(len(self.stack) - 1, 0, -1):
            if self.stack[i].tag == T:
                del self.stack[i:]; return

    def handle_data(self, d):
        self.stack[-1].kids.append(d)


def skip(el):
    return el.tag in SKIP or el.attrs.get("translate") == "no" or "notranslate" in (el.attrs.get("class") or "").split() \
        or "contenteditable" in el.attrs


def inline(el):
    return el.tag not in BLOCK and el.tag not in SKIP and all(inline(k) for k in el.kids if isinstance(k, El))


def skeleton(el, n):
    s = ""
    for k in el.kids:
        if isinstance(k, str):
            s += k
        else:
            i = n[0]; n[0] += 1
            s += f"<{i}>{skeleton(k, n)}</{i}>" if k.kids else f"<{i}/>"
    return s


def walk(el, out, where):
    if skip(el) and el.tag != "TEXTAREA":
        return
    for a in ATTRS:
        v = el.attrs.get(a)
        if v and LETTER.search(v) and not id_like(norm(v)) and "{" not in v[:1]:
            out.setdefault(norm(v), f"{where}:{el.line}")
    if el.tag == "INPUT" and (el.attrs.get("type") or "").lower() in ("button", "submit", "reset") and LETTER.search(el.attrs.get("value") or ""):
        out.setdefault(norm(el.attrs["value"]), f"{where}:{el.line}")
    if skip(el):
        return
    words = any(isinstance(k, str) and LETTER.search(k) for k in el.kids)
    kids = [k for k in el.kids if isinstance(k, El)]
    if el.tag != "ROOT" and words and kids and all(inline(k) for k in kids):
        key = norm(skeleton(el, [0]))
        out.setdefault(key, f"{where}:{el.line}")
        return
    for k in el.kids:
        if isinstance(k, El):
            walk(k, out, where)
        elif LETTER.search(k):
            t = norm(k)
            if t and not id_like(t):
                out.setdefault(t, f"{where}:{el.line}")


def html_texts(src, where, out):
    walk(Tree(src).root, out, where)


# ---------- JS: string literals and templates ----------
CODEY = re.compile(r"(className|classList\.\w+|querySelector(All)?|getElementById|\$|addEventListener|removeEventListener|invoke|dataset\.\w+|"
                   r"createElement|setAttribute|getAttribute|removeAttribute|localStorage\.\w+|style\.\w+|matches|closest|"
                   r"postMessage|fetch|new RegExp|split|join|replace|startsWith|endsWith|includes|indexOf|toggle|"
                   r"\.type|\.kind|\.mode|\.method|\.state|\.tab|\.id|\.key|\.code|\.name|\.cmd)\s*(\(|===?|!==?|=|\[)\s*[\w\s,.\[\]]*$")
CMP = re.compile(r"(===?|!==?|case)\s*$")


def js_strings(src):
    """(value, line, prev_code, is_template, holes) for each string literal; templates have ${...} replaced by {n}."""
    i, n, line, out = 0, len(src), 1, []
    last_sig = ""
    while i < n:
        c = src[i]
        if c == "\n":
            line += 1; i += 1; continue
        if src.startswith("//", i):
            j = src.find("\n", i); i = n if j < 0 else j; continue
        if src.startswith("/*", i):
            j = src.find("*/", i + 2); line += src.count("\n", i, j); i = n if j < 0 else j + 2; continue
        if c == "/" and last_sig in "(,=:[!&|?{};+-*%<>~^" :
            j = i + 1; cls = False
            while j < n and src[j] != "\n":
                if src[j] == "\\": j += 2; continue
                if src[j] == "[": cls = True
                elif src[j] == "]": cls = False
                elif src[j] == "/" and not cls: break
                j += 1
            i = j + 1; last_sig = "x"; continue
        if c in "\"'":
            j = i + 1; buf = []
            while j < n and src[j] != c:
                if src[j] == "\\" and j + 1 < n:
                    e = src[j + 1]
                    if e == "u" and src[j + 2:j + 3] == "{":
                        k = src.find("}", j); buf.append(chr(int(src[j + 3:k], 16))); j = k + 1; continue
                    if e == "u":
                        buf.append(chr(int(src[j + 2:j + 6], 16))); j += 6; continue
                    buf.append({"n": "\n", "t": "\t", "r": ""}.get(e, e)); j += 2; continue
                buf.append(src[j]); j += 1
            out.append(("".join(buf), line, src[max(0, i - 60):i], False))
            i = j + 1; last_sig = "x"; continue
        if c == "`":
            j = i + 1; buf = []; holes = 0
            while j < n and src[j] != "`":
                if src[j] == "\\":
                    buf.append({"n": "\n", "t": "\t"}.get(src[j + 1], src[j + 1])); j += 2; continue
                if src.startswith("${", j):
                    depth, k = 1, j + 2
                    while k < n and depth:
                        if src[k] == "{": depth += 1
                        elif src[k] == "}": depth -= 1
                        elif src[k] in "\"'`":                       # a string inside the hole
                            q = src[k]; k += 1
                            while k < n and src[k] != q:
                                k += 2 if src[k] == "\\" else 1
                        k += 1
                    inner = src[j + 2:k - 1]
                    for v, ln, prev, tpl in js_strings(inner):       # strings inside holes count too ("@" + tag, ...)
                        out.append((v, line, prev, tpl))
                    buf.append("{%d}" % holes); holes += 1; j = k; continue
                if src[j] == "\n": line += 1
                buf.append(src[j]); j += 1
            out.append(("".join(buf), line, src[max(0, i - 60):i], True))
            i = j + 1; last_sig = "x"; continue
        if not c.isspace():
            last_sig = c
        i += 1
    return out


def js_texts(src, where, out):
    for v, line, prev, tpl in js_strings(src):
        if not LETTER.search(v) or CODEY.search(prev) or CMP.search(prev):
            continue
        t = norm(v)
        if not t or id_like(t):
            continue
        if "<" in t and re.search(r"</?[a-zA-Z][^>]*>", t):          # markup: walk it like the page
            html_texts(v, f"{where}:{line}", out)
            continue
        if " " not in t and not re.match(r"^[A-Z][a-z]+[.!?…:]?$|^[A-Z][a-z]+[A-Z]", t) and not re.search(r"[.!?…:]$", t):
            continue                                                  # one word in lower case: a name in code, not text
        if re.fullmatch(r"[a-z0-9_\-]+( [a-z0-9_\-]+)*", t) and len(t.split()) <= 3 and not tpl:
            if not re.search(r"\b(a|an|the|of|to|in|on|is|no|not|yet|your|this|with|for|and|or)\b", t):
                continue                                              # "pill ok", "small primary": class lists
        toks = t.split()
        if all(re.fullmatch(r"[a-z0-9]+(-[a-z0-9{}]+)*|n?\{\d+\}", w) for w in toks) and any(re.search(r"-|\d|\{", w) for w in toks):
            continue                                                  # class lists: "mono faint df-a", "x-cols3 n{0}"
        if re.search(r"(serif|monospace|system-ui)\s*$|^\(prefers-|^--[a-z-]+:|^[a-z-]+:[^ ]", t):
            continue                                                  # font stacks, media queries, CSS values
        if re.search(r"[{;]\s*[a-z-]+\s*:[^ ]|^[.:#@*\[][\w-]*[^{]*\{", t):
            continue                                                  # CSS
        if re.fullmatch(r"[MmLlHhVvCcSsQqTtAaZz0-9 .,\-]+", t):
            continue                                                  # SVG path data
        if re.fullmatch(r"(\{\d+\}|\W)*", t):
            continue
        out.setdefault(t, f"{where}:{line}")


EXTRA = {}                                              # texts i18n.js itself shows (none: the languages show their own names)


def extract():
    out = dict(EXTRA)
    rt = I18N / "runtime.json"                              # texts that come from the service or data, seen at run time (I18N.dump())
    if rt.exists():
        out.update(json.loads(rt.read_text()))
    for p in sorted(DIST.glob("*.html")):
        html_texts(p.read_text(), p.name, out)
    for p in sorted(DIST.glob("*.js")):
        if p.name not in SKIP_FILES:
            js_texts(p.read_text(), p.name, out)
    I18N.mkdir(exist_ok=True)
    (I18N / "en.json").write_text(json.dumps(dict(sorted(out.items())), ensure_ascii=False, indent=1) + "\n")
    print(f"{len(out)} texts -> {(I18N / 'en.json').relative_to(ROOT)}")


def langs():
    return sorted(p.stem for p in I18N.glob("*.json") if p.stem not in ("en", "runtime"))


def check(code, en):
    tr = json.loads((I18N / f"{code}.json").read_text())
    done = {k for k, v in tr.items() if isinstance(v, str) and v}
    tags = lambda s: sorted(re.findall(r"</?\d+/?>", s))
    holes = lambda s: re.findall(r"\{\d+\}", s)
    # every tag kept once; placeholders each at most once and none new (a plural "s" placeholder may go)
    bad = [k for k in done & set(en) if tags(k) != tags(tr[k]) or not set(holes(tr[k])) <= set(holes(k))
           or len(holes(tr[k])) != len(set(holes(tr[k])))]
    return tr, done, bad


def status(only=None):
    en = json.loads((I18N / "en.json").read_text())
    for code in [only] if only else langs():
        tr, done, bad = check(code, en)
        have = done & set(en)
        print(f"{code}: {len(have)}/{len(en)} translated ({100 * len(have) // max(len(en), 1)}%), "
              f"{len(set(en) - done)} missing, {len(done - set(en))} no longer used, {len(bad)} with placeholders/markup that don't match")
        for k in bad[:20]:
            print("  mismatch:", k[:100])


def build():
    en = json.loads((I18N / "en.json").read_text())
    (DIST / "locales").mkdir(exist_ok=True)
    for code in langs():
        tr, done, bad = check(code, en)
        keep = {k: tr[k] for k in sorted(done) if k not in bad}
        js = ("// Generated by scripts/i18n.py build from crates/arcade-desktop/i18n/%s.json: edit that file, not this one.\n"
              "I18N.add(%s, %s);\n") % (code, json.dumps(code), json.dumps(keep, ensure_ascii=False, indent=0))
        (DIST / "locales" / f"{code}.js").write_text(js)
        print(f"{code}: {len(keep)} texts -> dist/locales/{code}.js")


if __name__ == "__main__":
    cmd = sys.argv[1] if len(sys.argv) > 1 else "status"
    {"extract": extract, "build": build}.get(cmd, lambda: status(sys.argv[2] if len(sys.argv) > 2 else None))()
