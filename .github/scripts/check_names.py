#!/usr/bin/env python3
"""Check the files a release is about to publish, and the commit messages it ships.

    check_names.py [--tracked] [--commits] [--unserved-everywhere] [--names-optional] [PATH...]

Reads every file given, every file under a directory, every file git tracks (--tracked),
and every member of an archive (zip, jar, whl, nupkg, tar, tar.gz, tgz, crate, gem, with
nested archives and .gz members opened in turn). A file fails when it holds:

- a word or phrase of the internal-name list, matched case-insensitively where no
  letter touches it on either side (a digit, an underscore or a camelCase hump is a
  boundary, and a phrase's words may be split by spaces, underscores or hyphens), in
  its text, in its UTF-16 strings and inside base64 runs; a file whose path holds one
  fails too;
- a path into internal documents, a local home directory or an attribution line;
- a booster production does not serve yet, when the file is a README or carries the
  package's description (in every file with --unserved-everywhere).

--commits also checks the message of every commit since the last v* tag before HEAD
(every commit, while there is no such tag) for the internal names.

The internal-name list is read only from the DN_NAME_CHECK_WORDS environment variable,
one lowercase word or phrase a line. Without it the check fails, except on a pull
request from a fork or from Dependabot, which gets no secrets: there the internal
names are skipped with a notice and every other check still runs. --names-optional,
for ordinary CI, skips them as well whenever the list is missing; the release
check never passes it. A hit prints the
file and line, or the commit, and never the matched text; a path that itself holds an
internal name is not printed either.
"""

from __future__ import annotations

import base64
import binascii
import gzip
import io
import json
import os
import re
import subprocess
import sys
import tarfile
import zipfile
from typing import Iterator, List, Optional, Pattern, Tuple

ENV = "DN_NAME_CHECK_WORDS"

# Written so that none of them matches its own source text.
PLAIN = [
    ("a path into internal documents", re.compile(r"docs/[0-9]{2}|backend/[0-9]{2}|product[s]/", re.I)),
    ("a local home directory", re.compile(r"/user[s]/", re.I)),
    ("an attribution line", re.compile(r"co-authored-b[y]|generated[ ]with", re.I)),
]
# Boosters a README or the package's description may name only once production serves
# them, and the files that carry one: a README, a wheel's METADATA or an sdist's
# PKG-INFO, an npm package.json, a gem's metadata.
UNSERVED = re.compile(r"wanted_perso[n]", re.I)
DESCRIBES = re.compile(r"(^|/)(README[^/]*|PKG-INFO|METADATA|package\.json|metadata)$", re.I)

TAR = re.compile(r"\.(tar|tar\.gz|tgz|crate|gem)$", re.I)
B64 = re.compile(r"[A-Za-z0-9+/]{8,}={0,2}")
HUMP = re.compile(r"([a-z0-9])([A-Z])")
FOUND = "internal name found"


def compile_names(raw: str) -> Optional[Pattern[str]]:
    """One pattern for the whole list; None when the list is empty."""
    entries = set()
    for line in raw.splitlines():
        parts = [p for p in re.split(r"[\s_-]+", line.strip().lower()) if p]
        if parts:
            entries.add(r"[\s_-]+".join(re.escape(p) for p in parts))
    if not entries:
        return None
    alternatives = "|".join(sorted(entries, key=len, reverse=True))
    return re.compile(r"(?<![a-z])(?:%s)(?![a-z])" % alternatives)


def withheld() -> bool:
    """A pull request run that GitHub gives no secrets: from a fork, or from Dependabot."""
    if os.environ.get("GITHUB_EVENT_NAME") != "pull_request":
        return False
    if os.environ.get("GITHUB_ACTOR") == "dependabot[bot]":
        return True
    try:
        with open(os.environ["GITHUB_EVENT_PATH"], encoding="utf-8") as f:
            event = json.load(f)
    except (KeyError, OSError, ValueError):
        return False
    head = ((event.get("pull_request") or {}).get("head") or {}).get("repo")
    base = (event.get("repository") or {}).get("full_name")
    return not head or head.get("full_name") != base


def offsets(names: Pattern[str], text: str) -> Iterator[int]:
    """Where a name starts, as is and with camelCase humps split; no newline is added,
    so a line number counted in either view is the line in the text."""
    for view in (text.lower(), HUMP.sub(r"\1 \2", text).lower()):
        for m in names.finditer(view):
            yield view.count("\n", 0, m.start()) + 1


def members(name: str, data: bytes) -> Iterator[Tuple[str, bytes]]:
    """The files inside an archive, nested archives opened in turn; a plain file as is."""
    try:
        if data[:4] == b"PK\x03\x04":
            with zipfile.ZipFile(io.BytesIO(data)) as z:
                inner = [(i.filename, z.read(i)) for i in z.infolist() if not i.is_dir()]
        elif TAR.search(name):
            with tarfile.open(fileobj=io.BytesIO(data), mode="r:*") as t:
                inner = []
                for m in t.getmembers():
                    f = t.extractfile(m) if m.isfile() else None
                    if f is not None:
                        inner.append((m.name, f.read()))
        elif data[:2] == b"\x1f\x8b":
            yield from members(re.sub(r"\.gz$", "", name), gzip.decompress(data))
            return
        else:
            yield name, data
            return
    except (zipfile.BadZipFile, tarfile.TarError, OSError, EOFError):
        yield name, data
        return
    for inner_name, inner_data in inner:
        yield from members("%s!%s" % (name, inner_name), inner_data)


