#!/usr/bin/env python3
"""Proofs run only on the four broker-owned image builders/validators.

Secrets stay in RAM or private runtime files. Standard output is a small proof;
commands and secret-bearing exception text are never returned to CodeBuild.
"""
import base64
import contextlib
import hashlib
import hmac
import io
import ipaddress
import json
import os
from pathlib import Path
import re
import secrets
import signal
import socket
import ssl
import struct
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request


AMI = Path(__file__).resolve().parent
TLS = Path("/run/fcvm-fleet/tls")
GUARD = "cve-2026-98432-mitigation.service"


def need(value, message):
    if not value:
        raise RuntimeError(message)


def command(args, timeout=120):
    return subprocess.check_output(args, text=True, stderr=subprocess.DEVNULL, timeout=timeout).strip()


def boot():
    return {"boot_id": Path("/proc/sys/kernel/random/boot_id").read_text().strip(), "kernel_release": os.uname().release}


def guard():
    """Use the publisher's reviewed proof, rather than a weaker second checker."""
    source = (AMI / "build-ami.sh").read_text()
    proof = source.split("# BEGIN IMAGE GUARD PROOF\n", 1)[1].split("# END IMAGE GUARD PROOF", 1)[0]
    implementation = AMI / "common/cve-2026-98432-mitigation.py"
    argv = ["-", hashlib.sha256(implementation.read_bytes()).hexdigest()]
    for kind, names in (("host", ("fcvm-fleet-agent.service", "fcvm-fleet-store.service")),
                        ("tile", ("fcvm-fleet-renderer.service", "fcvm-fleet-nginx.service"))):
        argv.extend(name + "=" + hashlib.sha256((AMI / kind / name).read_bytes()).hexdigest() for name in names)
    previous = sys.argv
    try:
        sys.argv = argv
        with contextlib.redirect_stdout(io.StringIO()):
            exec(compile(proof, "reviewed-image-publication-proof", "exec"), {"__name__": "__main__"})
    finally:
        sys.argv = previous
    return True


def private_file(path, data):
    path = Path(path)
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    need(not path.parent.is_symlink() and path.parent.stat().st_uid == 0, "unsafe runtime directory")
    fd, temporary = tempfile.mkstemp(prefix=".pipeline-", dir=path.parent)
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "wb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def secret(arn, region):
    import boto3
    value = json.loads(boto3.client("secretsmanager", region_name=region)
        .get_secret_value(SecretId=arn)["SecretString"])
    need(set(value) == {"cert", "key", "scene_token"}
         and all(isinstance(item, str) and item for item in value.values()), "invalid pipeline TLS secret")
    return value


def cert_hash(cert):
    # The first PEM block is the leaf; the secret may also contain a chain.
    match = re.search(r"-----BEGIN CERTIFICATE-----.*?-----END CERTIFICATE-----", cert, re.S)
    need(match, "pipeline TLS certificate has no leaf")
    return hashlib.sha256(ssl.PEM_cert_to_DER_cert(match.group())).hexdigest()


def renderer():
    value = command(["podman", "image", "inspect", "localhost/fcvm-renderer:fleet", "--format", "{{.Id}}"])
    need(re.fullmatch(r"sha256:[0-9a-f]{64}", value), "renderer image has no digest")
    return {"image_id": value}


def bucket_client(bucket, prefix, region):
    import boto3
    need(re.fullmatch(r"[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]", bucket), "invalid pipeline bucket")
    need(re.fullmatch(r"builders/arn:aws:ec2:[a-z0-9-]+:\d{12}:instance/i-[0-9a-f]{17}/", prefix),
         "invalid broker-owned builder prefix")
    return boto3.client("s3", region_name=region)


def upload(client, bucket, key, path):
    size = path.stat().st_size
    need(0 < size < 5 * 1024**3, "single-PUT artifact is empty or oversized")
    with path.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    checksum = base64.b64encode(bytes.fromhex(digest)).decode()
    # Single PUT preserves the full-object SHA256. Multipart SHA256 is composite
    # and cannot be compared with the digest of the original file.
    with path.open("rb") as stream:
        response = client.put_object(Bucket=bucket, Key=key, Body=stream, ContentLength=size,
            Metadata={"sha256": digest}, ChecksumAlgorithm="SHA256", ChecksumSHA256=checksum)
    version = response.get("VersionId")
    need(isinstance(version, str) and version not in {"", "null"}, "artifact bucket versioning is not active")
    metadata = client.head_object(Bucket=bucket, Key=key, VersionId=version, ChecksumMode="ENABLED")
    need(metadata.get("Metadata", {}).get("sha256") == digest, "uploaded artifact digest changed")
    need(metadata.get("ChecksumSHA256") == checksum and metadata.get("ContentLength") == size
         and metadata.get("VersionId") == version, "uploaded artifact server checksum or version differs")
    return {"sha256": digest, "size": metadata["ContentLength"], "version_id": version}


