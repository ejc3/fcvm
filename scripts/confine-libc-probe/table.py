"""strace logs (run.sh) of the glibc and musl probes -> {function: [syscalls]} for each libc, as JSON."""
import json, re, sys
def parse(path):
    out, cur = {}, None
    for line in open(path):
        line = re.sub(r"^\d+\s+", "", line.strip())
        m = re.match(r'write\(-1, "([^"]+)"', line)
        if m:
            cur = m.group(1); out.setdefault(cur, []); continue
        m = re.match(r"([a-z0-9_]+)\(", line)
        if cur and m and m.group(1) not in out[cur]:
            out[cur].append(m.group(1))
    return out
json.dump({"glibc": parse(sys.argv[1]), "musl": parse(sys.argv[2])}, sys.stdout, indent=1, sort_keys=True)
