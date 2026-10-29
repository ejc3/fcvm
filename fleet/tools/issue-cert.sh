#!/bin/bash
# Issue the fleet's wildcard certificate (*.fcvm.dev and fcvm.dev) from Let's Encrypt by
# DNS-01 through Cloudflare, and store it in Secrets Manager as fcvm-fleet/tls-wildcard.
# This is the first issuance; renewal belongs to the coordinator (docs/fleet/ARCHITECTURE.md,
# "Slots and addressing"), which reads and writes the same secret in the same format.
#
#   AWS credentials for the fcvm-fleet account in the environment, then:
#   fleet/tools/issue-cert.sh [--staging]
#
# Needs lego v4 on PATH (`go install github.com/go-acme/lego/v4/cmd/lego@v4.17.4`), the aws CLI
# and python3. The Cloudflare token (fcvm-fleet/cloudflare-dns-token, DNS Write on the zone
# only) goes into lego's environment and nowhere else; the ACME account key and the issued
# key live in a private temporary directory removed on exit.
#
# Secret format (one JSON object, read by the renderer's TLS front and the coordinator):
#   cert       PEM, the leaf certificate followed by its intermediates (a full chain)
#   key        PEM, the leaf's private key (ECDSA P-256, SEC 1 "EC PRIVATE KEY")
#   domains    the names the certificate covers
#   not_after  expiry, RFC 3339 UTC
#   issued_at  RFC 3339 UTC
#   issuer     the issuing CA's subject
#   scene_token  the renderers' scene token (fleet/PROTOCOL.md, "Access"): kept from the
#              secret's current version; the coordinator adds one when there is none
set -euo pipefail
ZONE=${ZONE:-fcvm.dev}
SECRET=${SECRET:-fcvm-fleet/tls-wildcard}
TOKEN_SECRET=${TOKEN_SECRET:-fcvm-fleet/cloudflare-dns-token}
SECRET_REGION=${SECRET_REGION:-us-east-2}
# lego v4.17 requires a contact address; Let's Encrypt no longer mails expiry notices, so
# an address in the zone is enough.
ACME_EMAIL=${ACME_EMAIL:-acme@$ZONE}
SERVER=https://acme-v02.api.letsencrypt.org/directory
[ "${1:-}" = --staging ] && SERVER=https://acme-staging-v02.api.letsencrypt.org/directory

W=$(mktemp -d); chmod 700 "$W"; trap 'rm -rf "$W"' EXIT
CLOUDFLARE_DNS_API_TOKEN=$(aws --region "$SECRET_REGION" secretsmanager get-secret-value --secret-id "$TOKEN_SECRET" --query SecretString --output text)
export CLOUDFLARE_DNS_API_TOKEN
CLOUDFLARE_PROPAGATION_TIMEOUT=300 lego --accept-tos --email "$ACME_EMAIL" --server "$SERVER" --path "$W" \
  --dns cloudflare --dns.resolvers 1.1.1.1:53 --key-type ec256 \
  --domains "*.$ZONE" --domains "$ZONE" run
unset CLOUDFLARE_DNS_API_TOKEN

crt="$W/certificates/_.$ZONE.crt"; key="$W/certificates/_.$ZONE.key"
# The current version, for its scene token. Only a secret that does not exist yet (a first
# issuance) has none: any other failure stops here, since storing a certificate without the
# token would leave every renderer refusing the coordinator's scenes.
if ! ( umask 077; aws --region "$SECRET_REGION" secretsmanager get-secret-value --secret-id "$SECRET" \
    --query SecretString --output text > "$W/current.json" 2> "$W/current.err" ); then
  grep -q ResourceNotFoundException "$W/current.err" || { cat "$W/current.err" >&2; echo "cannot read $SECRET; not storing a certificate without its scene token" >&2; exit 1; }
  : > "$W/current.json"
fi
python3 - "$crt" "$key" "$W/secret.json" "$W/current.json" <<'PY'
import json, subprocess, sys, datetime
crt, key, out, current = sys.argv[1:5]
def x509(*args):
    return subprocess.run(["openssl", "x509", "-noout", *args], input=open(crt, "rb").read(), capture_output=True, check=True).stdout.decode().strip()
end = x509("-enddate").split("=", 1)[1]
not_after = datetime.datetime.strptime(end, "%b %d %H:%M:%S %Y %Z").replace(tzinfo=datetime.timezone.utc)
sans = x509("-ext", "subjectAltName").splitlines()[1]
doc = {
    "cert": open(crt).read(),
    "key": open(key).read(),
    "domains": [s.strip()[4:] for s in sans.split(",")],
    "not_after": not_after.strftime("%Y-%m-%dT%H:%M:%SZ"),
    "issued_at": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "issuer": x509("-issuer").split("=", 1)[1].strip(),
}
try:
    token = json.load(open(current)).get("scene_token")
except (ValueError, AttributeError):
    token = None
if token:
    doc["scene_token"] = token
fd = open(out, "w"); json.dump(doc, fd); fd.close()
print("domains", doc["domains"], "not_after", doc["not_after"], "issuer", doc["issuer"])
PY
chmod 600 "$W/secret.json"
if [ "$SERVER" = https://acme-v02.api.letsencrypt.org/directory ]; then
  aws --region "$SECRET_REGION" secretsmanager put-secret-value --secret-id "$SECRET" \
    --secret-string "file://$W/secret.json" --query VersionId --output text
else
  echo "staging certificate issued; not stored"
fi
