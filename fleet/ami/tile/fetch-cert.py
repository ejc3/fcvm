#!/usr/bin/python3
"""Write the fleet's wildcard certificate (Secrets Manager, fcvm-fleet/tls-wildcard: JSON with
`cert`, a full-chain PEM, and `key`) to /etc/fcvm-fleet/tls for nginx, using the instance role,
and the secret's `scene_token` to /etc/fcvm-fleet/scene/token for the renderer, which takes a
scene only with it (fleet/PROTOCOL.md, "Access"). The renderer gets that directory alone, not
the key's."""
import json, os, sys
import boto3

SECRET = os.environ.get("FLEET_TLS_SECRET", "fcvm-fleet/tls-wildcard")
REGION = os.environ.get("FLEET_SECRET_REGION", "us-east-2")
TLS_DIR = os.environ.get("FLEET_TLS_DIR", "/etc/fcvm-fleet/tls")
SCENE_DIR = os.environ.get("FLEET_SCENE_DIR", "/etc/fcvm-fleet/scene")


def write_private(d, name, text):
    os.makedirs(d, mode=0o700, exist_ok=True)
    tmp = os.path.join(d, name + ".tmp")
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as f:
        f.write(text)
    os.replace(tmp, os.path.join(d, name))


doc = json.loads(boto3.client("secretsmanager", region_name=REGION).get_secret_value(SecretId=SECRET)["SecretString"])
for name, field in (("fullchain.pem", "cert"), ("key.pem", "key")):
    write_private(TLS_DIR, name, doc[field])
if not doc.get("scene_token"):
    # Without it the renderer refuses every scene; fail so the unit retries.
    sys.exit("the certificate secret has no scene_token")
write_private(SCENE_DIR, "token", doc["scene_token"])
print("certificate for", doc.get("domains"), "until", doc.get("not_after"), file=sys.stderr)