def export_renderer(bucket, prefix, region):
    client = bucket_client(bucket, prefix, region)
    path = Path("/mnt/fcvm-btrfs/renderer.oci.tar.zst")
    need(path.is_file() and not path.is_symlink(), "renderer export is absent")
    return {**upload(client, bucket, prefix + "renderer.oci.tar.zst", path), **renderer()}


def store_parts():
    root = Path("/var/lib/fcvm-fleet/store")
    parts = (root / "parts").read_text().splitlines()
    need(parts == [f"part-{index:02d}" for index in range(16)], "store is not the complete sixteen-part image")
    names = {name + ".tar.zst" for name in [*parts, "dirs"]}
    need({path.name for path in root.glob("*.tar.zst")} == names, "store archive set differs from its manifest")
    for name in names:
        path = root / name
        need(path.is_file() and not path.is_symlink() and 0 < path.stat().st_size < 5 * 1024**3,
             "store archive is missing, unsafe or oversized")
    return root, names


def export_store(bucket, prefix, region):
    client = bucket_client(bucket, prefix, region)
    root, names = store_parts()
    receipts = {}
    for name in sorted(names):
        path = root / name
        receipts[name] = upload(client, bucket, prefix + "store/" + name, path)
    manifest = {name: value["sha256"] for name, value in receipts.items()}
    private_file(root / "sha256.json", json.dumps(manifest, sort_keys=True, separators=(",", ":")).encode())
    for name in ("parts", "sha256.json"):
        receipts[name] = upload(client, bucket, prefix + "store/" + name, root / name)
    return {"objects": receipts}


def tile_tls(arn, region):
    # Only the certificate fetcher receives the pipeline identity. Public service
    # fragments remain byte-for-byte those verified by the image publisher.
    need(re.fullmatch(r"arn:aws:secretsmanager:[a-z0-9-]+:\d{12}:secret:[A-Za-z0-9/_+=.@-]+", arn), "invalid pipeline TLS ARN")
    need(re.fullmatch(r"[a-z]+-[a-z]+-\d", region), "invalid pipeline secret region")
    path = Path("/etc/systemd/system/fcvm-fleet-tls.service.d/20-pipeline-tls.conf")
    private_file(path, ("[Service]\nEnvironment=FLEET_TLS_SECRET=" + arn
        + "\nEnvironment=FLEET_SECRET_REGION=" + region + "\n").encode())
    command(["systemctl", "daemon-reload"])
    command(["systemctl", "restart", "fcvm-fleet-tls.service"], timeout=180)
    return {"configured": True}


def tile_ready():
    command(["systemctl", "start", "fcvm-fleet-renderer.service", "fcvm-fleet-nginx.service"])
    wait(lambda: (request(8080, "/health")[1] or {}).get("ready") is True)
    # The trace phase requires this measured first-frame record, not an HTTP 200.
    command(["/usr/local/lib/fcvm-fleet/first-frame.sh"], timeout=610)
    return {"ready": True}


def clean_identity():
    # These are pipeline-owned paths on fresh factory instances, never user homes.
    for name in ("/etc/fcvm-fleet/agent.env", "/var/lib/fleet-ami/env",
                 "/etc/systemd/system/fcvm-fleet-tls.service.d/20-pipeline-tls.conf"):
        Path(name).unlink(missing_ok=True)
    for root in (Path("/etc/fcvm-fleet/tls"), Path("/etc/fcvm-fleet/scene"), TLS):
        if root.exists():
            need(root.is_dir() and not root.is_symlink(), "unsafe factory TLS directory")
            for path in root.iterdir():
                need(path.is_file() and not path.is_symlink(), "unexpected factory TLS entry")
                path.unlink()
    command(["systemctl", "daemon-reload"])


def sanitized():
    forbidden = ["/etc/fcvm-fleet/agent.env", "/var/lib/fleet-ami/env",
        "/root/.aws/credentials", "/root/.git-credentials", "/home/ubuntu/.aws/credentials",
        "/home/ubuntu/.git-credentials", "/root/.codex/auth.json", "/home/ubuntu/.codex/auth.json"]
    need(not any(Path(name).exists() for name in forbidden), "factory image contains enrollment or personal credentials")
    return True


def seal(kind):
    need(kind in {"host", "tile"}, "invalid factory kind")
    clean_identity()
    sanitized()
    guard()
    if kind == "host":
        store_parts()
    command(["cloud-init", "clean", "--logs", "--seed"])
    Path("/etc/machine-id").write_text("")
    command(["sync"])
    return {"guard": True, "sanitized": True}


