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
import re
import tomllib
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

    def push(self, tag, config=None):
        # The image's config digest is what podman calls its image ID: by default one per tag.
        config = config or "sha256:" + hashlib.sha256(tag.encode()).hexdigest()
        manifest = json.dumps({"schemaVersion": 2, "config": {"digest": config}, "tag": tag})
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

    def tags(self, digest):
        return sorted(self.images[digest]["tags"])

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

    def protected(self, digest):
        return any(t.startswith("keep-") for t in self.ecr.tags_of(digest))

    def interrupted(self, k, action):
        """Run action with the k-th tag write failing; True if it was interrupted."""
        real_put, writes = self.ecr.put, []

        def put(tag, image):
            writes.append(tag)
            if len(writes) == k:
                raise OSError("interrupted")
            real_put(tag, image)
        self.ecr.put = put
        try:
            action()
            return False
        except OSError:
            return True
        finally:
            self.ecr.put = real_put

    def test_an_interrupted_rollback_keeps_both_images_and_completes_on_retry(self):
        for k in range(1, 10):
            with self.subTest(failing_write=k):
                self.ecr = FakeEcr()
                a, b = self.pushes(2)
                release.select(self.ecr, "prod", a, None)
                release.select(self.ecr, "prod", b, a)
                if not self.interrupted(k, lambda: release.rollback(self.ecr, "prod", b)):
                    break
                self.assertTrue(self.protected(a), "the rollback target lost its last protected tag")
                self.assertTrue(self.protected(b), "the running image lost its last protected tag")
                release.rollback(self.ecr, "prod", b)
                self.assertEqual(release.tags(self.ecr, "prod"),
                                 {"keep-prod-current": a, "keep-prod-previous": b})
        else:
            self.fail("the rollback never completed")

    def test_an_interrupted_selection_keeps_the_running_image_and_completes_on_retry(self):
        for k in range(1, 10):
            with self.subTest(failing_write=k):
                self.ecr = FakeEcr()
                a, b = self.pushes(2)
                release.select(self.ecr, "stage", a, None)
                if not self.interrupted(k, lambda: release.select(self.ecr, "stage", b, a)):
                    break
                self.assertTrue(self.protected(a))
                release.select(self.ecr, "stage", b, a)
                self.assertEqual(release.tags(self.ecr, "stage"),
                                 {"keep-stage-current": b, "keep-stage-previous": a})
        else:
            self.fail("the selection never completed")

    def test_selecting_the_running_image_again_keeps_the_rollback_target(self):
        a, b = self.pushes(2)
        release.select(self.ecr, "stage", a, None)
        release.promote(self.ecr, None)
        release.select(self.ecr, "stage", b, a)
        release.promote(self.ecr, a)
        # A retried promotion, and a retried selection, of the image prod already runs.
        release.promote(self.ecr, b)
        release.select(self.ecr, "prod", b, b)
        self.assertEqual(release.tags(self.ecr, "prod"), {"keep-prod-current": b, "keep-prod-previous": a})
        release.rollback(self.ecr, "prod", b)
        self.assertEqual(release.tags(self.ecr, "prod")["keep-prod-current"], a)

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
        self.assertEqual(seen, [True, True, True])

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

    def test_a_build_two_commits_share_stays_recorded_against_an_older_one(self):
        # One image under two rev- tags (a later commit whose build is the same image): recording
        # a commit older than either of them leaves keep-built where it is.
        old, mid, new = "a" * 40, "b" * 40, "c" * 40
        order = [old, mid, new]
        is_ancestor = lambda a, b: order.index(a) <= order.index(b)
        shared = self.ecr.push("rev-" + mid)
        self.ecr.put("rev-" + new, self.ecr.image(shared))
        release.record(self.ecr, shared, new, is_ancestor)
        earlier = self.ecr.push("rev-" + old, config="sha256:" + "1" * 64)
        # old is behind both held commits.
        self.assertEqual(release.record(self.ecr, earlier, old, is_ancestor)["status"], "kept")
        # A commit after mid but before new is behind one held commit: kept as well.
        order.insert(2, "d" * 40)
        between = self.ecr.push("rev-" + "d" * 40, config="sha256:" + "2" * 64)
        self.assertEqual(release.record(self.ecr, between, "d" * 40, is_ancestor)["status"], "kept")
        self.assertEqual(self.ecr.tagged("keep-built").digest, shared)

    def test_a_recorded_commit_git_cannot_find_names_itself_and_the_remedy(self):
        held = self.ecr.push("rev-" + "a" * 40)
        release.record(self.ecr, held)
        new = self.ecr.push("rev-" + "b" * 40)

        def unknown(older, newer):
            raise release.UnknownCommit(older)
        with self.assertRaises(RuntimeError) as raised:
            release.record(self.ecr, new, "b" * 40, unknown)
        message = str(raised.exception)
        self.assertIn("a" * 40, message)
        self.assertIn("release.py record --digest " + new, message)
        self.assertEqual(self.ecr.tagged("keep-built").digest, held)

    def git(self, results):
        """subprocess.run as git answers it: (returncode, stderr) per call, in order."""
        calls, answers = [], iter(results)

        def run(args, **kwargs):
            calls.append(list(args))
            code, err = next(answers)
            return type("R", (), {"returncode": code, "stdout": "", "stderr": err})()
        return calls, patch.object(release.subprocess, "run", side_effect=run)

    def test_git_reports_a_commit_it_cannot_find(self):
        # merge-base cannot answer, git finds the checked-out commit and not the recorded one.
        older, newer = "a" * 40, "b" * 40
        calls, p = self.git([(0, ""), (1, ""), (128, "fatal: Not a valid commit name"), (0, ""), (128, "")])
        with p:
            self.assertTrue(release.git_is_ancestor(older, newer))
            self.assertFalse(release.git_is_ancestor(older, newer))
            with self.assertRaises(release.UnknownCommit) as raised:
                release.git_is_ancestor(older, newer)
        self.assertEqual(raised.exception.commit, older)
        self.assertEqual(calls[3:], [["git", "cat-file", "-e", newer + "^{commit}"],
                                     ["git", "cat-file", "-e", older + "^{commit}"]])

    def test_git_that_cannot_answer_at_all_is_not_an_unknown_commit(self):
        # Outside a checkout every git command exits 128: not a statement about the recorded commit.
        _, p = self.git([(128, "fatal: not a git repository"), (128, "fatal: not a git repository"),
                         (128, "fatal: not a git repository")])
        with p:
            with self.assertRaises(RuntimeError) as raised:
                release.git_is_ancestor("a" * 40, "b" * 40)
        self.assertNotIsInstance(raised.exception, release.UnknownCommit)
        self.assertIn("not a git repository", str(raised.exception))

    def test_a_git_failure_with_both_commits_present_is_not_called_unknown(self):
        _, p = self.git([(128, "fatal: bad object"), (0, ""), (0, "")])
        with p:
            with self.assertRaises(RuntimeError) as raised:
                release.git_is_ancestor("a" * 40, "b" * 40)
        self.assertNotIsInstance(raised.exception, release.UnknownCommit)
        self.assertIn("bad object", str(raised.exception))

    def test_a_known_newer_held_commit_decides_whatever_the_tag_order(self):
        # keep-built's image carries two rev- tags: one of a commit git cannot find, and one of a
        # commit newer than the build being recorded. The known one decides: kept, in either order.
        for unknown, known in (("1" * 40, "f" * 40), ("f" * 40, "1" * 40)):
            with self.subTest(first="unknown" if unknown < known else "known"):
                self.ecr = FakeEcr()
                held = self.ecr.push("rev-" + known)
                self.ecr.put("rev-" + unknown, self.ecr.image(held))
                release.record(self.ecr, held)
                new = self.ecr.push("rev-" + "5" * 40)

                def is_ancestor(older, newer):
                    if older == unknown:
                        raise release.UnknownCommit(older)
                    return False  # known is newer than the recorded commit
                self.assertEqual(release.record(self.ecr, new, "5" * 40, is_ancestor)["status"], "kept")
                self.assertEqual(self.ecr.tagged("keep-built").digest, held)

    def test_a_reset_puts_back_a_selection_and_keeps_what_runs_protected(self):
        p, r, s_ = self.pushes(3)
        release.select(self.ecr, "stage", p, None)
        release.select(self.ecr, "stage", r, p)  # stage runs r, can roll back to p
        before = release.snapshot(self.ecr, "stage")
        release.select(self.ecr, "stage", s_, r)  # the release of s then fails: stage still runs r
        seen, real_put = [], self.ecr.put

        def put(tag, image):
            real_put(tag, image)
            seen.append((self.protected(r), self.protected(p)))
        self.ecr.put = put
        release.reset(self.ecr, "stage", before)
        self.assertEqual(release.snapshot(self.ecr, "stage"), before)
        # What runs stays protected at every write; the old rollback target is protected again.
        self.assertTrue(seen and all(running for running, _ in seen), seen)
        self.assertTrue(self.protected(p))
        self.assertFalse(self.protected(s_))

    def test_a_build_pairs_with_a_commit_whose_lane_inputs_it_has(self):
        c0, c1, c2 = "0" * 40, "1" * 40, "2" * 40  # c0 <- c1 <- c2
        order = [c0, c1, c2]
        changed_between = {(c0, c1)}  # lane inputs changed in c1, not in c2

        def is_ancestor(a, b):
            return order.index(a) <= order.index(b)

        def changed(a, b):
            lo, hi = sorted((order.index(a), order.index(b)))
            return any((order[i], order[i + 1]) in changed_between for i in range(lo, hi))
        clock, sleeps = [0.0], []
        pair = lambda commit, **kw: release.paired_build(self.ecr, commit, is_ancestor=is_ancestor, changed=changed,
                                                         clock=lambda: clock[0], sleep=lambda t: (sleeps.append(t), clock.__setitem__(0, clock[0] + t)),
                                                         timeout=kw.get("timeout", 600))
        b1 = self.ecr.push("rev-" + c1)
        release.record(self.ecr, b1)
        self.assertEqual(pair(c1), b1)  # the commit's own build
        self.assertEqual(pair(c2), b1)  # c2 changed no lane input since c1
        # c0 is behind a lane change: keep-built already holds a newer build and only moves forward, so
        # no build can pair with it and the release refuses at once.
        with self.assertRaisesRegex(RuntimeError, "newer lane build"):
            pair(c0, timeout=90)
        self.assertEqual(sleeps, [])
        # A build that lands while the deploy waits is taken.
        changed_between.add((c1, c2))
        b2 = self.ecr.push("rev-" + c2)
        sleeps.clear()
        real_sleep = lambda t: (sleeps.append(t), clock.__setitem__(0, clock[0] + t), release.record(self.ecr, b2))
        self.assertEqual(release.paired_build(self.ecr, c2, is_ancestor=is_ancestor, changed=changed,
                                              clock=lambda: clock[0], sleep=real_sleep, timeout=600), b2)
        self.assertEqual(sleeps, [30])

    def test_with_nothing_built_the_release_waits_then_says_so(self):
        clock, sleeps = [0.0], []
        with self.assertRaisesRegex(RuntimeError, "no lane build is recorded"):
            release.paired_build(self.ecr, "3" * 40, is_ancestor=lambda a, b: True, changed=lambda a, b: True,
                                 clock=lambda: clock[0], timeout=60,
                                 sleep=lambda t: (sleeps.append(t), clock.__setitem__(0, clock[0] + t)))
        self.assertEqual(sleeps, [30, 30])

    def test_git_lists_every_changed_path_whole(self):
        # A file renamed out of the lane inputs counts under its old path too, and a path git would quote
        # (not ASCII) comes back as it is.
        import subprocess as sp
        with tempfile.TemporaryDirectory() as repo:
            git = lambda *a: sp.run(["git", "-C", repo, "-c", "user.name=t", "-c", "user.email=t@example.invalid", *a],
                                    check=True, capture_output=True, text=True).stdout.strip()
            git("init", "-q")
            os.makedirs(os.path.join(repo, "src"))
            Path(repo, "src", "moved.rs").write_text("fn a() {}\n" * 20)
            git("add", "-A")
            git("commit", "-q", "-m", "one")
            first = git("rev-parse", "HEAD")
            os.makedirs(os.path.join(repo, "docs"))
            git("mv", "src/moved.rs", "docs/moved.rs")
            Path(repo, "src", "caf\u00e9.rs").write_text("fn b() {}\n")
            git("add", "-A")
            git("commit", "-q", "-m", "two")
            second = git("rev-parse", "HEAD")
            changed = release.git_changed(first, second, cwd=repo)
        self.assertIn("src/moved.rs", changed)
        self.assertIn("src/caf\u00e9.rs", changed)
        self.assertTrue(release.matches("src/moved.rs", ["src/**"]))

    def test_lane_inputs_are_the_workflows_push_paths(self):
        inputs = release.lane_inputs((HERE.parents[1] / ".github/workflows/fleet-lambda-image.yml").read_text())
        self.assertIn("src/**", inputs)
        self.assertIn("fleet/renderer/**", inputs)
        self.assertTrue(release.matches("src/fleet/tunnel.rs", inputs))
        self.assertFalse(release.matches("fleet/coord/publish.py", inputs))

    def test_ci_records_each_build_and_a_selection_takes_it_by_name(self):
        a = self.ecr.push("rev-a")
        release.record(self.ecr, a)
        release.run(["select", "--stack", "stage", "--built", "--running", "none"], ecr=self.ecr)
        b = self.ecr.push("rev-b")
        release.record(self.ecr, b)
        self.assertEqual(self.ecr.tagged("keep-built").digest, b)
        release.run(["select", "--stack", "stage", "--built", "--running", a], ecr=self.ecr)
        self.assertEqual(release.tags(self.ecr, "stage"), {"keep-stage-current": b, "keep-stage-previous": a})
        with self.assertRaises(SystemExit):
            release.arguments(["select", "--stack", "stage", "--built", "--digest", a, "--running", "none"])

    def test_an_older_commit_published_later_does_not_move_the_record_back(self):
        # Builds of two main pushes finish out of order, or an older run is rerun: the older
        # commit's image is pushed after the newer one's. Commit order decides, not push order.
        older, newer = "a" * 40, "b" * 40
        history = {(older, newer)}  # older is an ancestor of newer
        is_ancestor = lambda a, b: a == b or (a, b) in history
        b = self.ecr.push("rev-" + newer)
        release.record(self.ecr, b, newer, is_ancestor)
        a = self.ecr.push("rev-" + older)
        self.assertEqual(release.record(self.ecr, a, older, is_ancestor)["status"], "kept")
        self.assertEqual(self.ecr.tagged("keep-built").digest, b)
        c = self.ecr.push("rev-" + "c" * 40)
        history |= {(newer, "c" * 40)}
        self.assertEqual(release.record(self.ecr, c, "c" * 40, is_ancestor)["status"], "recorded")
        self.assertEqual(self.ecr.tagged("keep-built").digest, c)
        release.record(self.ecr, c, "c" * 40, is_ancestor)  # the same build again

    def test_the_newest_build_is_kept_and_an_older_unselected_one_is_not(self):
        a = self.ecr.push("rev-a")
        release.record(self.ecr, a)
        b = self.ecr.push("rev-b")
        release.record(self.ecr, b)
        later = self.pushes(6, start=10)
        self.ecr.lifecycle()
        self.assertNotIn(a, self.ecr.images)
        self.assertIn(b, self.ecr.images)
        self.assertTrue(set(later[1:]) <= set(self.ecr.images))

    def test_a_selection_of_nothing_built_is_refused(self):
        with self.assertRaisesRegex(LookupError, "no lane image"):
            release.run(["select", "--stack", "stage", "--built", "--running", "none"], ecr=self.ecr)

    def test_arguments_are_digests(self):
        for bad in ["latest", "sha256:" + "A" * 64, "sha256:" + "a" * 63, ""]:
            with self.assertRaises(ValueError):
                release.digest(bad)
        self.assertIsNone(release.digest("none", none=True))
        with self.assertRaises(ValueError):
            release.digest("none")
        with self.assertRaises(SystemExit):
            release.arguments(["select", "--stack", "other", "--digest", "x", "--running", "none"])


