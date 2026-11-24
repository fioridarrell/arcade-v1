#!/usr/bin/env python3
"""Append and check the Windows installer payload trailer.

The layout is specified in crates/arcade-install. This script is what the
release workflow runs. Keep the two in step: the Rust tests build an installer
both ways and require the bytes to match.
"""

from __future__ import annotations

import argparse
import hashlib
import sys
from pathlib import Path

MAGIC = b"ARCDPAY1"
MARKER = "ARCADE_PAYLOAD_SHA256="


class PayloadError(Exception):
    """The payload was not accepted. Nothing was extracted."""


def sha256_hex(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def normalize_hash(expected: str) -> str:
    digest = expected.strip().lower()
    if len(digest) != 64 or any(c not in "0123456789abcdef" for c in digest):
        raise PayloadError(
            "The embedded SHA-256 is not 64 hex characters. "
            "Nothing was extracted and nothing was installed."
        )
    return digest


def _u16(blob: bytes, off: int) -> int:
    if off + 2 > len(blob):
        raise PayloadError(
            "This installer is not a readable PE file. "
            "Nothing was extracted and nothing was installed."
        )
    return int.from_bytes(blob[off : off + 2], "little")


def _u32(blob: bytes, off: int) -> int:
    if off + 4 > len(blob):
        raise PayloadError(
            "This installer is not a readable PE file. "
            "Nothing was extracted and nothing was installed."
        )
    return int.from_bytes(blob[off : off + 4], "little")


def security_directory(image: bytes) -> tuple[int, int] | None:
    """Certificate-table file offset and size, or None when there is no signature."""
    if len(image) < 0x40 or image[0:2] != b"MZ":
        return None
    e_lfanew = _u32(image, 0x3C)
    if e_lfanew + 24 > len(image) or image[e_lfanew : e_lfanew + 4] != b"PE\0\0":
        raise PayloadError(
            "This installer is not a readable PE file. "
            "Nothing was extracted and nothing was installed."
        )
    coff = e_lfanew + 4
    opt_size = _u16(image, coff + 16)
    opt = coff + 20
    magic = _u16(image, opt)
    if magic == 0x10B:
        num_off, dd_off = 92, 96
    elif magic == 0x20B:
        num_off, dd_off = 108, 112
    else:
        raise PayloadError(
            "This installer is not a readable PE file. "
            "Nothing was extracted and nothing was installed."
        )
    if opt + dd_off > len(image) or opt_size < dd_off:
        raise PayloadError(
            "This installer is not a readable PE file. "
            "Nothing was extracted and nothing was installed."
        )
    if _u32(image, opt + num_off) <= 4:
        return None
    entry = opt + dd_off + 4 * 8
    off = _u32(image, entry)
    size = _u32(image, entry + 4)
    if off == 0 or size == 0:
        return None
    if off > len(image) or size > len(image) - off:
        raise PayloadError(
            "The Authenticode certificate table points outside the installer. "
            "Nothing was extracted and nothing was installed."
        )
    return off, size


def content_end(image: bytes) -> int:
    sec = security_directory(image)
    if sec is None:
        return len(image)
    return sec[0]


def split_payload(image: bytes) -> tuple[int, bytes]:
    end = content_end(image)
    if end < 16:
        raise PayloadError(
            "The installer is too small to carry a payload. "
            "Nothing was extracted and nothing was installed."
        )
    magic_at = None
    for pad in range(8):
        at = end - pad - 8
        if at < 8:
            break
        if image[at : at + 8] == MAGIC:
            magic_at = at
            break
    if magic_at is None:
        raise PayloadError(
            "This installer has no Arcade payload trailer (ARCDPAY1). "
            "Nothing was extracted and nothing was installed."
        )
    len_at = magic_at - 8
    payload_len = int.from_bytes(image[len_at : len_at + 8], "little")
    if payload_len <= 0 or payload_len > len_at:
        raise PayloadError(
            "The installer payload length is not valid. "
            "Nothing was extracted and nothing was installed."
        )
    start = len_at - payload_len
    return start, bytes(image[start:len_at])


def append_payload(stub: bytes, payload: bytes) -> bytes:
    if not payload:
        raise PayloadError(
            "The payload is empty. Nothing was extracted and nothing was installed."
        )
    if security_directory(stub) is not None:
        raise PayloadError(
            "The installer stub is already Authenticode-signed. "
            "Append the payload before signing. "
            "Nothing was extracted and nothing was installed."
        )
    return stub + payload + len(payload).to_bytes(8, "little") + MAGIC


def _push_hash(found: list[str], raw: str) -> None:
    digest = raw.strip().lower()
    if len(digest) < 64:
        return
    digest = digest[:64]
    if len(digest) == 64 and all(c in "0123456789abcdef" for c in digest) and digest not in found:
        found.append(digest)


def hashes_in(region: bytes) -> list[str]:
    found: list[str] = []
    marker_u = MARKER.encode("utf-16le")
    start = 0
    while True:
        rel = region.find(marker_u, start)
        if rel < 0:
            break
        i = rel + len(marker_u)
        chars = region[i : i + 128]
        try:
            text = chars.decode("utf-16le")
        except UnicodeDecodeError:
            text = ""
        _push_hash(found, text)
        start = min(rel + 2, len(region))
        if start <= rel:
            break
    marker_a = MARKER.encode("ascii")
    start = 0
    while True:
        rel = region.find(marker_a, start)
        if rel < 0:
            break
        i = rel + len(marker_a)
        _push_hash(found, region[i : i + 64].decode("ascii", "replace"))
        start = min(rel + 1, len(region))
        if start <= rel:
            break
    return found


def embedded_hash(image: bytes) -> str:
    try:
        start, _payload = split_payload(image)
        region = image[:start]
    except PayloadError as err:
        if "no Arcade payload trailer" not in str(err):
            raise
        region = image
    found = hashes_in(region)
    if len(found) != 1:
        raise PayloadError(
            f"Expected one embedded payload SHA-256, found {len(found)}. "
            "Nothing was extracted and nothing was installed."
        )
    return found[0]


def verify(image: bytes, expected: str) -> bytes:
    want = normalize_hash(expected)
    _start, payload = split_payload(image)
    actual = sha256_hex(payload)
    if actual != want:
        raise PayloadError(
            "Arcade payload SHA-256 does not match the hash embedded in this installer. "
            f"Expected {want}, got {actual}. "
            "Nothing was extracted and nothing was installed."
        )
    return payload


def check_installer(image: bytes, expected: str) -> None:
    """The embedded hash, the caller's hash and the payload hash are one hash."""
    want = normalize_hash(expected)
    embedded = embedded_hash(image)
    payload = verify(image, want)
    actual = sha256_hex(payload)
    if embedded != want or actual != want:
        raise PayloadError(
            "Arcade payload SHA-256 does not match the hash embedded in this installer. "
            f"Expected {want}, embedded {embedded}, got {actual}. "
            "Nothing was extracted and nothing was installed."
        )


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description="Arcade installer payload trailer")
    sub = parser.add_subparsers(dest="cmd", required=True)

    append = sub.add_parser("append", help="append a payload zip to an unsigned installer stub")
    append.add_argument("--stub", required=True)
    append.add_argument("--payload", required=True)
    append.add_argument("--out", required=True)

    check = sub.add_parser("check", help="fail unless the payload matches the embedded hash")
    check.add_argument("--installer", required=True)
    check.add_argument("--expected", required=True)

    embedded = sub.add_parser("embedded", help="print the SHA-256 stored in the stub")
    embedded.add_argument("--installer", required=True)

    args = parser.parse_args(argv)
    try:
        if args.cmd == "append":
            stub = Path(args.stub).read_bytes()
            payload = Path(args.payload).read_bytes()
            Path(args.out).write_bytes(append_payload(stub, payload))
        elif args.cmd == "check":
            image = Path(args.installer).read_bytes()
            check_installer(image, args.expected)
        elif args.cmd == "embedded":
            image = Path(args.installer).read_bytes()
            print(embedded_hash(image))
    except PayloadError as err:
        print(err, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
