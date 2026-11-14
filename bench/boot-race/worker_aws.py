"""Use only the dedicated EC2 benchmark worker's AWS role, entirely in memory."""

import http.client
import json
import re

REGION = "us-east-2"
WORKER_ROLE = "benchmark-worker"
DRIVER_ROLE = "boot-race-driver"
METADATA = "169.254.169.254"


def metadata_request(method, path, headers):
    # Direct link-local HTTP: proxy and AWS environment settings cannot redirect it.
    connection = http.client.HTTPConnection(METADATA, timeout=2)
    try:
        connection.request(method, "/latest/" + path, headers=headers)
        response = connection.getresponse()
        if response.status != 200:
            raise RuntimeError(f"benchmark worker IMDSv2 returned HTTP {response.status}")
        return response.read().decode()
    finally:
        connection.close()


def metadata_reader():
    token = metadata_request("PUT", "api/token", {"X-aws-ec2-metadata-token-ttl-seconds": "60"})

    def read(path):
        return metadata_request("GET", path, {"X-aws-ec2-metadata-token": token})

    return read


def worker_identity():
    read = metadata_reader()
    identity = json.loads(read("dynamic/instance-identity/document"))
    account = identity.get("accountId", "")
    if identity.get("region") != REGION or not re.fullmatch(r"[0-9]{12}", account):
        raise RuntimeError(f"boot-race needs its dedicated benchmark worker in {REGION}")
    return account


def worker_credentials():
    read = metadata_reader()
    path = "meta-data/iam/security-credentials/"
    if read(path).strip() != WORKER_ROLE:
        raise RuntimeError(f"boot-race needs the EC2 {WORKER_ROLE} instance profile")
    document = json.loads(read(path + WORKER_ROLE))
    if document.get("Code") != "Success":
        raise RuntimeError("the benchmark worker's instance credentials are unavailable")
    return {
        "access_key": document["AccessKeyId"],
        "secret_key": document["SecretAccessKey"],
        "token": document["Token"],
        "expiry_time": document["Expiration"],
    }


def make_session():
    import boto3
    from botocore.config import Config
    from botocore.credentials import AssumeRoleCredentialFetcher, CredentialResolver, DeferredRefreshableCredentials
    from botocore.session import Session

    account = worker_identity()
    botocore_session = Session()
    botocore_session.set_config_variable("region", REGION)
    botocore_session.set_config_variable("sts_regional_endpoints", "regional")
    botocore_session.set_default_client_config(Config(ignore_configured_endpoint_urls=True))
    # Pin the source; never trust ambient profiles, env credentials, SSO, or a shared credentials file.
    source = DeferredRefreshableCredentials(worker_credentials, method="benchmark-worker-imds")
    fetcher = AssumeRoleCredentialFetcher(
        client_creator=botocore_session.create_client,
        source_credentials=source,
        role_arn=f"arn:aws:iam::{account}:role/{DRIVER_ROLE}",
        extra_args={"RoleSessionName": "boot-race", "DurationSeconds": 3600},
    )
    driver = DeferredRefreshableCredentials(fetcher.fetch_credentials, method="boot-race-assume-role")

    class DriverProvider:
        METHOD = "boot-race-assume-role"

        def load(self):
            return driver

    botocore_session.register_component("credential_provider", CredentialResolver([DriverProvider()]))
    return boto3.Session(botocore_session=botocore_session, region_name=REGION)