class Publish(unittest.TestCase):
    """CI's push: rev-<commit> by digest, then keep-built, against a fake repository and fake commands."""

    COMMIT = "c" * 40
    REPO = "123456789012.dkr.ecr.us-east-1.amazonaws.com/fcvm-fleet/lambda-tile"

    def setUp(self):
        self.ecr = FakeEcr()
        self.calls = []
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        patcher = patch.object(release.tempfile, "gettempdir", return_value=self.tmp.name)
        patcher.start()
        self.addCleanup(patcher.stop)

    def commands(self, push_digest=None):
        """podman and aws as the runner has them: a push lands rev-<commit> in the fake repository."""
        def run(args, input=None, capture_output=None, text=None, env=None, check=None):
            self.calls.append(list(args))
            out = ""
            if args[:2] == ["podman", "push"]:
                digest = self.ecr.push(args[-1].rsplit(":", 1)[1])
                with open(args[args.index("--digestfile") + 1], "w") as f:
                    f.write(push_digest or digest)
            if args[:3] == ["aws", "ecr", "get-login-password"]:
                out = "password"
            if args[:3] == ["podman", "image", "inspect"]:
                out = hashlib.sha256(("rev-" + args[-1].rsplit(":", 1)[1]).encode()).hexdigest() + "\n"
            code = 1 if args[:2] == ["git", "merge-base"] else 0
            return type("R", (), {"returncode": code, "stdout": out, "stderr": ""})()
        return patch.object(release.subprocess, "run", side_effect=run)

    def test_ci_pushes_rev_commit_and_records_its_digest(self):
        with self.commands():
            result = release.publish(self.ecr, "lane.tar", self.COMMIT, self.REPO)
        pushed = self.ecr.tagged("rev-" + self.COMMIT).digest
        self.assertEqual(result, {"tag": "rev-" + self.COMMIT, "digest": pushed, "status": "recorded"})
        self.assertEqual(self.ecr.tagged("keep-built").digest, pushed)
        push = next(c for c in self.calls if c[:2] == ["podman", "push"])
        self.assertEqual(push[-2:], ["localhost/fcvm-lambda-tile:" + self.COMMIT,
                                     "docker://" + self.REPO + ":rev-" + self.COMMIT])
        login = next(c for c in self.calls if c[:2] == ["podman", "login"])
        self.assertIn("--password-stdin", login)
        self.assertNotIn("password", login)

    def test_a_rerun_records_the_digest_already_pushed(self):
        pushed = self.ecr.push("rev-" + self.COMMIT)
        with self.commands():
            release.publish(self.ecr, "lane.tar", self.COMMIT, self.REPO)
        self.assertFalse([c for c in self.calls if c[:2] == ["podman", "push"]])
        self.assertEqual(self.ecr.tagged("keep-built").digest, pushed)

    def test_an_image_pushed_by_hand_under_the_tag_is_not_recorded(self):
        # rev-<commit> holds an image another build pushed (by hand, from another tree): it is
        # not the image this run built and checked, so nothing is recorded.
        self.ecr.push("rev-" + self.COMMIT, config="sha256:" + "e" * 64)
        with self.commands():
            with self.assertRaisesRegex(RuntimeError, "not the image this run built"):
                release.publish(self.ecr, "lane.tar", self.COMMIT, self.REPO)
        self.assertIsNone(self.ecr.tagged("keep-built"))

    def test_publish_takes_commit_order_from_git(self):
        with self.commands():
            release.publish(self.ecr, "lane.tar", self.COMMIT, self.REPO)
        ancestry = [c for c in self.calls if c[:3] == ["git", "merge-base", "--is-ancestor"]]
        self.assertEqual(ancestry, [])  # nothing recorded yet: nothing to order against
        older = "d" * 40
        self.ecr.push("rev-" + older)
        with self.commands():
            result = release.publish(self.ecr, "lane.tar", older, self.REPO)
        self.assertEqual([c for c in self.calls if c[:3] == ["git", "merge-base", "--is-ancestor"]],
                         [["git", "merge-base", "--is-ancestor", self.COMMIT, older]])
        self.assertEqual(result["status"], "kept")  # the fake git answers "not an ancestor"

    def test_a_push_whose_digest_the_repository_disagrees_with_records_nothing(self):
        with self.commands(push_digest="sha256:" + "f" * 64):
            with self.assertRaisesRegex(RuntimeError, "digest"):
                release.publish(self.ecr, "lane.tar", self.COMMIT, self.REPO)
        self.assertIsNone(self.ecr.tagged("keep-built"))

    def test_publish_refuses_another_repository_or_a_short_commit(self):
        for commit, repo in [(self.COMMIT, self.REPO.replace("us-east-1", "us-west-2")),
                             (self.COMMIT, self.REPO + "x"), ("c" * 7, self.REPO)]:
            with self.commands(), self.assertRaises(ValueError):
                release.publish(self.ecr, "lane.tar", commit, repo)
        self.assertEqual(self.calls, [])