def request(port, path, body=None, token=None):
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = "Bearer " + token
    req = urllib.request.Request(f"http://127.0.0.1:{port}{path}",
        data=None if body is None else json.dumps(body).encode(), headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=5) as response:
            data = response.read(1048577)
            need(len(data) <= 1048576, "renderer health is oversized")
            return response.status, json.loads(data)
    except urllib.error.HTTPError as error:
        return error.code, None


def wait(predicate, seconds=300):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            if predicate():
                return
        except (OSError, urllib.error.URLError, ValueError, RuntimeError):
            pass
        time.sleep(1)
    raise RuntimeError("isolated renderer did not become ready")


def exact(sock, count):
    need(count <= 1048576, "renderer frame exceeds the proof bound")
    result = bytearray()
    while len(result) < count:
        value = sock.recv(count - len(result))
        need(value, "renderer stream closed")
        result.extend(value)
    return bytes(result)


def ws_frame(sock):
    a, b = exact(sock, 2)
    need(a & 0x80 and not b & 0x80, "unexpected renderer websocket framing")
    length = b & 127
    if length == 126:
        length = struct.unpack("!H", exact(sock, 2))[0]
    elif length == 127:
        length = struct.unpack("!Q", exact(sock, 8))[0]
    return a & 15, exact(sock, length)


def stream(port, token, *, denied=False):
    with socket.create_connection(("127.0.0.1", port), timeout=10) as sock:
        key = base64.b64encode(os.urandom(16)).decode()
        sock.sendall((f"GET /frames?h=64&q=lo HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n"
            f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n").encode())
        head = bytearray()
        while not head.endswith(b"\r\n\r\n"):
            need(len(head) < 8192, "renderer websocket headers are oversized")
            head.extend(exact(sock, 1))
        need(bytes(head).startswith(b"HTTP/1.1 101 "), "renderer rejected websocket upgrade")
        payload = json.dumps({"admit": token}).encode()
        need(len(payload) < 126, "admission fixture is oversized")
        mask = os.urandom(4)
        sock.sendall(bytes([0x81, 0x80 | len(payload)]) + mask
            + bytes(value ^ mask[index % 4] for index, value in enumerate(payload)))
        for _ in range(10):
            opcode, value = ws_frame(sock)
            if denied:
                need(opcode != 2, "an unauthorized client received video")
                if opcode == 8:
                    need(value[:2] == struct.pack("!H", 1008), "unauthorized stream did not fail closed")
                    return
            elif opcode == 2:
                need(len(value) > 22 and value[:5] == b"FCVF\x01", "renderer sent invalid video framing")
                return
            else:
                need(opcode != 8, "authorized renderer stream closed")
        raise RuntimeError("renderer did not prove stream admission")


def scene_proof(port, new, old):
    key = secrets.token_bytes(32)
    scene = {"epoch_unix_ms": int(time.time() * 1000) - 1000,
        "rect": {"x": 0, "y": 0, "w": 1, "h": 1}, "image_aspect": 1,
        "camera": [{"t": 0, "pos": [0, 3, 8], "look": [0, 1, 0], "fov_deg": 60}],
        "time_of_day": 0.5, "words": [], "lights": [], "admit_keys": [key.hex()]}
    need(request(port, "/scene", scene)[0] == 401, "unauthenticated scenes are accepted")
    need(request(port, "/scene", scene, old["scene_token"])[0] == 401, "the build secret token authorizes the validator")
    need(request(port, "/scene", scene, new["scene_token"])[0] == 200, "the new scene token was not loaded")
    stream(port, "invalid", denied=True)
    signed = "v1." + str(int(time.time()) + 60) + ".factory-validator"
    token = signed + "." + base64.urlsafe_b64encode(hmac.digest(key, signed.encode(), "sha256")).rstrip(b"=").decode()
    stream(port, token)


def tls_proof(port, document):
    with socket.create_connection(("127.0.0.1", port), timeout=5) as raw:
        # Exact leaf pinning to the isolated test certificate replaces public CA trust.
        with ssl._create_unverified_context().wrap_socket(raw, server_hostname="factory.invalid") as sock:
            need(hashlib.sha256(sock.getpeercert(binary_form=True)).hexdigest() == cert_hash(document["cert"]),
                 "the restored service is still serving the build certificate")


def ordering(kind):
    names = ("fcvm-fleet-store.service",) if kind == "host" else (
        "fcvm-fleet-renderer.service", "fcvm-fleet-nginx.service")
    at = int(command(["systemctl", "show", GUARD, "--property=ActiveEnterTimestampMonotonic", "--value"]))
    need(at > 0 and command(["systemctl", "is-active", GUARD]) == "active", "guard did not start at boot")
    for name in names:
        need(command(["systemctl", "is-active", name]) == "active", "required image service is not active")
        started = int(command(["systemctl", "show", name, "--property=ExecMainStartTimestampMonotonic", "--value"]))
        need(started >= at, "a fleet service started before the guard")


def host_proof(new, old):
    need(not Path("/etc/fcvm-fleet/agent.env").exists(), "validator enrolled in the live fleet")
    paths = sorted(Path("/sys/bus/pci/drivers/vfio-pci").glob("????:??:??.?"))
    need(len(paths) >= 2, "host has no two passable GPUs")
    token = urllib.request.Request("http://169.254.169.254/latest/api/token", method="PUT",
        headers={"X-aws-ec2-metadata-token-ttl-seconds": "60"})
    with urllib.request.urlopen(token, timeout=5) as response:
        imds = response.read(4096).decode()
    def metadata(path):
        req = urllib.request.Request("http://169.254.169.254/latest/meta-data/" + path,
            headers={"X-aws-ec2-metadata-token": imds})
        with urllib.request.urlopen(req, timeout=5) as response:
            return response.read(4096).decode().strip()
    mac = metadata("mac")
    prefix = metadata("network/interfaces/macs/" + mac + "/ipv6-prefix").splitlines()[0]
    address = str(ipaddress.ip_network(prefix, strict=False).network_address + 3)
    private_file(TLS / "tls.json", json.dumps(new).encode())
    argv = ["fcvm", "snapshot", "run", "--snapshot", "phone-warm", "--name", "factory-phone-proof",
        "--gpu", paths[1].name, "--publish", "127.0.0.1:20095:8080", "--publish", "127.0.0.1:20094:443",
        "--ipv6-prefix", prefix, "--ipv6-address", address, "--map", str(TLS) + ":/run/fleet-tls:ro"]
    with tempfile.TemporaryFile() as log:
        process = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True)
        try:
            wait(lambda: (request(20095, "/health")[1] or {}).get("phone", {}).get("verified") is True, seconds=600)
            wait(lambda: tls_proof(20094, new) is None, seconds=30)
            scene_proof(20095, new, old)
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=60)
                except subprocess.TimeoutExpired:
                    # The broker terminates this validator; never proceed to promotion.
                    raise RuntimeError("phone validator did not stop") from None
    # A separate GPU VM uses the pre-start renderer snapshot and proves real frames too.
    private_file(TLS / "tls.json", json.dumps(new).encode())
    address = str(ipaddress.ip_network(prefix, strict=False).network_address + 2)
    argv = ["fcvm", "podman", "run", "--name", "factory-renderer-proof", "--kernel-profile", "gpu",
        "--gpu", paths[0].name, "--publish", "127.0.0.1:20099:8080", "--publish", "127.0.0.1:20098:443",
        "--network", "routed", "--ipv6-prefix", prefix, "--ipv6-address", address,
        "--map", str(TLS) + ":/run/fleet-tls:ro", "--env", "TLS_CERT_FILE=/run/fleet-tls/tls.json",
        "--env", "TLS_PORT=443", "localhost/fcvm-renderer:fleet"]
    with tempfile.TemporaryFile() as log:
        process = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True)
        try:
            wait(lambda: (request(20099, "/health")[1] or {}).get("ready") is True, seconds=600)
            tls_proof(20098, new)
            scene_proof(20099, new, old)
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                process.wait(timeout=60)


