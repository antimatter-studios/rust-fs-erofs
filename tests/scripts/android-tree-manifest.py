#!/usr/bin/env python3
"""Emit a stable content and metadata manifest for an extracted image tree."""

import base64
import hashlib
import os
import stat
import sys


def encoded(value: bytes) -> str:
    return base64.urlsafe_b64encode(value).decode("ascii")


def entry(path: bytes, root: bytes) -> str:
    metadata = os.lstat(path)
    relative = os.path.relpath(path, root)
    mode = metadata.st_mode
    if stat.S_ISDIR(mode):
        kind = "d"
        digest = "-"
    elif stat.S_ISREG(mode):
        kind = "f"
        hasher = hashlib.sha256()
        with open(path, "rb") as source:
            for chunk in iter(lambda: source.read(1024 * 1024), b""):
                hasher.update(chunk)
        digest = hasher.hexdigest()
    elif stat.S_ISLNK(mode):
        kind = "l"
        digest = hashlib.sha256(os.readlink(path)).hexdigest()
    elif stat.S_ISFIFO(mode):
        kind = "p"
        digest = "-"
    elif stat.S_ISCHR(mode):
        kind = "c"
        digest = "-"
    elif stat.S_ISBLK(mode):
        kind = "b"
        digest = "-"
    elif stat.S_ISSOCK(mode):
        kind = "s"
        digest = "-"
    else:
        raise RuntimeError(f"unsupported file type at {os.fsdecode(relative)}")

    attributes = []
    for name in sorted(os.listxattr(path, follow_symlinks=False)):
        value = os.getxattr(path, name, follow_symlinks=False)
        attributes.append(f"{encoded(os.fsencode(name))}={encoded(value)}")

    return "\t".join(
        (
            encoded(relative),
            kind,
            f"{stat.S_IMODE(mode):04o}",
            str(metadata.st_uid),
            str(metadata.st_gid),
            str(metadata.st_size),
            str(metadata.st_mtime_ns),
            str(metadata.st_nlink),
            digest,
            ",".join(attributes) if attributes else "-",
        )
    )


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit("usage: android-tree-manifest.py DIRECTORY")
    root = os.fsencode(os.path.abspath(sys.argv[1]))
    rows = []
    for directory, dirnames, filenames in os.walk(root, followlinks=False):
        dirnames.sort()
        filenames.sort()
        rows.append(entry(directory, root))
        rows.extend(entry(os.path.join(directory, name), root) for name in dirnames)
        rows.extend(entry(os.path.join(directory, name), root) for name in filenames)
    for row in sorted(rows):
        print(row)


if __name__ == "__main__":
    main()
