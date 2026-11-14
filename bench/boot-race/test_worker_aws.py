"""Credential source and renewal boundaries for the benchmark worker."""

import base64
import json
from pathlib import Path
import subprocess
import unittest
from unittest.mock import Mock, call, patch

import prepare
import worker_aws


class WorkerMetadataTests(unittest.TestCase):
    path = "meta-data/iam/security-credentials/"

    def test_worker_uses_imdsv2_and_renews_source_credentials(self):
        issued = [
            {"Code": "Success", "AccessKeyId": key, "SecretAccessKey": "fake", "Token": "fake",
             "Expiration": "2026-11-30T19:00:00Z"}
            for key in ("first", "second")
        ]
        responses = ["token-a", "benchmark-worker", json.dumps(issued[0]),
                     "token-b", "benchmark-worker", json.dumps(issued[1])]
        with patch.object(worker_aws, "metadata_request", side_effect=responses) as request:
            self.assertEqual(worker_aws.worker_credentials()["access_key"], "first")
            self.assertEqual(worker_aws.worker_credentials()["access_key"], "second")
        self.assertEqual(request.call_args_list, [
            item for token in ("token-a", "token-b") for item in (
                call("PUT", "api/token", {"X-aws-ec2-metadata-token-ttl-seconds": "60"}),
                call("GET", self.path, {"X-aws-ec2-metadata-token": token}),
                call("GET", self.path + "benchmark-worker", {"X-aws-ec2-metadata-token": token}),
            )
        ])

    def test_unrelated_profile_is_refused_before_reading_its_credentials(self):
        with patch.object(worker_aws, "metadata_request", side_effect=["token", "dev-server-role"]) as request:
            with self.assertRaisesRegex(RuntimeError, "benchmark-worker instance profile"):
                worker_aws.worker_credentials()
        self.assertEqual(request.call_count, 2)

    def test_direct_metadata_http_does_not_fallback_when_token_is_refused(self):
        with patch.object(worker_aws.http.client, "HTTPConnection") as connect:
            connection = connect.return_value
            connection.getresponse.return_value.status = 403
            with self.assertRaisesRegex(RuntimeError, "IMDSv2 returned HTTP 403"):
                worker_aws.worker_credentials()
        connect.assert_called_once_with("169.254.169.254", timeout=2)
        connection.request.assert_called_once_with(
            "PUT", "/latest/api/token", headers={"X-aws-ec2-metadata-token-ttl-seconds": "60"})
        connection.close.assert_called_once()

    def test_identity_must_be_in_the_worker_region(self):
        read = Mock(return_value=json.dumps({"accountId": "123456789012", "region": "us-west-1"}))
        with patch.object(worker_aws, "metadata_reader", return_value=read):
            with self.assertRaisesRegex(RuntimeError, "dedicated benchmark worker in us-east-2"):
                worker_aws.worker_identity()


class BuilderKeyTests(unittest.TestCase):
    def test_builder_uses_its_one_temporary_identity_and_removes_it_on_failure(self):
        seen = []

        def build(cfg, ec2, key, pub):
            identity = Path(key)
            seen.append(identity)
            self.assertEqual(identity.parent.parent, Path("/dev/shm"))
            self.assertEqual(identity.stat().st_mode & 0o777, 0o600)
            self.assertTrue(pub.startswith("ssh-ed25519 "))
            self.assertNotIn(str(identity), pub)
            with patch.object(prepare.subprocess, "run") as run:
                prepare.ssh(key, "192.0.2.100", "true")
                command = run.call_args.args[0]
                self.assertEqual(command[command.index("-i") + 1], key)
                self.assertEqual(command[command.index("-F") + 1], "/dev/null")
                self.assertIn("IdentitiesOnly=yes", command)
                self.assertIn("IdentityAgent=none", command)
            raise RuntimeError("failed benchmark AMI build")

        with patch.object(prepare, "build_ami", side_effect=build):
            with self.assertRaisesRegex(RuntimeError, "failed benchmark AMI build"):
                prepare.ami({}, Mock())
        self.assertFalse(seen[0].parent.exists())


class RegistryAuthTests(unittest.TestCase):
    def test_push_failure_removes_the_private_temporary_registry_token(self):
        sdk = Mock()
        sdk.client.return_value.get_authorization_token.return_value = {"authorizationData": [{
            "authorizationToken": base64.b64encode(b"AWS:temporary-token").decode(),
            "proxyEndpoint": "https://registry.example.com",
        }]}
        auth_paths = []

        def podman(args, **kwargs):
            if args[1] == "login":
                auth = Path(args[args.index("--authfile") + 1])
                auth_paths.append(auth)
                self.assertEqual(auth.parent.parent, Path("/dev/shm"))
                self.assertEqual(auth.parent.stat().st_mode & 0o777, 0o700)
                self.assertEqual(kwargs["input"], b"temporary-token")
                auth.write_text("fake registry auth")
            elif args[1] == "push":
                self.assertEqual(Path(args[args.index("--authfile") + 1]), auth_paths[0])
                raise subprocess.CalledProcessError(1, args)

        with patch.object(prepare.subprocess, "run", side_effect=podman):
            with self.assertRaises(subprocess.CalledProcessError):
                prepare.image({}, sdk)
        self.assertFalse(auth_paths[0].parent.exists())


if __name__ == "__main__":
    unittest.main()