# include!, include_str! and include_bytes!, with a literal path or one under CARGO_MANIFEST_DIR.
INCLUDE = re.compile(r'\binclude(?:_str|_bytes)?!\s*\(\s*(concat!\s*\(\s*env!\s*\(\s*"CARGO_MANIFEST_DIR"\s*\)\s*,\s*)?'
                     r'(?:"([^"]+)")?')


def path_dependencies(manifest):
    """The `path` of every dependency that builds into the package: [dependencies], [build-dependencies],
    their target-specific forms, and [patch] entries. Dev-dependencies build only tests.

    The checker supports exactly the dependency forms this tree uses and refuses the rest rather than
    modelling them: a `workspace = true` dependency (resolved from [workspace.dependencies]) or a [replace]
    table fails with a ValueError, as do a [patch] table and `paths` overrides in .cargo/config.toml
    (cargo_config_patches). Extend the
    checker when the tree starts using one."""
    data = tomllib.loads(manifest)
    tables = [data.get("dependencies", {}), data.get("build-dependencies", {})]
    for target in data.get("target", {}).values():
        tables += [target.get("dependencies", {}), target.get("build-dependencies", {})]
    tables += list(data.get("patch", {}).values())
    if "replace" in data:
        raise ValueError("a [replace] table: the lane image's input check does not model it")
    deps = [dep for table in tables for dep in table.values() if isinstance(dep, dict)]
    if any("workspace" in dep for dep in deps):
        raise ValueError("a dependency with workspace = true: the lane image's input check does not model it")
    return [dep["path"] for dep in deps if "path" in dep]


