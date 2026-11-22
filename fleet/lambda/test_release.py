#!/usr/bin/env python3
"""Offline checks for the lane image's protected tags (release.py) against a model of the
repository's tag mutability and lifecycle policy."""
import copy
import fcntl
import fnmatch
import hashlib
import importlib.util
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("lane_release", HERE / "release.py")
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class TagExists(Exception):
    pass


class FakeEcr:
    """The repository as ECR keeps it: images by digest with their tags and push order. A tag matching
    a mutability exclusion moves when put on another image; any other tag is immutable."""

    def __init__(self, mutable=(release.PROTECTED,)):
        self.mutable = mutable
        self.images = {}  # digest -> {"tags": set, "pushed": int, "manifest": str}
        self.clock = 0

    def push(self, tag):
        manifest = json.dumps({"schemaVersion": 2, "config": {"digest": tag}})
        digest = "sha256:" + hashlib.sha256(manifest.encode()).hexdigest()
        self.clock += 1
        self.images[digest] = {"tags": set(), "pushed": self.clock, "manifest": manifest}
        self.put(tag, release.Image(digest, manifest, release.MEDIA_TYPES[1]))
        return digest

    def tagged(self, tag):
        for digest, image in self.images.items():
            if tag in image["tags"]:
                return release.Image(digest, image["manifest"], release.MEDIA_TYPES[1])
        return None

    def image(self, digest):
        if digest not in self.images:
            raise LookupError(digest)
        return release.Image(digest, self.images[digest]["manifest"], release.MEDIA_TYPES[1])

    def put(self, tag, image):
        assert image.digest == "sha256:" + hashlib.sha256(image.manifest.encode()).hexdigest()
        holder = self.tagged(tag)
        if holder is not None and holder.digest != image.digest:
            if not any(fnmatch.fnmatchcase(tag, m) for m in self.mutable):
                raise TagExists(tag)
            self.images[holder.digest]["tags"].discard(tag)
        self.images[image.digest]["tags"].add(tag)

    def tags_of(self, digest):
        return self.images[digest]["tags"]

    def lifecycle(self, policy=release.LIFECYCLE):
        """Apply a lifecycle policy as ECR documents it, and return the digests it expired.

        Rules run in priority order; a rule selects images by its tag requirements and orders them
        newest first by push. An image a higher-priority rule's tag requirements select is never
        expired by a lower one, but lower rules still count it."""
        def selects(rule, image):
            s = rule["selection"]
            if s["tagStatus"] == "any":
                return True
            if s["tagStatus"] == "untagged":
                return not image["tags"]
            return bool(image["tags"]) and all(
                any(fnmatch.fnmatchcase(t, p) for t in image["tags"]) for p in s["tagPatternList"])

        expired, claimed = set(), set()
        for rule in sorted(policy["rules"], key=lambda r: r["rulePriority"]):
            s = rule["selection"]
            assert s["countType"] == "imageCountMoreThan"
            chosen = sorted((d for d, i in self.images.items() if selects(rule, i)),
                            key=lambda d: self.images[d]["pushed"], reverse=True)
            expired.update(d for d in chosen[s["countNumber"]:] if d not in claimed)
            claimed.update(chosen)
        for d in expired:
            del self.images[d]
        return expired


