#!/usr/bin/env python3
"""Build one private host/tile pair through the fixed Fleet AMI broker.

The CodeBuild project supplies configuration. This program has no EC2 mutation
client, cannot choose a profile/network/type, and never promotes a template.
"""
import concurrent.futures
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import shlex
import ssl
import subprocess
import threading
import time


SLOTS = ("host-build", "tile-build", "host-verify", "tile-verify")
ROOT = Path(__file__).resolve().parents[2]
REMOTE = "/opt/fcvm-image-source"
GUEST = REMOTE + "/fleet/ami/pipeline_guest.py"
GUEST_PYTHON = "/opt/fcvm-image-sdk/bin/python"


def require(value, message):
    if not value:
        raise RuntimeError(message)


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


def input_digest(root, commit):
    """Hash the complete immutable tracked tree used by the image build."""
    actual = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
    require(re.fullmatch(r"[0-9a-f]{40}", commit) and actual == commit,
            "checkout does not match the broker's source revision")
    dirty = subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=normal"], cwd=root)
    require(not dirty, "image publication requires an unchanged checkout")
    records = subprocess.check_output(["git", "ls-tree", "-rz", commit, "--"], cwd=root)
    require(records, "image source inputs are absent")
    digest = hashlib.sha256(b"fcvm-fleet-ami-source-v1\0" + records).hexdigest()
    archive = subprocess.check_output(["git", "archive", "--format=tar", commit], cwd=root)
    return digest, archive


class Worker:
    """The broker issues prefix-scoped sessions; CodeBuild cannot assume a role."""
    def __init__(self, call, region, build_id, clock=time.time):
        self.call, self.region, self.clock = call, region, clock
        self.prefix = "runs/" + hashlib.sha256(build_id.encode()).hexdigest() + "/"
        self.lock = threading.Lock()
        self.expires = 0
        self.session = None

    def invalidate(self):
        with self.lock:
            self.expires = 0

    def client(self, name, region=None):
        import boto3
        from botocore.config import Config
        with self.lock:
            if self.clock() + 300 >= self.expires:
                result = self.call("credentials")
                require(result.get("asset_prefix") == self.prefix, "worker credentials name another build")
                creds = result["credentials"]
                self.expires = creds["Expiration"]
                require(self.clock() + 300 < self.expires <= self.clock() + 3600,
                        "worker credential lifetime is invalid")
                self.session = boto3.Session(aws_access_key_id=creds["AccessKeyId"],
                    aws_secret_access_key=creds["SecretAccessKey"],
                    aws_session_token=creds["SessionToken"], region_name=self.region)
            return self.session.client(name, region_name=region or self.region, config=Config(retries={"total_max_attempts": 1},
                connect_timeout=10, read_timeout=60))