class Check:
    def __init__(self, names: Optional[Pattern[str]], unserved_everywhere: bool) -> None:
        self.names = names
        self.unserved_everywhere = unserved_everywhere
        self.findings: List[str] = []
        self.files = 0
        self.commits = 0

    def named(self, text: str) -> bool:
        return self.names is not None and any(True for _ in offsets(self.names, text))

    def label(self, top: int, path: str) -> str:
        """The path to print: as is, unless it holds an internal name itself."""
        if not self.named(path):
            return path
        parts = path.split("!")
        if self.named(parts[0]):
            return "<argument %d>" % top
        shown = parts[0]
        for depth, part in enumerate(parts[1:], 1):
            shown += "!" + ("<member at depth %d>" % depth if self.named(part) else part)
        return shown

    def hit(self, where: str, what: str) -> None:
        self.findings.append("%s: %s" % (where, what))

    def scan(self, top: int, path: str, data: bytes) -> None:
        self.files += 1
        shown = self.label(top, path)
        if shown != path:
            self.hit(shown, "a file name holds an internal name")
        text = data.decode("utf-8", errors="replace")
        lines = set()
        if self.names is not None:
            lines.update(offsets(self.names, text))
            for m in B64.finditer(text):
                token = m.group(0)
                try:
                    decoded = base64.b64decode(token + "=" * (-len(token) % 4), validate=True)
                except (binascii.Error, ValueError):
                    continue
                if self.named(decoded.decode("utf-8", errors="replace")):
                    lines.add(text.count("\n", 0, m.start()) + 1)
            if b"\0" in data and not lines:
                # The strings of a compiled assembly, at either byte alignment.
                for view in (data, data[1:]):
                    if self.named(view.decode("utf-16-le", errors="ignore")):
                        self.hit(shown, FOUND)
                        break
        for line in sorted(lines):
            self.hit("%s:%d" % (shown, line), FOUND)
        for kind, pattern in PLAIN:
            for m in pattern.finditer(text):
                self.hit("%s:%d" % (shown, text.count("\n", 0, m.start()) + 1), kind)
        if self.unserved_everywhere or DESCRIBES.search(path.split("!")[-1]):
            for m in UNSERVED.finditer(text):
                where = "%s:%d" % (shown, text.count("\n", 0, m.start()) + 1)
                self.hit(where, "a booster production does not serve yet")

    def scan_commits(self) -> bool:
        def git(*args: str) -> subprocess.CompletedProcess:
            return subprocess.run(["git", *args], capture_output=True, text=True)

        if git("rev-parse", "--is-shallow-repository").stdout.strip() != "false":
            print("::error::--commits needs the full history (actions/checkout with fetch-depth: 0)")
            return False
        since = git("describe", "--tags", "--abbrev=0", "--match", "v*", "HEAD~1")
        span = "%s..HEAD" % since.stdout.strip() if since.returncode == 0 else "HEAD"
        log = git("log", "--format=%H%x00%B%x1e", span)
        if log.returncode != 0:
            print("::error::git log failed for the commits to check")
            return False
        for record in log.stdout.split("\x1e"):
            if "\x00" not in record:
                continue
            sha, message = record.strip("\n").split("\x00", 1)
            self.commits += 1
            if self.named(message):
                self.hit("commit %s" % sha[:12], FOUND)
        return True


def tracked() -> List[str]:
    out = subprocess.run(["git", "ls-files", "-z"], capture_output=True, check=True).stdout
    return [p for p in out.decode("utf-8", errors="surrogateescape").split("\0") if p]


def main(argv: List[str]) -> int:
    flags = {"--tracked", "--commits", "--unserved-everywhere", "--names-optional"}
    given = [a for a in argv if a in flags]
    args = [a for a in argv if a not in flags]
    if any(a.startswith("--") for a in args) or not (args or given):
        print(
            "usage: check_names.py [--tracked] [--commits] [--unserved-everywhere] [--names-optional] [PATH...]",
            file=sys.stderr,
        )
        return 2

    names = compile_names(os.environ.get(ENV, ""))
    if names is None and withheld():
        print("::notice::%s is withheld from this pull request run: internal names not checked" % ENV)
    elif names is None and "--names-optional" in given:
        print("::notice::%s is not set here: internal names not checked (the release check needs it)" % ENV)
    elif names is None:
        print("::error::%s is empty or not set: the internal-name check cannot run" % ENV)
        return 1

    check = Check(names, "--unserved-everywhere" in given)
    paths: List[str] = tracked() if "--tracked" in given else []
    for top, arg in enumerate(args, 1):
        if not os.path.exists(arg):
            print("::error::%s does not exist" % check.label(top, arg))
            return 1
        if os.path.isdir(arg):
            for root, dirs, files in os.walk(arg):
                dirs.sort()
                paths.extend(os.path.join(root, f) for f in sorted(files))
        else:
            paths.append(arg)
    if (args or "--tracked" in given) and not paths:
        print("::error::no file to check")
        return 1
    for top, path in enumerate(paths, 1):
        if os.path.isfile(path):
            with open(path, "rb") as f:
                for name, data in members(path, f.read()):
                    check.scan(top, name, data)
    if "--commits" in given and names is not None and not check.scan_commits():
        return 1

    for finding in check.findings:
        print("::error::%s" % finding)
    print("%d files and %d commit messages checked, %d findings" % (check.files, check.commits, len(check.findings)))
    return 1 if check.findings else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