class Selection(unittest.TestCase):
    def setUp(self):
        self.ecr = FakeEcr()
        # Each test's locks in a directory of its own.
        self.tmp = tempfile.TemporaryDirectory()
        patcher = patch.object(release.tempfile, "gettempdir", return_value=self.tmp.name)
        patcher.start()
        self.addCleanup(patcher.stop)
        self.addCleanup(self.tmp.cleanup)

    def pushes(self, n, start=0):
        return [self.ecr.push(f"rev-{start + i}") for i in range(n)]

    def test_a_selected_digest_outlives_more_than_five_later_pushes(self):
        a = self.ecr.push("rev-a")
        release.select(self.ecr, "prod", a, None)
        later = self.pushes(8)
        expired = self.ecr.lifecycle()
        self.assertIn(a, self.ecr.images)
        # The newest five are kept whatever they are; the others no protected tag holds go.
        self.assertEqual(expired, set(later[:3]))
        self.assertTrue(set(later[3:]) <= set(self.ecr.images))

    def test_the_model_expires_it_under_the_policy_this_replaces(self):
        # Without the protected tags' rule, the same history loses prod's image: the model is not vacuous.
        a = self.ecr.push("rev-a")
        release.select(self.ecr, "prod", a, None)
        self.pushes(8)
        only_newest_five = {"rules": [dict(release.LIFECYCLE["rules"][1], rulePriority=1)]}
        self.assertIn(a, self.ecr.lifecycle(only_newest_five))

    def test_both_stacks_keep_what_they_run_and_can_roll_back_to(self):
        prod_old, prod_now = self.pushes(2)
        release.select(self.ecr, "prod", prod_old, None)
        release.select(self.ecr, "prod", prod_now, prod_old)
        # prod now runs prod_now; staging pushes and selects many times after it.
        running, staged = None, []
        for i in range(12):
            new = self.ecr.push(f"rev-stage-{i}")
            release.select(self.ecr, "stage", new, running)
            running = new
            staged.append(new)
            self.ecr.lifecycle()
        self.assertIn(prod_old, self.ecr.images)
        self.assertIn(prod_now, self.ecr.images)
        self.assertEqual(self.ecr.tags_of(prod_now) & {"keep-prod-current"}, {"keep-prod-current"})
        self.assertIn("keep-prod-previous", self.ecr.tags_of(prod_old))
        # Staging's last two (running and its rollback) are held; its older selections went.
        self.assertEqual(release.tags(self.ecr, "stage"),
                         {"keep-stage-current": staged[-1], "keep-stage-previous": staged[-2]})
        self.assertFalse(set(staged[:-5]) & set(self.ecr.images))

    def test_an_image_no_stack_runs_or_rolls_back_to_stays_eligible(self):
        a, b, c = self.pushes(3)
        release.select(self.ecr, "stage", a, None)
        release.select(self.ecr, "stage", b, a)
        release.select(self.ecr, "stage", c, b)
        # a is neither running nor the rollback target any more.
        self.assertFalse(any(t.startswith("keep-") for t in self.ecr.tags_of(a)))
        self.pushes(5, start=10)
        self.assertEqual(self.ecr.lifecycle(), {a})
        self.assertIn(b, self.ecr.images)
        self.assertIn(c, self.ecr.images)

    def test_the_running_image_is_protected_while_a_selection_waits_to_be_deployed(self):
        a, b, c = self.pushes(3)
        release.select(self.ecr, "stage", a, None)
        # Selected b, but the coordinator still runs a; the next selection names a as running.
        release.select(self.ecr, "stage", b, a)
        release.select(self.ecr, "stage", c, a)
        self.assertEqual(release.tags(self.ecr, "stage"),
                         {"keep-stage-current": c, "keep-stage-previous": a})
        self.pushes(8, start=10)
        self.ecr.lifecycle()
        self.assertIn(a, self.ecr.images)
        self.assertNotIn(b, self.ecr.images)

    def test_every_step_of_a_selection_keeps_the_running_image_protected(self):
        a, b = self.pushes(2)
        release.select(self.ecr, "stage", a, None)
        seen = []
        real_put = self.ecr.put

        def put(tag, image):
            real_put(tag, image)
            seen.append(any(t.startswith("keep-stage-") for t in self.ecr.tags_of(a)))
        self.ecr.put = put
        release.select(self.ecr, "stage", b, a)
        self.assertEqual(seen, [True, True])

    def test_promote_takes_stagings_selection_and_rollback_swaps(self):
        a, b = self.pushes(2)
        release.select(self.ecr, "stage", a, None)
        release.promote(self.ecr, None)
        release.select(self.ecr, "stage", b, a)
        release.promote(self.ecr, a)
        self.assertEqual(release.tags(self.ecr, "prod"), {"keep-prod-current": b, "keep-prod-previous": a})
        release.rollback(self.ecr, "prod", b)
        self.assertEqual(release.tags(self.ecr, "prod"), {"keep-prod-current": a, "keep-prod-previous": b})
        self.pushes(10, start=10)
        self.ecr.lifecycle()
        self.assertIn(a, self.ecr.images)
        self.assertIn(b, self.ecr.images)

    def test_rollback_refuses_without_a_deployed_selection_or_a_target(self):
        a, b = self.pushes(2)
        with self.assertRaises(RuntimeError):
            release.rollback(self.ecr, "stage", a)
        release.select(self.ecr, "stage", a, None)
        with self.assertRaises(LookupError):
            release.rollback(self.ecr, "stage", a)
        release.select(self.ecr, "stage", b, a)
        # b is selected but the coordinator still runs a: nothing to roll back from.
        with self.assertRaises(RuntimeError):
            release.rollback(self.ecr, "stage", a)

    def test_a_selection_reads_its_tags_back(self):
        a, b, c = self.pushes(3)
        real_put = self.ecr.put

        def racing_put(tag, image):
            real_put(tag, image)
            if tag == "keep-stage-current":
                real_put(tag, self.ecr.image(c))  # another host's selection lands in between
        self.ecr.put = racing_put
        with self.assertRaisesRegex(RuntimeError, "another selection"):
            release.select(self.ecr, "stage", b, a)

    def test_a_held_lock_refuses_a_second_selection(self):
        a = self.ecr.push("rev-a")
        with open(os.path.join(self.tmp.name, "fcvm-lambda-tile-stage.lock"), "a") as f:
            fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with self.assertRaisesRegex(RuntimeError, "another selection for stage"):
                release.select(self.ecr, "stage", a, None)
            # promote holds staging's lock too: staging's selection cannot move under it.
            with self.assertRaisesRegex(RuntimeError, "another selection for stage"):
                release.promote(self.ecr, None)
        self.assertEqual(release.tags(self.ecr, "stage")["keep-stage-current"], None)

    def test_only_protected_tags_move(self):
        a, b = self.pushes(2)
        with self.assertRaises(TagExists):
            self.ecr.put("rev-0", self.ecr.image(b))
        self.ecr.put("keep-stage-current", self.ecr.image(a))
        self.ecr.put("keep-stage-current", self.ecr.image(b))
        self.assertEqual(self.ecr.tagged("keep-stage-current").digest, b)

    def test_arguments_are_digests(self):
        for bad in ["latest", "sha256:" + "A" * 64, "sha256:" + "a" * 63, ""]:
            with self.assertRaises(ValueError):
                release.digest(bad)
        self.assertIsNone(release.digest("none", none=True))
        with self.assertRaises(ValueError):
            release.digest("none")
        with self.assertRaises(SystemExit):
            release.arguments(["select", "--stack", "other", "--digest", "x", "--running", "none"])