def cargo_config_patches(config):
    """Refuse a [patch] table or `paths` overrides in .cargo/config.toml: the input check models neither."""
    data = tomllib.loads(config)
    if "patch" in data:
        raise ValueError(".cargo/config.toml has a [patch] table: the lane image's input check does not model it")
    if "paths" in data:
        raise ValueError(".cargo/config.toml has paths overrides: the lane image's input check does not model them")


class Paths(unittest.TestCase):
    def test_every_dependency_table_that_builds_into_the_package_is_read(self):
        manifest = """
[dependencies]
a = { path = "a" }
remote = "1"
[dependencies.b]
path = "b"
[build-dependencies]
c = { path = "c" }
[target.'cfg(unix)'.dependencies]
d = { path = "d" }
[target.'cfg(unix)'.build-dependencies]
e = { path = "e" }
[dev-dependencies]
tests-only = { path = "t" }
[patch.crates-io]
f = { path = "f" }
"""
        self.assertEqual(sorted(path_dependencies(manifest)), ["a", "b", "c", "d", "e", "f"])

    def test_a_dependency_form_the_checker_does_not_model_is_refused(self):
        with self.assertRaisesRegex(ValueError, "workspace = true"):
            path_dependencies('[dependencies]\nshared = { workspace = true }\n')
        with self.assertRaisesRegex(ValueError, "workspace = true"):
            path_dependencies("[target.'cfg(unix)'.build-dependencies]\nshared = { workspace = true }\n")
        with self.assertRaisesRegex(ValueError, r"\[patch\]"):
            cargo_config_patches('[patch.crates-io]\nfoo = { path = "foo" }\n')
        with self.assertRaisesRegex(ValueError, "paths"):
            cargo_config_patches('paths = ["../local-crate"]\n')
        with self.assertRaisesRegex(ValueError, r"\[replace\]"):
            path_dependencies('[replace]\n"foo:0.1.0" = { path = "foo" }\n')
        cargo_config_patches('[build]\njobs = 4\n')
        cargo_config_patches((HERE.parents[1] / ".cargo/config.toml").read_text())

    def test_every_include_form_is_seen(self):
        found = [m.group(2) for m in INCLUDE.finditer(
            'include!("gen.rs"); include_str!("a.txt"); include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/b.bin"));'
            ' include_str!(MACRO_PATH)')]
        self.assertEqual(found, ["gen.rs", "a.txt", "/b.bin", None])


