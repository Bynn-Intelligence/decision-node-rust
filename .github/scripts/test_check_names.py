"""Tests for check_names.py, with invented words in place of the real list.

    python3 .github/scripts/test_check_names.py

The words come from CHECK_NAMES_TEST_WORDS (one a line) and default to three invented
ones. Every run asserts that the check's output never holds a word of the list.
"""

from __future__ import annotations

import base64
import gzip
import io
import json
import os
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile

SCRIPT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "check_names.py")
WORDS = os.environ.get("CHECK_NAMES_TEST_WORDS", "zorvex\nquillbrand\nmarrow plinth")
FIRST, SECOND, PHRASE = (WORDS.splitlines() + ["", "", ""])[:3]
# Built from pieces so that this file matches none of the check's plain patterns.
DOCS_PATH = "prod" + "ucts/decide.md"
BOOSTER = "wanted_" + "person"


class CheckNames(unittest.TestCase):
    def setUp(self) -> None:
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)

    def write(self, name: str, data: str | bytes) -> str:
        path = os.path.join(self.dir.name, name)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "wb") as f:
            f.write(data.encode() if isinstance(data, str) else data)
        return path

    def run_check(self, *args: str, words: str | None = WORDS, extra: dict | None = None, cwd: str | None = None):
        env = {k: v for k, v in os.environ.items() if not k.startswith("GITHUB_") and k != "DN_NAME_CHECK_WORDS"}
        if words is not None:
            env["DN_NAME_CHECK_WORDS"] = words
        env.update(extra or {})
        result = subprocess.run([sys.executable, SCRIPT, *args], capture_output=True, text=True, env=env, cwd=cwd)
        output = result.stdout + result.stderr
        for word in WORDS.lower().splitlines():
            for part in word.split():
                self.assertNotIn(part, output.lower(), "the output holds a word of the list")
        return result.returncode, output

    def test_clean_file_passes(self) -> None:
        code, out = self.run_check(self.write("a.txt", "nothing to see here\n"))
        self.assertEqual(code, 0, out)
        self.assertIn("0 findings", out)

    def test_word_is_found_case_insensitively_with_its_line(self) -> None:
        path = self.write("a.txt", "one\ntwo %s three\n" % FIRST.upper())
        code, out = self.run_check(path)
        self.assertEqual(code, 1)
        self.assertIn("%s:2: internal name found" % path, out)

    def test_word_boundaries(self) -> None:
        for text in ("%s_client", "my-%s", "use %s.", "%s2", "%sClient"):
            word = FIRST.capitalize() if text.endswith("Client") else FIRST
            code, out = self.run_check(self.write("hit.txt", "x " + text % word + "\n"))
            self.assertEqual(code, 1, "%r should hit" % text)
        for text in ("%sed", "un%s", "%sclient"):
            code, out = self.run_check(self.write("miss.txt", "x " + text % FIRST + "\n"))
            self.assertEqual(code, 0, "%r should not hit: %s" % (text, out))

    def test_camel_case_hump_is_a_boundary(self) -> None:
        code, _ = self.run_check(self.write("a.ts", "const x = new My%sClient()\n" % FIRST.capitalize()))
        self.assertEqual(code, 1)

    def test_phrase_matches_across_separators(self) -> None:
        a, b = PHRASE.split()
        for text in ("%s %s", "%s  %s", "%s_%s", "%s-%s", "%s\n%s"):
            code, _ = self.run_check(self.write("p.txt", text % (a.capitalize(), b) + "\n"))
            self.assertEqual(code, 1, "%r should hit" % text)
        code, _ = self.run_check(self.write("p.txt", a + b + "\n"))
        self.assertEqual(code, 0)
        code, _ = self.run_check(self.write("p.txt", a.capitalize() + b.capitalize() + "\n"))
        self.assertEqual(code, 1)

    def test_archive_members_and_nested_archives(self) -> None:
        inner = io.BytesIO()
        with zipfile.ZipFile(inner, "w") as z:
            z.writestr("pkg/inner.txt", "line\nhas %s\n" % SECOND)
        outer = io.BytesIO()
        with tarfile.open(fileobj=outer, mode="w:gz") as t:
            info = tarfile.TarInfo("pkg/lib.jar")
            info.size = len(inner.getvalue())
            t.addfile(info, io.BytesIO(inner.getvalue()))
        path = self.write("pkg.tgz", outer.getvalue())
        code, out = self.run_check(path)
        self.assertEqual(code, 1)
        self.assertIn("%s!pkg/lib.jar!pkg/inner.txt:2: internal name found" % path, out)

    def test_gem_metadata_is_a_description(self) -> None:
        gem = io.BytesIO()
        with tarfile.open(fileobj=gem, mode="w") as t:
            data = gzip.compress(("summary: %s\n" % BOOSTER).encode())
            info = tarfile.TarInfo("metadata.gz")
            info.size = len(data)
            t.addfile(info, io.BytesIO(data))
        code, out = self.run_check(self.write("x.gem", gem.getvalue()))
        self.assertEqual(code, 1)
        self.assertIn("x.gem!metadata:1: a booster production does not serve yet", out)

    def test_path_holding_a_word_is_not_printed(self) -> None:
        archive = io.BytesIO()
        with zipfile.ZipFile(archive, "w") as z:
            z.writestr("src/%s_client.py" % FIRST, "clean\n")
        path = self.write("pkg.whl", archive.getvalue())
        code, out = self.run_check(path)
        self.assertEqual(code, 1)
        self.assertIn("pkg.whl!<member at depth 1>: a file name holds an internal name", out)
        top = self.write("%s.txt" % FIRST, "clean\n")
        code, out = self.run_check(top)
        self.assertEqual(code, 1)
        self.assertIn("<argument 1>: a file name holds an internal name", out)

    def test_base64_and_utf16(self) -> None:
        token = base64.b64encode(("call the %s model" % FIRST).encode()).decode()
        code, out = self.run_check(self.write("b.txt", "a\nblob: %s\n" % token))
        self.assertEqual(code, 1)
        self.assertIn("b.txt:2: internal name found", out)
        for pad in (b"", b"\0"):
            data = b"\0\x01MZ" + pad + ("the %s client" % FIRST).encode("utf-16-le")
            code, out = self.run_check(self.write("a.dll", data))
            self.assertEqual(code, 1, out)
            self.assertIn("a.dll: internal name found", out)

    def test_plain_patterns_and_boosters(self) -> None:
        code, out = self.run_check(self.write("src/a.txt", "see %s\n" % DOCS_PATH))
        self.assertEqual(code, 1)
        self.assertIn("a.txt:1: a path into internal documents", out)
        code, _ = self.run_check(self.write("src/b.txt", "the %s booster\n" % BOOSTER))
        self.assertEqual(code, 0)
        code, _ = self.run_check("--unserved-everywhere", self.write("src/b.txt", "the %s booster\n" % BOOSTER))
        self.assertEqual(code, 1)
        code, out = self.run_check(self.write("README.md", "the %s booster\n" % BOOSTER))
        self.assertEqual(code, 1)
        self.assertIn("README.md:1: a booster production does not serve yet", out)

    def test_fails_closed_without_the_list(self) -> None:
        path = self.write("a.txt", "clean\n")
        for words in (None, "", "\n \n"):
            code, out = self.run_check(path, words=words)
            self.assertEqual(code, 1)
            self.assertIn("DN_NAME_CHECK_WORDS is empty or not set", out)
        event = self.write(
            "event.json",
            json.dumps(
                {
                    "pull_request": {"head": {"repo": {"full_name": "acme/sdk"}}},
                    "repository": {"full_name": "acme/sdk"},
                }
            ),
        )
        extra = {"GITHUB_EVENT_NAME": "pull_request", "GITHUB_EVENT_PATH": event}
        code, _ = self.run_check(path, words=None, extra=extra)
        self.assertEqual(code, 1, "a pull request from the repository itself has the secret")
        code, _ = self.run_check(path, words=None, extra={"GITHUB_EVENT_NAME": "push", "GITHUB_EVENT_PATH": event})
        self.assertEqual(code, 1)

    def test_names_optional_skips_only_a_missing_list(self) -> None:
        code, out = self.run_check("--names-optional", self.write("a.txt", "has %s\n" % FIRST), words=None)
        self.assertEqual(code, 0, out)
        self.assertIn("::notice::DN_NAME_CHECK_WORDS is not set here", out)
        code, _ = self.run_check("--names-optional", self.write("b.txt", "see %s\n" % DOCS_PATH), words=None)
        self.assertEqual(code, 1, "the other checks still run")
        code, _ = self.run_check("--names-optional", self.write("a.txt", "has %s\n" % FIRST))
        self.assertEqual(code, 1, "with the list set, the names are checked")

    def test_fork_pull_request_skips_the_names_only(self) -> None:
        event = self.write(
            "event.json",
            json.dumps(
                {
                    "pull_request": {"head": {"repo": {"full_name": "someone/sdk"}}},
                    "repository": {"full_name": "acme/sdk"},
                }
            ),
        )
        extra = {"GITHUB_EVENT_NAME": "pull_request", "GITHUB_EVENT_PATH": event}
        code, out = self.run_check(self.write("a.txt", "has %s\n" % FIRST), words=None, extra=extra)
        self.assertEqual(code, 0, out)
        self.assertIn("::notice::DN_NAME_CHECK_WORDS is withheld", out)
        code, out = self.run_check(self.write("b.txt", "see %s\n" % DOCS_PATH), words=None, extra=extra)
        self.assertEqual(code, 1, "the other checks still run")
        code, _ = self.run_check(
            self.write("c.txt", "clean\n"),
            words=None,
            extra={"GITHUB_EVENT_NAME": "pull_request", "GITHUB_ACTOR": "dependabot[bot]"},
        )
        self.assertEqual(code, 0)

    def git(self, repo: str, *args: str) -> str:
        return subprocess.run(
            [
                "git",
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "tag.gpgsign=false",
                *args,
            ],
            cwd=repo,
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()

    def test_commit_messages_since_the_last_tag(self) -> None:
        repo = os.path.join(self.dir.name, "repo")
        os.makedirs(repo)
        self.git(repo, "init", "-q")
        self.git(repo, "commit", "-q", "--allow-empty", "-m", "Old work on %s" % FIRST)
        self.git(repo, "tag", "v0.1.0")
        self.git(repo, "commit", "-q", "--allow-empty", "-m", "Clean change")
        code, out = self.run_check("--commits", cwd=repo)
        self.assertEqual(code, 0, out)
        self.assertIn("1 commit messages checked", out)
        self.git(repo, "commit", "-q", "--allow-empty", "-m", "Tidy\n\nBody names %s here" % SECOND.upper())
        sha = self.git(repo, "rev-parse", "HEAD")
        code, out = self.run_check("--commits", cwd=repo)
        self.assertEqual(code, 1)
        self.assertIn("commit %s: internal name found" % sha[:12], out)
        # Before the first tag, every commit counts; with HEAD tagged, the tag before it.
        self.git(repo, "tag", "-d", "v0.1.0")
        code, out = self.run_check("--commits", cwd=repo)
        self.assertIn("3 commit messages checked", out)
        self.git(repo, "tag", "v0.1.0", "HEAD~2")
        self.git(repo, "tag", "v0.2.0")
        code, out = self.run_check("--commits", cwd=repo)
        self.assertIn("2 commit messages checked", out)
        self.assertEqual(code, 1)

    def test_commits_need_the_full_history(self) -> None:
        origin = os.path.join(self.dir.name, "origin")
        os.makedirs(origin)
        self.git(origin, "init", "-q")
        for n in range(3):
            self.git(origin, "commit", "-q", "--allow-empty", "-m", "c%d" % n)
        shallow = os.path.join(self.dir.name, "shallow")
        self.git(self.dir.name, "clone", "-q", "--depth", "1", "file://" + origin, shallow)
        code, out = self.run_check("--commits", cwd=shallow)
        self.assertEqual(code, 1)
        self.assertIn("needs the full history", out)

    def test_tracked_files(self) -> None:
        repo = os.path.join(self.dir.name, "repo")
        os.makedirs(repo)
        self.git(repo, "init", "-q")
        with open(os.path.join(repo, "a.txt"), "w") as f:
            f.write("has %s\n" % FIRST)
        with open(os.path.join(repo, "untracked.txt"), "w") as f:
            f.write("has %s\n" % FIRST)
        self.git(repo, "add", "a.txt")
        code, out = self.run_check("--tracked", cwd=repo)
        self.assertEqual(code, 1)
        self.assertIn("a.txt:1: internal name found", out)
        self.assertNotIn("untracked.txt", out)

    def test_nothing_to_check_fails(self) -> None:
        empty = os.path.join(self.dir.name, "empty")
        os.makedirs(empty)
        code, out = self.run_check(empty)
        self.assertEqual(code, 1)
        self.assertIn("no file to check", out)
        code, out = self.run_check(os.path.join(self.dir.name, "missing"))
        self.assertEqual(code, 1)
        self.assertIn("does not exist", out)


if __name__ == "__main__":
    unittest.main(verbosity=2)
