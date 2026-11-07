#!/usr/bin/env python3
"""Helpers for build-image.sh, kept here so they are tested (test_image.py).

  image_tool.py render TEMPLATE OUT IMAGE_INDEX ADMIN_PASSWORD_FILE SANDBOX_PASSWORD_FILE
      write autounattend.xml from its template: each placeholder must appear exactly once,
      and the passwords are XML-escaped
  image_tool.py setuplog COM2_LOG OUT
      cut the guest's setup log out of what its finish step wrote to COM2
  image_tool.py expiry SETUP_LOG
      print the evaluation's expiry (slmgr /xpr) as an ISO 8601 UTC time
"""
import datetime
import re
import sys
from xml.sax.saxutils import escape

PLACEHOLDERS = ("@IMAGE_INDEX@", "@ADMIN_PASSWORD@", "@SANDBOX_PASSWORD@")
LOG_START = "===FCVM-SETUP-LOG==="
LOG_END = "===FCVM-SETUP-END==="


def render(template: str, image_index: str, admin_password: str, sandbox_password: str) -> str:
    for key in PLACEHOLDERS:
        n = template.count(key)
        if n != 1:
            raise ValueError(f"{key} appears {n} times in the template; it must appear once")
    if not image_index.isdigit():
        raise ValueError(f"image index {image_index!r} is not a number")
    if admin_password == sandbox_password:
        raise ValueError("the administrator and sandbox passwords must differ")
    return (template.replace("@IMAGE_INDEX@", image_index)
            .replace("@ADMIN_PASSWORD@", escape(admin_password))
            .replace("@SANDBOX_PASSWORD@", escape(sandbox_password)))


def setup_log(com2: bytes) -> str:
    raw = com2.decode("utf-8", "replace")
    if LOG_START not in raw or LOG_END not in raw:
        raise ValueError("the guest wrote no setup log to COM2")
    body = raw.split(LOG_START, 1)[1].split(LOG_END, 1)[0]
    return body.replace("\r", "").replace("\0", "").lstrip("﻿")


def expiry(setup_log_text: str) -> str:
    """The image's time zone is UTC (autounattend.xml), so slmgr's local time is UTC."""
    m = re.search(r"will expire (\d+)/(\d+)/(\d+) (\d+):(\d+):(\d+) (AM|PM)", setup_log_text)
    if not m:
        raise ValueError("no evaluation expiry in the setup log")
    mo, d, y, h, mi, s, ap = m.groups()
    hour = int(h) % 12 + (12 if ap == "PM" else 0)
    t = datetime.datetime(int(y), int(mo), int(d), hour, int(mi), int(s), tzinfo=datetime.timezone.utc)
    return t.isoformat()


def main(argv):
    if len(argv) == 6 and argv[0] == "render":
        out = render(open(argv[1]).read(), argv[3], open(argv[4]).read(), open(argv[5]).read())
        with open(argv[2], "w") as f:
            f.write(out)
    elif len(argv) == 3 and argv[0] == "setuplog":
        with open(argv[2], "w") as f:
            f.write(setup_log(open(argv[1], "rb").read()))
    elif len(argv) == 2 and argv[0] == "expiry":
        print(expiry(open(argv[1], encoding="utf-8", errors="replace").read()))
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    try:
        main(sys.argv[1:])
    except ValueError as e:
        sys.exit(f"image_tool: {e}")
