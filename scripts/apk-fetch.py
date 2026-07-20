#!/usr/bin/env python3
"""Resolve and extract Alpine packages into a rootfs directory — no `apk`.

Runs on any host with Python 3.12+ (macOS included): parses the Alpine
APKINDEX, resolves the dependency closure of the requested packages, then
downloads each .apk and extracts its files into <dest>.

Usage: apk-fetch.py <mirror> <arch> <dest> <pkg> [<pkg> ...]
  mirror e.g. https://dl-cdn.alpinelinux.org/alpine/latest-stable
"""
import gzip
import io
import sys
import tarfile
import urllib.request
import zlib

REPOS = ["main", "community"]


def fetch(url):
    with urllib.request.urlopen(url) as r:
        return r.read()


def strip_ver(tok):
    """Drop version constraints: 'python3>=3.12' -> 'python3'."""
    for op in ("<", ">", "=", "~"):
        i = tok.find(op)
        if i != -1:
            tok = tok[:i]
    return tok


def load_index(mirror, arch):
    """Return (index, provides): name->(repo,ver,deps), token->name."""
    index, provides = {}, {}
    for repo in REPOS:
        raw = fetch(f"{mirror}/{repo}/{arch}/APKINDEX.tar.gz")
        tf = tarfile.open(fileobj=io.BytesIO(raw), mode="r:gz")
        text = tf.extractfile("APKINDEX").read().decode()
        for rec in text.split("\n\n"):
            fields = {}
            for line in rec.splitlines():
                if len(line) > 2 and line[1] == ":":
                    fields.setdefault(line[0], []).append(line[2:])
            name = fields.get("P", [None])[0]
            if not name:
                continue
            ver = fields["V"][0]
            deps = fields.get("D", [""])[0].split()
            index[name] = (repo, ver, deps)
            provides[name] = name
            for p in fields.get("p", [""])[0].split():
                provides[strip_ver(p)] = name
    return index, provides


def resolve(index, provides, wanted):
    """Depth-first closure of package names for the wanted tokens."""
    seen, order, stack = set(), [], list(wanted)
    while stack:
        tok = strip_ver(stack.pop())
        if tok.startswith("!"):  # conflict marker
            continue
        name = tok if tok in index else provides.get(tok)
        if not name:
            sys.stderr.write(f"warn: unresolved dependency {tok!r}\n")
            continue
        if name in seen:
            continue
        seen.add(name)
        order.append(name)
        stack.extend(index[name][2])
    return order


def gzip_members(raw):
    """Split concatenated gzip streams (an .apk is signature+control+data)."""
    off = 0
    while off < len(raw):
        dec = zlib.decompressobj(16 + zlib.MAX_WBITS)
        out = dec.decompress(raw[off:]) + dec.flush()
        consumed = len(raw) - off - len(dec.unused_data)
        if consumed <= 0:
            break
        off += consumed
        yield out


def extract_apk(raw, dest):
    # Real files live in the data segment; signature/control hold only
    # dotfiles (.PKGINFO, .SIGN…), which we skip — so extracting non-dot
    # entries from every segment yields exactly the package's files.
    for blob in gzip_members(raw):
        try:
            tf = tarfile.open(fileobj=io.BytesIO(blob), mode="r:")
        except tarfile.ReadError:
            continue
        for m in tf.getmembers():
            if m.name.startswith("."):
                continue
            try:
                tf.extract(m, dest, filter="tar")
            except Exception as e:  # pre-existing symlink/dir from the base
                sys.stderr.write(f"warn: extract {m.name}: {e}\n")


def main():
    if len(sys.argv) < 5:
        sys.exit(__doc__)
    mirror, arch, dest = sys.argv[1:4]
    wanted = sys.argv[4:]
    index, provides = load_index(mirror, arch)
    order = resolve(index, provides, wanted)
    for name in order:
        repo, ver, _ = index[name]
        extract_apk(fetch(f"{mirror}/{repo}/{arch}/{name}-{ver}.apk"), dest)
    print(f"installed {len(order)} packages into {dest}")


if __name__ == "__main__":
    main()
