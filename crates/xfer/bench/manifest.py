#!/usr/bin/env python3
"""Compare two trees by a full manifest: type, mode, mtime (ns), size, SHA-256 of regular files,
symlink targets and hard-link groups. Names are compared as bytes (non-UTF-8 safe).

usage: manifest.py SRC DST      exit 0 when identical, 1 with the first differences printed
"""
import hashlib
import os
import stat
import sys


def manifest(root):
    root = os.fsencode(root)
    out = {}
    first_of_inode = {}
    for dirpath, dirnames, filenames in os.walk(root, followlinks=False):
        dirnames.sort()
        for name in sorted(dirnames) + sorted(filenames):
            p = os.path.join(dirpath, name)
            rel = os.path.relpath(p, root)
            st = os.lstat(p)
            rec = {"mode": stat.S_IMODE(st.st_mode)}
            if stat.S_ISLNK(st.st_mode):
                rec["kind"] = "symlink"
                rec["target"] = os.readlink(p)
                rec.pop("mode")  # symlink modes are not portable
            elif stat.S_ISDIR(st.st_mode):
                rec["kind"] = "dir"
                rec["mtime_ns"] = st.st_mtime_ns
            elif stat.S_ISREG(st.st_mode):
                rec["kind"] = "file"
                rec["size"] = st.st_size
                rec["mtime_ns"] = st.st_mtime_ns
                key = (st.st_dev, st.st_ino)
                if st.st_nlink > 1 and key in first_of_inode:
                    rec["link_of"] = first_of_inode[key]
                else:
                    first_of_inode.setdefault(key, rel)
                    h = hashlib.sha256()
                    with open(p, "rb") as f:
                        for b in iter(lambda: f.read(1 << 20), b""):
                            h.update(b)
                    rec["sha256"] = h.hexdigest()
            else:
                rec["kind"] = "other"
            out[rel] = rec
    return out


def main():
    a, b = manifest(sys.argv[1]), manifest(sys.argv[2])
    diffs = []
    for k in sorted(set(a) | set(b)):
        if a.get(k) != b.get(k):
            diffs.append((k, a.get(k), b.get(k)))
    files = sum(1 for r in a.values() if r["kind"] == "file")
    print(f"entries={len(a)} files={files} differences={len(diffs)}")
    for k, x, y in diffs[:20]:
        print(f"  {k!r}: src={x} dst={y}")
    sys.exit(1 if diffs else 0)


if __name__ == "__main__":
    main()
