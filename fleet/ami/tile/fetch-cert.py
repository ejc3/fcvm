#!/usr/bin/python3
"""Write the fleet's wildcard certificate (Secrets Manager, fcvm-fleet/tls-wildcard: JSON with
`cert`, a full-chain PEM, and `key`) to /etc/fcvm-fleet/tls for nginx, using the instance role."""
import json, os, sys
import boto3

SECRET = os.environ.get("FLEET_TLS_SECRET", "fcvm-fleet/tls-wildcard")
REGION = os.environ.get("FLEET_SECRET_REGION", "us-east-2")
d = "/etc/fcvm-fleet/tls"
doc = json.loads(boto3.client("secretsmanager", region_name=REGION).get_secret_value(SecretId=SECRET)["SecretString"])
os.makedirs(d, mode=0o700, exist_ok=True)
for name, field in (("fullchain.pem", "cert"), ("key.pem", "key")):
    tmp = os.path.join(d, name + ".tmp")
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as f:
        f.write(doc[field])
    os.replace(tmp, os.path.join(d, name))
print("certificate for", doc.get("domains"), "until", doc.get("not_after"), file=sys.stderr)
