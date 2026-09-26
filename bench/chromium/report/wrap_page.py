#!/usr/bin/env python3
"""Wrap the report fragment into a complete HTML document for GitHub Pages.

shared-nothing-renders.html is a <title> and a <style>, then the page body: a
header, a <main> and a footer. It has no <!doctype>, <html>, <head> or <body>.
GitHub Pages serves a file as it is, so .github/workflows/pages.yml runs this
at deploy time and publishes the result beside docs/. The fragment stays the
one source, and the lints in test_reqbench.py keep reading it unchanged.

    python3 bench/chromium/report/wrap_page.py SOURCE > OUT
"""
import re
import sys

PREAMBLE = (
    "<!DOCTYPE html>\n"
    '<html lang="en">\n'
    "<head>\n"
    '<meta charset="utf-8">\n'
    '<meta name="viewport" content="width=device-width, initial-scale=1">\n'
)


def wrap(fragment):
    """The fragment's <title> and <style> go in <head>; everything after the
    style goes in <body>, so the header and footer sit outside <main>."""
    split = re.search(r"</style>\r?\n?", fragment)
    if not split:
        raise ValueError("the fragment has no </style>")
    head, body = fragment[:split.end()], fragment[split.end():]
    if "<main>" in head:
        raise ValueError("the fragment has a <main> before the end of its style")
    if "<main>" not in body:
        raise ValueError("the fragment has no <main> after its style")
    return (PREAMBLE + head + "</head>\n<body>\n" + body.rstrip("\n")
            + "\n</body>\n</html>\n")


def main(argv):
    """Write the wrapped document for the one source path in argv to stdout."""
    if len(argv) != 2:
        sys.stderr.write(__doc__)
        return 2
    with open(argv[1], encoding="utf-8") as handle:
        sys.stdout.write(wrap(handle.read()))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