class Pipeline:
    def __init__(self, broker, worker, config, root=ROOT, clock=time.time, sleep=time.sleep):
        self.broker, self.worker, self.config, self.root = broker, worker, config, Path(root)
        self.clock, self.sleep = clock, sleep
        self.build_id = config["build_id"]
        self.prefix = "runs/" + hashlib.sha256(self.build_id.encode()).hexdigest() + "/"
        self.deadline = 0
        self.instances = {}
        self.archive = None
        self.input_sha = None
        self.source_tree_sha = None
        self.builder_prefixes = {}
        self.store = None
        self.tls = None
        # The fixed broker has reserved concurrency one. Serialize both parallel
        # validators through the complete invocation, including its response body.
        self.call_lock = threading.Lock()
        self.cancelled = threading.Event()

    def remaining(self, *, cleanup=False):
        require(cleanup or not self.cancelled.is_set(), "a parallel validator failed")
        seconds = int(self.deadline - self.clock())
        require(seconds > 0, "the image build reached its fixed deadline")
        return seconds

    def call(self, action, slot=None, command_id=None):
        require(action in {"launch", "status", "reboot", "stop", "start", "seal", "cleanup", "credentials", "command_status"},
                "unsupported broker action")
        require(slot is None or slot in SLOTS, "unsupported broker slot")
        request = {"action": action, "build_id": self.build_id, "capability": self.config["capability"]}
        if slot:
            request["slot"] = slot
        if command_id:
            require(action == "command_status" and slot and re.fullmatch(r"[0-9a-f-]{36}", command_id),
                    "invalid broker command receipt")
            request["command_id"] = command_id
        retry_until = self.clock() + 60
        if self.deadline:
            retry_until = min(retry_until, self.deadline)
        with self.call_lock:
            while True:
                require(action in {"cleanup", "status"} or not self.cancelled.is_set(),
                        "a parallel validator failed")
                try:
                    reply = self.broker.invoke(FunctionName=self.config["broker_arn"],
                        InvocationType="RequestResponse", Payload=canonical(request))
                    require(not reply.get("FunctionError"), "image broker rejected the request")
                    body = reply["Payload"].read(131073)
                    require(len(body) <= 131072, "image broker response is too large")
                    value = json.loads(body)
                    require(isinstance(value, dict) and not value.get("error") and value.get("status") != "refused",
                            "image broker request failed")
                    return value
                except Exception as error:
                    code = getattr(error, "response", {}).get("Error", {}).get("Code")
                    # Lambda rejected a throttled invocation before execution.
                    # Network ambiguity, FunctionError and payload failures never replay.
                    if code != "TooManyRequestsException" or self.clock() >= retry_until:
                        raise
                    self.sleep(min(0.5, retry_until - self.clock()))

    def wait(self, predicate, *, cleanup=False):
        while True:
            self.remaining(cleanup=cleanup)
            value = predicate()
            if value:
                return value
            self.sleep(min(10, self.remaining(cleanup=cleanup)))

    def shell(self, slot, command, *, json_result=False):
        """Submit once. An uncertain send never replays a phase or launches a replacement."""
        require(slot in self.instances, "the broker has not assigned this slot")
        seconds = min(9000, self.remaining())
        ssm = self.worker.client("ssm")
        result = ssm.send_command(InstanceIds=[self.instances[slot]],
            DocumentName=self.config["document"], TimeoutSeconds=min(600, seconds),
            Parameters={"commands": ["set -eu\numask 077\n" + command],
                        "executionTimeout": [str(seconds)]})
        command_id = result["Command"]["CommandId"]

        def complete():
            invocation = self.call("command_status", slot, command_id)
            if invocation["status"] in {"Pending", "InProgress", "Delayed"}:
                return None
            require(invocation["status"] == "Success" and invocation.get("response_code") == 0,
                    "the isolated builder command failed")
            output = invocation.get("standard_output", "")
            require(len(output.encode()) <= 24000, "builder receipt is too large")
            return {"output": output}

        output = self.wait(complete)["output"]
        return json.loads(output) if json_result else None

    def guest(self, slot, action, *arguments):
        return self.shell(slot, shlex.join([GUEST_PYTHON, GUEST, action, *arguments]), json_result=True)

    def install(self, slot):
        key = self.prefix + "source.tar"
        url = self.worker.client("s3", self.config["asset_region"]).generate_presigned_url("get_object",
            Params={"Bucket": self.config["bucket"], "Key": key}, ExpiresIn=min(3600, self.remaining()))
        digest = hashlib.sha256(self.archive).hexdigest()
        command = "\n".join([
            "test ! -L " + REMOTE,
            "t=$(mktemp /var/tmp/fleet-image-source.XXXXXX)",
            "trap 'rm -f \"$t\"' EXIT",
            "curl --fail --silent --show-error --proto '=https' --tlsv1.2 --max-time 300 " + shlex.quote(url) + ' -o "$t"',
            "printf '%s  %s\\n' " + shlex.quote(digest) + ' "$t" | sha256sum -c - >/dev/null',
            "if [ -e " + REMOTE + " ]; then",
            "  test -d " + REMOTE,
            '  test "$(stat -c %u ' + REMOTE + ')" = 0',
            "  rm -rf -- " + REMOTE,
            "fi",
            "install -d -m 0755 " + REMOTE,
            'tar -xf "$t" --no-same-owner -C ' + REMOTE,
        ])
        self.shell(slot, command)
        self.shell(slot, "\n".join([
            "cloud-init status --wait > /var/log/fleet-image-cloud-init.log 2>&1",
            "export DEBIAN_FRONTEND=noninteractive",
            "if ! python3 -c 'import ensurepip, venv' >/dev/null 2>&1; then",
            "  apt-get -o DPkg::Lock::Timeout=120 update > /var/log/fleet-image-sdk.log 2>&1",
            "  apt-get -o DPkg::Lock::Timeout=120 -y install python3-venv >> /var/log/fleet-image-sdk.log 2>&1",
            "fi",
            "test ! -L /opt/fcvm-image-sdk",
            "python3 -m venv /opt/fcvm-image-sdk >> /var/log/fleet-image-sdk.log 2>&1",
            GUEST_PYTHON + " -m pip --isolated install --no-input --only-binary=:all: --require-hashes -r "
                + REMOTE + "/fleet/ami/requirements-pipeline.txt >> /var/log/fleet-image-sdk.log 2>&1",
        ]))
        if not slot.endswith("-build"):
            return
        env = {
            "FCVM_COMMIT": self.config["source_commit"],
            "FLEET_TLS_SECRET": self.config["tls_build_arn"],
            "FLEET_SECRET_REGION": self.config["secret_region"],
            "STORE_S3_REGION": self.config["final_store_region"], "STORE_S3_BUCKET": self.config["final_store_bucket"],
            "STORE_S3_PREFIX": "factory/" + self.input_sha + "/store/",
        }
        lines = "\n".join("export " + name + "=" + shlex.quote(value) for name, value in env.items()) + "\n"
        self.shell(slot, "install -d -m 0700 /var/lib/fleet-ami\ncat > /var/lib/fleet-ami/env <<'FLEET_BUILD_ENV'\n"
            + lines + "FLEET_BUILD_ENV\n")

    def launch(self, slot):
        value = self.call("launch", slot)
        instance = value.get("instance_id", "")
        require(re.fullmatch(r"i-[0-9a-f]{17}", instance), "broker returned an invalid instance")
        require(value.get("slot") == slot and value.get("deadline_unix") == self.deadline,
                "broker returned a different slot or deadline")
        require(instance not in self.instances.values(), "broker reused an instance between slots")
        self.instances[slot] = instance
        prefix = value.get("asset_prefix", "")
        require(re.fullmatch(r"builders/arn:aws:ec2:[a-z0-9-]+:\d{12}:instance/" + instance + "/", prefix),
                "broker returned an invalid builder prefix")
        self.builder_prefixes[slot] = prefix
        self.worker.invalidate()
        self.wait(lambda: self.call("status", slot).get("state") == "running")
        self.wait(lambda: self.online(slot))
        self.install(slot)

    def online(self, slot):
        values = self.worker.client("ssm").describe_instance_information(
            Filters=[{"Key": "InstanceIds", "Values": [self.instances[slot]]}])["InstanceInformationList"]
        return len(values) == 1 and values[0].get("PingStatus") == "Online"

    def phase(self, slot, kind, phase):
        path = f"{REMOTE}/fleet/ami/{kind}/build.sh"
        # Logs remain on the isolated builder. SSM and CI receive only exit status.
        self.shell(slot, "bash " + shlex.join([path, phase]) + " > /var/log/fleet-image-phase.log 2>&1")

    def security_update(self, slot):
        # Only disposable factory instances. Upgrade before the NVIDIA module is
        # built, then boot the installed kernel before either image recipe runs.
        self.shell(slot, "\n".join([
            "export DEBIAN_FRONTEND=noninteractive",
            "systemctl stop apt-daily.timer apt-daily-upgrade.timer",
            "apt-get -o DPkg::Lock::Timeout=120 update > /var/log/fleet-image-security-update.log 2>&1",
            "apt-get -o DPkg::Lock::Timeout=120 -y --with-new-pkgs upgrade >> /var/log/fleet-image-security-update.log 2>&1",
        ]))
        self.reboot(slot)

    def reboot(self, slot, *, stop_start=False):
        old = self.guest(slot, "boot")["boot_id"]
        if stop_start:
            self.call("stop", slot)
            self.wait(lambda: self.call("status", slot).get("state") == "stopped")
            self.call("start", slot)
        else:
            self.call("reboot", slot)
        self.wait(lambda: self.call("status", slot).get("state") == "running")
        # SSM's cached Online value is insufficient: require a command from a new boot.
        def changed():
            if not self.online(slot):
                return None
            try:
                current = self.guest(slot, "boot")["boot_id"]
            except Exception:
                # This command is read-only. A cached SSM Online value can precede
                # the new agent connection; no mutating phase is replayed here.
                return None
            return current if current != old else None
        return self.wait(changed)

    def build_host(self):
        slot = "host-build"
        self.launch(slot)
        self.security_update(slot)
        self.phase(slot, "host", "prep")
        self.reboot(slot)
        self.phase(slot, "host", "build")
        renderer = self.guest(slot, "export-renderer", self.config["bucket"], self.builder_prefixes[slot],
                              self.config["asset_region"])
        require(re.fullmatch(r"[0-9a-f]{64}", renderer.get("sha256", "")), "renderer export has no digest")
        require(re.fullmatch(r"sha256:[0-9a-f]{64}", renderer.get("image_id", "")), "renderer image has no identity")
        renderer["staged"] = self.stage(slot, "renderer.oci.tar.zst", renderer)
        self.phase(slot, "host", "bake")
        exported = self.guest(slot, "export-store", self.config["bucket"], self.builder_prefixes[slot], self.config["asset_region"])
        expected = {f"part-{index:02d}.tar.zst" for index in range(16)} | {"dirs.tar.zst", "parts", "sha256.json"}
        require(set(exported.get("objects", {})) == expected, "factory store export is incomplete")
        objects = {name: self.stage(slot, "store/" + name, receipt) for name, receipt in exported["objects"].items()}
        self.store = {"bucket": self.config["bucket"], "region": self.config["asset_region"],
            "prefix": self.prefix + "store/", "objects": objects,
            "destination": {"bucket": self.config["final_store_bucket"], "region": self.config["final_store_region"],
                "prefix": "factory/" + self.input_sha + "/store/"}}
        self.guest(slot, "seal", "host")
        image = self.seal(slot)
        return renderer, image

    def initialize_tls(self):
        """Two isolated per-build canaries, generated without a private-key file."""
        fingerprints = {}
        for variant, arn in (("build", self.config["tls_build_arn"]), ("test", self.config["tls_test_arn"])):
            pem = subprocess.check_output(["openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt",
                "ec_paramgen_curve:P-256", "-nodes", "-days", "2", "-subj", "/CN=factory.invalid",
                "-keyout", "/dev/stdout", "-out", "/dev/stdout"], stderr=subprocess.DEVNULL, timeout=30).decode()
            certs = re.findall(r"-----BEGIN CERTIFICATE-----.*?-----END CERTIFICATE-----", pem, re.S)
            keys = re.findall(r"-----BEGIN PRIVATE KEY-----.*?-----END PRIVATE KEY-----", pem, re.S)
            require(len(certs) == len(keys) == 1, "cannot create isolated pipeline TLS identity")
            document = {"cert": certs[0] + "\n", "key": keys[0] + "\n", "scene_token": secrets.token_urlsafe(32)}
            token = hashlib.sha256((self.build_id + "\0tls-" + variant).encode()).hexdigest()
            response = self.worker.client("secretsmanager", self.config["secret_region"]).put_secret_value(
                SecretId=arn, ClientRequestToken=token, SecretString=canonical(document).decode())
            require(response.get("VersionId") == token, "pipeline TLS write did not return its immutable version")
            fingerprints[variant + "_cert_sha256"] = hashlib.sha256(ssl.PEM_cert_to_DER_cert(document["cert"])).hexdigest()
        require(fingerprints["build_cert_sha256"] != fingerprints["test_cert_sha256"], "pipeline TLS canaries are identical")
        self.tls = fingerprints

    def stage(self, slot, name, receipt):
        require(re.fullmatch(r"[0-9a-f]{64}", receipt.get("sha256", "")), "artifact digest is missing")
        require(isinstance(receipt.get("version_id"), str) and receipt["version_id"] not in {"", "null"},
                "artifact does not have an immutable version")
        require(isinstance(receipt.get("size"), int) and 0 < receipt["size"] < 5 * 1024**3,
                "artifact has an invalid size")
        source = {"Bucket": self.config["bucket"], "Key": self.builder_prefixes[slot] + name,
                  "VersionId": receipt["version_id"]}
        result = self.worker.client("s3", self.config["asset_region"]).copy_object(
            Bucket=self.config["bucket"], Key=self.prefix + name, CopySource=source,
            MetadataDirective="COPY", ChecksumAlgorithm="SHA256")
        require(result.get("CopySourceVersionId") == receipt["version_id"], "staging did not copy the pinned artifact version")
        checksum = base64.b64encode(bytes.fromhex(receipt["sha256"])).decode()
        require(result.get("CopyObjectResult", {}).get("ChecksumSHA256") == checksum,
                "staged artifact server checksum differs")
        version = result.get("VersionId")
        require(isinstance(version, str) and version not in {"", "null"}, "staged artifact has no immutable version")
        return {"sha256": receipt["sha256"], "size": receipt["size"], "version_id": version}

    def seal(self, slot):
        self.call("stop", slot)
        self.wait(lambda: self.call("status", slot).get("state") == "stopped")
        image = self.call("seal", slot)
        require(re.fullmatch(r"ami-[0-9a-f]{17}", image.get("image_id", "")), "broker returned an invalid image")
        self.wait(lambda: self.call("status", slot).get("image_state") == "available")
        return image

    def build_tile(self, renderer):
        slot = "tile-build"
        self.launch(slot)
        self.security_update(slot)
        # A sanitized previous tile already runs its GPU services. The NVIDIA
        # installer cannot replace an in-use module; stop only this disposable
        # builder's consumers, without changing their enabled boot configuration.
        self.shell(slot, "for unit in fcvm-fleet-renderer.service fcvm-fleet-nginx.service nvidia-persistenced.service; do\n"
            "  if [ \"$(systemctl show \"$unit\" --property=LoadState --value 2>/dev/null)\" = loaded ]; then\n"
            "    systemctl stop \"$unit\"\n  fi\ndone")
        self.phase(slot, "tile", "prep")
        # The tracing boot must read on its own; trace replaces the old profile.
        self.shell(slot, "rm -f /etc/fcvm-fleet/prefetch.list")
        self.reboot(slot)
        url = self.worker.client("s3", self.config["asset_region"]).generate_presigned_url("get_object",
            Params={"Bucket": self.config["bucket"], "Key": self.prefix + "renderer.oci.tar.zst",
                    "VersionId": renderer["staged"]["version_id"]},
            ExpiresIn=min(3600, self.remaining()))
        self.shell(slot, "printf '%s\\n' " + shlex.quote("export RENDERER_URL=" + shlex.quote(url))
            + " >> /var/lib/fleet-ami/env")
        self.guest(slot, "tile-tls", self.config["tls_build_arn"], self.config["secret_region"])
        self.phase(slot, "tile", "image")
        self.guest(slot, "tile-ready")
        actual = self.guest(slot, "renderer")
        require(actual["image_id"] == renderer["image_id"], "host and tile render different images")
        self.phase(slot, "tile", "trace")
        self.phase(slot, "tile", "bake")
        self.guest(slot, "seal", "tile")
        return self.seal(slot)

    def cleanup(self):
        self.call("cleanup")
        def gone():
            values = self.call("status").get("slots", {})
            require(set(values) <= set(SLOTS), "broker returned unknown slots")
            require(set(self.instances) <= set(values), "broker lost a paid slot during cleanup")
            require(all(values[slot].get("instance_id") == instance for slot, instance in self.instances.items()),
                    "broker slot ownership changed during cleanup")
            return all(value.get("state") == "terminated" for value in values.values())
        self.wait(gone, cleanup=True)

    def verify(self, kind, renderer):
        slot = kind + "-verify"
        self.launch(slot)
        if kind == "tile":
            self.guest(slot, "tile-tls", self.config["tls_test_arn"], self.config["secret_region"])
            self.guest(slot, "tile-ready")
        boots = []
        for transition in (None, "reboot", "resume"):
            if transition:
                self.reboot(slot, stop_start=transition == "resume")
            result = self.guest(slot, "verify", kind, renderer["image_id"],
                self.config["tls_build_arn"], self.config["tls_test_arn"], self.config["secret_region"])
            require(result.get("guard") is True and result.get("frames") is True
                    and result.get("sanitized") is True and result.get("tls_rotation") is True,
                    "candidate failed the boot, renderer or security proof")
            require(all(result.get(key) == value for key, value in self.tls.items()),
                    "the validation TLS identity changed during this build")
            require(kind != "host" or result.get("phone_clone") is True, "candidate phone clone failed")
            require(re.fullmatch(r"[0-9a-f-]{36}", result.get("boot_id", "")), "candidate has no boot identity")
            boots.append(result["boot_id"])
        require(len(set(boots)) == 3, "cold boot, reboot and stop/start did not produce three boots")
        result["boot_ids"] = boots
        del result["boot_id"]
        return result

    def await_ready(self):
        initial = self.call("status")
        self.deadline = initial["deadline_unix"]
        require(self.clock() < self.deadline <= self.clock() + 9000,
                "broker deadline is not bounded by the CodeBuild lease")
        require(initial.get("build_id") == self.build_id, "broker build identity differs")
        require(initial.get("source_commit") == self.config["source_commit"], "broker source revision differs")
        self.input_sha = initial.get("input_sha256", "")
        require(re.fullmatch(r"[0-9a-f]{64}", self.input_sha), "broker input fingerprint is missing")
        identity = {name: initial.get(name) for name in ("build_id", "source_commit", "input_sha256", "deadline_unix")}
        current = initial
        while True:
            require(all(current.get(name) == value for name, value in identity.items()),
                    "broker build identity changed while waiting for readiness")
            require(not current.get("slots"), "this executor cannot replay an existing build")
            require(type(current.get("ready")) is bool, "broker readiness is missing")
            if current["ready"]:
                return
            # StartBuild can run before the dispatcher reacquires the backend
            # lock and commits its active-build fence. Status is the only
            # operation permitted here; credentials, uploads and launches wait.
            self.sleep(min(1, self.remaining()))
            self.remaining()
            current = self.call("status")

    def run(self):
        # Cleanup also runs for ordinary exceptions, SSM ambiguity and failed validation.
        started = False
        try:
            self.await_ready()
            started = True
            self.source_tree_sha, self.archive = input_digest(self.root, self.config["source_commit"])
            self.worker.client("s3", self.config["asset_region"]).put_object(Bucket=self.config["bucket"], Key=self.prefix + "source.tar",
                Body=self.archive, ContentType="application/x-tar", IfNoneMatch="*")
            self.initialize_tls()
            renderer, host = self.build_host()
            tile = self.build_tile(renderer)
            require(host.get("image_id") != tile.get("image_id"), "host and tile images are not distinct")
            self.cleanup()
            def validate(kind):
                try:
                    return self.verify(kind, renderer)
                except BaseException:
                    self.cancelled.set()
                    raise
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
                results = {executor.submit(validate, kind): kind for kind in ("host", "tile")}
                try:
                    validation = {results[future]: future.result() for future in concurrent.futures.as_completed(results)}
                except BaseException:
                    self.cancelled.set()
                    self.cleanup()
                    raise
            status = self.call("status")
            require(status.get("candidates") == {"host": host["image_id"], "tile": tile["image_id"]},
                    "broker candidate pair changed")
            guard = hashlib.sha256((self.root / "fleet/ami/common/cve-2026-98432-mitigation.py").read_bytes()).hexdigest()
            manifest = {"version": 1, "build_id": self.build_id,
                "source_commit": self.config["source_commit"], "input_sha256": self.input_sha,
                "source_tree_sha256": self.source_tree_sha,
                "guard_sha256": guard, "deadline_unix": self.deadline, "store": self.store, "tls": self.tls,
                "images": {kind: {"image_id": image["image_id"], "renderer_sha256": renderer["sha256"],
                    "renderer_image_id": renderer["image_id"], "validation": validation[kind]}
                    for kind, image in (("host", host), ("tile", tile))}}
            # No result exists until cleanup succeeds. A failed run cannot be promoted.
            self.cleanup()
            self.remaining()
            self.worker.client("s3", self.config["asset_region"]).put_object(Bucket=self.config["bucket"],
                Key=self.prefix + "manifest.json", Body=canonical(manifest), ContentType="application/json", IfNoneMatch="*")
            return manifest
        finally:
            if started:
                # This also handles the deadline: the broker and its independent reaper
                # enforce termination if CodeBuild or this process disappears.
                self.cleanup()


