#!/usr/bin/env python3
"""Check that relative links in the repository's Markdown files resolve.

Usage: scripts/check-links.py [ROOT]. Exits 1 and lists each broken link.
External (http, https, mailto) links and pure #anchors are not checked.
"""
import os
import re
import sys

LINK = re.compile(r"\[[^\]]*\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)")
FENCE = re.compile(r"^\s*(```|~~~)")
SKIP_DIRS = {".git", "target"}


def links(path):
    in_fence = False
    with open(path, encoding="utf-8") as f:
        for n, line in enumerate(f, 1):
            if FENCE.match(line):
                in_fence = not in_fence
                continue
            if in_fence:
                continue
            line = re.sub(r"`[^`]*`", "", line)
            for m in LINK.finditer(line):
                yield n, m.group(1)


def main():
    root = sys.argv[1] if len(sys.argv) > 1 else "."
    broken = 0
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(d for d in dirnames if d not in SKIP_DIRS)
        for name in sorted(filenames):
            if not name.endswith(".md"):
                continue
            path = os.path.join(dirpath, name)
            for n, target in links(path):
                if re.match(r"^[a-z]+:", target) or target.startswith("#"):
                    continue
                file = target.split("#", 1)[0]
                if not os.path.exists(os.path.join(dirpath, file)):
                    print(f"{path}:{n}: broken link {target}")
                    broken += 1
    sys.exit(1 if broken else 0)


if __name__ == "__main__":
    main()
