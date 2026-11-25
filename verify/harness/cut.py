#!/usr/bin/env python3
"""Cut C functions out of a kernel source file, verbatim.

usage: cut.py FILE NAME [NAME...]

Prints each named function's definition exactly as it is in FILE: from the line that starts its
definition (the return type's line) to the closing brace in column 0. Fails if a name is not
found exactly once, so a release that moves or renames a function stops the harness.
"""
import re
import sys


def cut(lines, name):
    pat = re.compile(r'^[A-Za-z_].*\b' + re.escape(name) + r'\s*\(')
    starts = [i for i, l in enumerate(lines)
              if pat.match(l) and not l.rstrip().endswith(';')]
    # keep definitions only: the opening brace follows before any ';'
    defs = []
    for i in starts:
        j = i
        while j < len(lines) and '{' not in lines[j] and ';' not in lines[j]:
            j += 1
        if j < len(lines) and lines[j].startswith('{'):
            defs.append(i)
    if len(defs) != 1:
        sys.exit(f"cut.py: {name}: found {len(defs)} definitions, want 1")
    i = defs[0]
    end = next(k for k in range(i, len(lines)) if lines[k].startswith('}'))
    return i + 1, end + 1, lines[i:end + 1]


def main():
    path, names = sys.argv[1], sys.argv[2:]
    lines = open(path).read().split('\n')
    for name in names:
        a, b, body = cut(lines, name)
        print(f"/* {name}: lines {a}-{b} */")
        print('\n'.join(body))
        print()


if __name__ == '__main__':
    main()