class Workflow(unittest.TestCase):
    """.github/workflows/fleet-lambda-image.yml, as text (the runner's python has no YAML module)."""
    text = (HERE.parents[1] / ".github/workflows/fleet-lambda-image.yml").read_text()

    def paths(self, event):
        block = self.text.split(f"\n  {event}:\n", 1)[1].split("\n  workflow_dispatch:", 1)[0].split("\n  pull_request:", 1)[0]
        return re.findall(r"^      - '([^']+)'", block, re.M)

    # The build's own files: the manifests, the toolchain and cargo configuration, and the
    # Makefile whose recipe (build-fleet-coordinator) builds the binary through the cargo wrapper
    # scripts below (and whatever they source).
    BUILD_FILES = ["Cargo.toml", "Cargo.lock", "build.rs", "Makefile", "rust-toolchain.toml", ".cargo/config.toml",
                   "rootfs-config.toml", "fleet/lambda/Makefile", "fleet/lambda/Containerfile",
                   "fleet/renderer/Makefile", "fleet/renderer/Containerfile"]
    BUILD_SCRIPTS = ["scripts/cargo-target-run.sh", "scripts/cargo-target-link.sh"]

    @staticmethod
    def files(root):
        for parent, dirs, names in os.walk(root):
            dirs[:] = [d for d in dirs if d not in ("target", ".git", "node_modules")]
            yield from (os.path.join(parent, n) for n in names)

    def crates(self, repo):
        """The fcvm package's path dependencies, and theirs."""
        found, todo = [], ["."]
        while todo:
            here = todo.pop()
            for rel in path_dependencies((repo / here / "Cargo.toml").read_text()):
                crate = os.path.normpath(os.path.join(here, rel))
                if crate not in found:
                    found.append(crate)
                    todo.append(crate)
        return found

    def sourced(self, repo, scripts):
        """The scripts, and every script they source from their own directory."""
        out, todo = set(), list(scripts)
        while todo:
            script = todo.pop()
            if script in out:
                continue
            out.add(script)
            for name in re.findall(r'^\s*\. "\$script_dir/([^"]+)"', (repo / script).read_text(), re.M):
                todo.append(os.path.join(os.path.dirname(script), name))
        return out

    def inputs(self):
        """Every file the lane image is built from, derived from the sources: each source file of the
        fcvm package and of its path dependencies, every file those sources include_str!/include_bytes!,
        the build files, the renderer's directory and the files its Containerfile copies from the build
        contexts its Makefile passes."""
        repo = HERE.parents[1]
        crates = self.crates(repo)
        self.assertIn("fleet/core", crates)
        cargo_config_patches((repo / ".cargo/config.toml").read_text())
        out = set(self.BUILD_FILES) | self.sourced(repo, self.BUILD_SCRIPTS) | {c + "/Cargo.toml" for c in crates}
        for root in ["src"] + crates:
            for path in self.files(repo / root):
                rel = os.path.relpath(path, repo)
                if rel.startswith(tuple(c + "/" for c in crates if c != root and c.startswith(root + "/"))):
                    continue
                out.add(rel)
                if not rel.endswith(".rs"):
                    continue
                crate = next((c for c in crates if rel.startswith(c + "/")), ".")
                for m in INCLUDE.finditer(Path(path).read_text()):
                    self.assertIsNotNone(m.group(2), f"{rel}: an include this test cannot resolve")
                    base = crate if m.group(1) else os.path.dirname(rel)
                    target = os.path.normpath(os.path.join(base, m.group(2).lstrip("/") if m.group(1) else m.group(2)))
                    self.assertTrue((repo / target).exists(), f"{rel} includes {target}, which does not exist")
                    out.add(target)
        out |= {os.path.relpath(p, repo) for p in self.files(repo / "fleet/renderer")}
        makefile = (repo / "fleet/renderer/Makefile").read_text()
        contexts = dict(re.findall(r"--build-context (\S+)=(\S+)", makefile))
        self.assertTrue(contexts)
        for ctx, name in re.findall(r"^COPY --from=(\S+) (\S+) ", (repo / "fleet/renderer/Containerfile").read_text(), re.M):
            if ctx in contexts:
                out.add(os.path.normpath(os.path.join("fleet/renderer", contexts[ctx], name)))
        return out

    def test_a_change_to_anything_in_the_image_builds_it(self):
        inputs = self.inputs()
        # The derivation finds what a hand-written list missed.
        for known in ("fleet/published-ports.json", "scripts/passt-udp-sock-errs-null-flow.patch",
                      "fleet/core/tests/autopilot_lap.json", "fleet/web/core/layout.ts", "src/paths.rs"):
            self.assertIn(known, inputs)
        for event in ("push", "pull_request"):
            patterns = self.paths(event)
            missing = sorted(p for p in inputs if not any(fnmatch.fnmatchcase(p, g.replace("**", "*")) for g in patterns))
            self.assertEqual(missing, [], f"{event}: inputs of the lane image that start no build")

    def test_every_build_publishes_in_commit_order(self):
        publish = self.text.split("\n  publish:\n", 1)[1]
        # queue: max keeps every pending publish: GitHub's default of one pending job per group
        # cancels the older pending one, which can be the newest commit's.
        self.assertRegex(publish, r"concurrency:\n      group: fleet-lambda-image\n      cancel-in-progress: false\n"
                                  r"(?:      #.*\n)*      queue: max\n")
        # release.py publish orders builds with git merge-base: it needs the history.
        self.assertRegex(publish, r"persist-credentials: false\n          fetch-depth: 0\n")


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

    def test_a_caller_in_another_account_names_the_repositorys(self):
        found = json.dumps({"images": [{"imageId": {"imageDigest": "sha256:" + "3" * 64},
                                        "imageManifest": "{}", "imageManifestMediaType": release.MEDIA_TYPES[0]}]})
        image = release.Image("sha256:" + "3" * 64, "{}", release.MEDIA_TYPES[0])
        ok = json.dumps({"image": {"imageId": {"imageDigest": image.digest}}})
        calls, p = self.run_with([(0, found, ""), (0, ok, "")])
        with p:
            ecr = release.Ecr(registry="123456789012")
            ecr.tagged("keep-built")
            ecr.put("keep-built", image)
        self.assertEqual(calls[0][0][calls[0][0].index("--registry-id") + 1], "123456789012")
        self.assertEqual(json.loads(calls[1][1])["registryId"], "123456789012")
        with patch.dict(os.environ, {"LAMBDA_TILE_REPO": Publish.REPO}):
            with patch.object(release, "publish", return_value={}) as published:
                release.run(["publish", "--archive", "a.tar", "--commit", "c" * 40])
        self.assertEqual(published.call_args.args[0].registry, "123456789012")

    def test_an_images_tags_are_read_whole(self):
        out = json.dumps({"imageDetails": [{"imageTags": ["rev-" + "a" * 40, "keep-built"]}]})
        calls, p = self.run_with([(0, out, "")])
        with p:
            self.assertEqual(release.Ecr().tags("sha256:" + "4" * 64), ["keep-built", "rev-" + "a" * 40])
        self.assertEqual(calls[0][0][:3], ["aws", "ecr", "describe-images"])
        self.assertIn("imageDigest=sha256:" + "4" * 64, calls[0][0])

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
