"""用本地发行产物和 GitHub 替身验证版本、准入、草稿恢复及公开版本不覆盖。"""
import importlib.util
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location("release_portable", pathlib.Path(__file__).with_name("release-portable.py"))
release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release)
SOURCE = "a" * 40
OTHER = "b" * 40
VERSION = "v0.1.1"


class FakeGitHub:
    def __init__(self, refs=()):
        self.refs = list(refs)
        self.head = SOURCE
        self.published = None
        self.calls = []
        self.uploads = 0
        self.partial = False
        self.advance_during_upload = False
        self.ci = [{"id": 1, "name": "Rust 持续集成", "status": "completed", "conclusion": "success", "html_url": "ci"}]

    def api(self, route, *, method="GET", data=None, optional=False, pages=False):
        self.calls.append((method, route, data))
        if route.startswith("git/matching-refs/"):
            return [self.refs]
        if route == "git/ref/heads/main":
            return {"object": {"type": "commit", "sha": self.head}}
        if route.startswith("git/tags/"):
            return {"object": {"type": "commit", "sha": SOURCE}}
        if route.startswith("git/ref/tags/"):
            return next((r for r in self.refs if r["ref"] == "refs/tags/" + route.split("/")[-1]), None)
        if route == "git/refs" and method == "POST":
            result = {"ref": data["ref"], "object": {"type": "commit", "sha": data["sha"]}}
            self.refs.append(result)
            return result
        if route.startswith("actions/runs?"):
            assert f"head_sha={SOURCE}&event=push" in route
            return {"workflow_runs": self.ci}
        if route.startswith("releases/tags/"):
            return self.published
        if route == "releases" and method == "POST":
            self.published = {"id": 1, "html_url": "release-url", "assets": [], **data}
            return self.published
        if route == "releases/1":
            if method == "PATCH":
                self.published.update(data)
            return self.published
        raise AssertionError((route, method))

    def upload(self, tag, files):
        self.uploads += 1
        self.published["assets"] = [{"name": f.name, "size": f.stat().st_size, "digest": "sha256:" + release.digest(f)} for f in files]
        if self.partial:
            self.published["assets"].pop()
        if self.advance_during_upload:
            self.head = OTHER