def main():
    import boto3
    from botocore.config import Config
    names = {"broker_arn": "FLEET_AMI_BROKER_ARN",
        "bucket": "FLEET_AMI_BUCKET", "region": "FLEET_AMI_REGION", "asset_region": "FLEET_AMI_ASSET_REGION",
        "secret_region": "FLEET_AMI_SECRET_REGION",
        "tls_build_arn": "FLEET_AMI_TLS_BUILD_ARN", "tls_test_arn": "FLEET_AMI_TLS_TEST_ARN",
        "final_store_bucket": "FLEET_AMI_PUBLISHED_ASSET_BUCKET", "final_store_region": "FLEET_AMI_PUBLISHED_ASSET_REGION",
        "build_id": "CODEBUILD_BUILD_ID", "source_commit": "CODEBUILD_RESOLVED_SOURCE_VERSION"}
    names["capability"] = "FLEET_AMI_CAPABILITY"
    config = {key: os.environ[name] for key, name in names.items()}
    config["document"] = "fcvm-fleet-ami-build"
    require(config["tls_build_arn"] != config["tls_test_arn"], "build and validation TLS identities must differ")
    base = boto3.Session()
    broker = base.client("lambda", config=Config(retries={"total_max_attempts": 1}, connect_timeout=10, read_timeout=60))
    driver = Pipeline(broker, None, config)
    driver.worker = Worker(driver.call, config["region"], config["build_id"])
    driver.run()
    print("Fleet image pair built, independently boot-validated and cleaned up; awaiting controlled promotion.")


if __name__ == "__main__":
    main()