def verify(kind, expected_image, build_arn, test_arn, region):
    need(kind in {"host", "tile"}, "invalid validator kind")
    sanitized()
    guard()
    ordering(kind)
    need(renderer()["image_id"] == expected_image, "validated renderer differs from the paired build")
    old, new = secret(build_arn, region), secret(test_arn, region)
    need(cert_hash(old["cert"]) != cert_hash(new["cert"]) and old["scene_token"] != new["scene_token"],
         "build and validation certificates/tokens are not distinct")
    if kind == "host":
        store_parts()
        host_proof(new, old)
    else:
        wait(lambda: (request(8080, "/health")[1] or {}).get("ready") is True)
        tls_proof(443, new)
        scene_proof(8080, new, old)
    return {**boot(), "guard": True, "frames": True, "phone_clone": kind == "host",
            "tls_rotation": True, "sanitized": True,
            "build_cert_sha256": cert_hash(old["cert"]), "test_cert_sha256": cert_hash(new["cert"])}


def main():
    actions = {"boot": boot, "renderer": renderer, "export-renderer": export_renderer,
        "export-store": export_store, "tile-tls": tile_tls, "tile-ready": tile_ready,
        "seal": seal, "verify": verify}
    try:
        need(len(sys.argv) >= 2 and sys.argv[1] in actions, "unknown image operation")
        result = actions[sys.argv[1]](*sys.argv[2:])
        print(json.dumps(result, sort_keys=True))
    except BaseException:
        # Exception payloads from AWS/subprocesses can contain headers or secret values.
        print("isolated image operation failed", file=sys.stderr)
        raise SystemExit(1) from None


if __name__ == "__main__":
    main()