class AwsCli(unittest.TestCase):
    """The commands Ecr runs, against recorded responses."""

    def run_with(self, responses):
        calls = []

        def fake(args, input=None, capture_output=None, text=None):
            calls.append((args, input))
            code, out, err = responses.pop(0)
            return type("R", (), {"returncode": code, "stdout": out, "stderr": err})()
        return calls, patch.object(release.subprocess, "run", side_effect=fake)

    def test_put_sends_the_pushed_manifest_and_its_digest(self):
        image = release.Image("sha256:" + "1" * 64, '{"schemaVersion":2}', release.MEDIA_TYPES[1])
        ok = json.dumps({"image": {"imageId": {"imageDigest": image.digest, "imageTag": "keep-stage-current"}}})
        calls, p = self.run_with([(0, ok, ""),
                                  (254, "", "An error occurred (ImageAlreadyExistsException) when calling")])
        with p:
            release.Ecr().put("keep-stage-current", image)
            release.Ecr().put("keep-stage-current", image)  # already there: nothing to do
        args, body = calls[0]
        self.assertEqual(args[:3], ["aws", "ecr", "put-image"])
        self.assertIn("us-east-1", args)
        self.assertEqual(json.loads(body), {
            "repositoryName": "fcvm-fleet/lambda-tile", "imageTag": "keep-stage-current",
            "imageDigest": image.digest, "imageManifest": image.manifest,
            "imageManifestMediaType": image.media_type})

    def test_put_refuses_another_digest_and_other_errors(self):
        image = release.Image("sha256:" + "1" * 64, "{}", release.MEDIA_TYPES[1])
        other = json.dumps({"image": {"imageId": {"imageDigest": "sha256:" + "2" * 64}}})
        _, p = self.run_with([(0, other, ""), (254, "", "An error occurred (AccessDeniedException) when")])
        with p:
            with self.assertRaisesRegex(RuntimeError, "another digest"):
                release.Ecr().put("keep-stage-current", image)
            with self.assertRaises(release.CommandFailed):
                release.Ecr().put("keep-stage-current", image)

    def test_a_missing_tag_reads_as_none(self):
        missing = json.dumps({"images": [], "failures": [{"failureCode": "ImageTagDoesNotExist"}]})
        found = json.dumps({"images": [{"imageId": {"imageDigest": "sha256:" + "3" * 64},
                                        "imageManifest": "{}", "imageManifestMediaType": release.MEDIA_TYPES[0]}]})
        calls, p = self.run_with([(0, missing, ""), (0, found, "")])
        with p:
            self.assertIsNone(release.Ecr().tagged("keep-prod-current"))
            self.assertEqual(release.Ecr().tagged("keep-prod-current").digest, "sha256:" + "3" * 64)
        self.assertIn("imageTag=keep-prod-current", calls[0][0])
        self.assertIn("--accepted-media-types", calls[0][0])


if __name__ == "__main__":
    unittest.main()