def ref(tag, source=OTHER, kind="commit"):
    return {"ref": "refs/tags/" + tag, "object": {"type": kind, "sha": source}}


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.folder = pathlib.Path(self.temporary.name)
        for label in release.LABELS:
            name = f"Eve-{VERSION}-{label}." + ("zip" if label.startswith("windows-") else "tar.gz")
            archive = self.folder / name
            archive.write_bytes((label + " verified process").encode())
            report = {"platform_label": label, "version": VERSION, "source_commit": SOURCE,
                      "source_tree": "c" * 40, "archive": name, "archive_sha256": release.digest(archive),
                      "node_launcher": True, "console_ctrl_c": True,
                      "external_model_requests": 0, "production_qq_connections": 0}
            report.update({key: True for key in ("unicode_and_space_path", "web_http", "memory_and_learning_views",
                                                "qq_segmented_delivery", "recovery_without_replay", "client_update_download_verified",
                                                "client_update_next_start", "client_update_rollback", "client_update_preserved_user_files")})
            (self.folder / f"acceptance-{label}.json").write_text(json.dumps(report), encoding="utf-8")

    def test_next_patch_uses_all_stable_tags_and_ignores_prerelease(self):
        github = FakeGitHub([ref("v0.1.9"), ref("v0.2.3"), ref("v2.0.0-rc.1")])
        self.assertEqual(release.select_version(github, SOURCE, "0.1.0"), "v0.2.4")

    def test_same_commit_reuses_lightweight_and_annotated_tag(self):
        for tag in (ref(VERSION, SOURCE), ref(VERSION, OTHER, "tag")):
            with self.subTest(tag=tag):
                self.assertEqual(release.select_version(FakeGitHub([tag]), SOURCE, "0.1.0"), VERSION)

    def test_main_dispatch_publishes_and_pr_or_feature_dispatch_does_not(self):
        github = FakeGitHub([ref("v0.1.0")])
        for event in ("push", "workflow_dispatch"):
            self.assertEqual(release.prepare(github, event, "refs/heads/main", SOURCE, "0.1.0"),
                             {"release_tag": VERSION, "publish": "true", "automatic": "true"})
        for event, branch in (("pull_request", "refs/pull/1/merge"), ("workflow_dispatch", "refs/heads/feature")):
            before = len(github.calls)
            self.assertEqual(release.prepare(github, event, branch, SOURCE, "0.1.0")["publish"], "false")
            self.assertEqual(len(github.calls), before)
        self.assertEqual(release.prepare(github, "push", "refs/tags/v0.2.0-rc.1", SOURCE, "0.1.0")["release_tag"], "v0.2.0-rc.1")
        self.assertEqual(release.select_version(FakeGitHub(), SOURCE, "0.1.0"), "v0.1.0")

    def test_complete_verification_contains_six_archives_and_two_metadata_files(self):
        files = release.verify(self.folder, VERSION, SOURCE)
        self.assertEqual(len(files), 8)
        evidence = json.loads((self.folder / "release-verification.json").read_text())
        self.assertEqual(set(evidence), release.LABELS)

    def test_actual_prepare_entry_writes_pr_outputs(self):
        output = self.folder / "github-output.txt"
        environment = os.environ | {"GITHUB_REPOSITORY": "owner/repo", "GITHUB_SHA": SOURCE,
                                    "GITHUB_EVENT_NAME": "pull_request", "GITHUB_REF": "refs/pull/1/merge",
                                    "GITHUB_OUTPUT": str(output)}
        process = subprocess.run([sys.executable, str(pathlib.Path(__file__).with_name("release-portable.py")), "prepare"],
                                 env=environment, capture_output=True, text=True, encoding="utf-8", check=True)
        self.assertEqual(output.read_text().splitlines(), ["release_tag=", "publish=false", "automatic=false"])
        self.assertEqual(json.loads(process.stdout)["publish"], "false")

    def test_incomplete_or_invalid_evidence_is_rejected(self):
        report_file = self.folder / "acceptance-linux-x64.json"
        original = report_file.read_bytes()
        for key, value in (("source_commit", OTHER), ("version", "v9.0.0"), ("archive", "../outside.tar.gz"),
                           ("archive_sha256", "wrong"), ("web_http", "true"), ("console_ctrl_c", False),
                           ("production_qq_connections", 1), ("external_model_requests", 1), ("client_update_next_start", False),
                           ("client_update_rollback", None), ("client_update_preserved_user_files", "true")):
            with self.subTest(key=key):
                report = json.loads(original)
                report[key] = value
                report_file.write_text(json.dumps(report))
                with self.assertRaises(ValueError):
                    release.verify(self.folder, VERSION, SOURCE)
        report_file.unlink()
        with self.assertRaises(ValueError):
            release.verify(self.folder, VERSION, SOURCE)

    def test_failed_or_missing_ci_creates_no_tag_or_release(self):
        for runs in ([], [{"id": 1, "name": "Rust 持续集成", "status": "completed", "conclusion": "failure", "html_url": "failed-ci"}]):
            github = FakeGitHub()
            github.ci = runs
            with self.subTest(runs=runs), self.assertRaises((ValueError, TimeoutError)):
                release.wait_for_ci(github, SOURCE, timeout=0)
            self.assertFalse(any(method == "POST" for method, _, _ in github.calls))

    def test_latest_filtered_workflow_failure_blocks_publication(self):
        github = FakeGitHub()
        github.ci += [{"id": 2, "name": "QQBot 通道离线验收", "status": "completed", "conclusion": "failure", "html_url": "qq-ci"}]
        with self.assertRaises(ValueError):
            release.publish(github, self.folder, VERSION, SOURCE, True)
        self.assertFalse(github.refs)
        self.assertIsNone(github.published)

    def test_updated_main_skips_old_commit_without_creating_a_tag(self):
        github = FakeGitHub()
        github.head = OTHER
        self.assertIsNone(release.publish(github, self.folder, VERSION, SOURCE, True))
        self.assertFalse(github.refs)
        self.assertIsNone(github.published)

    def test_tag_collision_never_moves_existing_ref_or_uploads(self):
        github = FakeGitHub([ref(VERSION)])
        with self.assertRaises(ValueError):
            release.publish(github, self.folder, VERSION, SOURCE, False)
        self.assertEqual(github.refs[0]["object"]["sha"], OTHER)
        self.assertEqual(github.uploads, 0)

    def test_partial_upload_preserves_draft_then_retry_publishes_same_version(self):
        github = FakeGitHub()
        github.partial = True
        with self.assertRaises(ValueError):
            release.publish(github, self.folder, VERSION, SOURCE, True)
        self.assertTrue(github.published["draft"])
        github.partial = False
        self.assertEqual(release.publish(github, self.folder, VERSION, SOURCE, True), "release-url")
        self.assertFalse(github.published["draft"])
        self.assertEqual(sum(method == "POST" and route == "releases" for method, route, _ in github.calls), 1)
        self.assertEqual(len(github.refs), 1)
        uploads = github.uploads
        self.assertEqual(release.publish(github, self.folder, VERSION, SOURCE, True), "release-url")
        self.assertEqual(github.uploads, uploads)

    def test_main_update_during_upload_keeps_draft(self):
        github = FakeGitHub()
        github.advance_during_upload = True
        self.assertIsNone(release.publish(github, self.folder, VERSION, SOURCE, True))
        self.assertTrue(github.published["draft"])

    def test_public_incomplete_release_is_not_modified(self):
        github = FakeGitHub([ref(VERSION, SOURCE)])
        github.published = {"draft": False, "assets": [], "html_url": "release-url"}
        with self.assertRaises(ValueError):
            release.publish(github, self.folder, VERSION, SOURCE, False)
        self.assertEqual(github.uploads, 0)
        self.assertFalse(any(method == "PATCH" for method, _, _ in github.calls))


if __name__ == "__main__":
    unittest.main()
