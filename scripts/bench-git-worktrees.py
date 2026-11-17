#!/usr/bin/env python3
"""Create disposable, deterministic Git worktrees for read-path benchmarks."""

import argparse
from io import BytesIO
import os
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("destination", type=Path, help="New directory for the benchmark fixture")
    parser.add_argument("--worktrees", type=int, default=20, choices=range(1, 21), metavar="1..20")
    options = parser.parse_args()
    destination = options.destination.resolve()
    destination.mkdir()
    repo = destination / "Harbor"
    repo.mkdir()
    environment = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    environment.update(GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1")

    def git(path, *arguments, payload=None):
        subprocess.run(["git", "-C", str(path), *arguments], env=environment, input=payload, check=True)

    stream = BytesIO()

    def line(value):
        stream.write(value.encode() + b"\n")

    def data(value):
        raw = value.encode()
        line(f"data {len(raw)}")
        stream.write(raw + b"\n")

    git(repo, "init", "-q", "-b", "main")
    for index in range(650):
        line("commit refs/heads/main")
        line(f"committer Fixture <fixture@example.invalid> {1_600_000_000 + index} +0000")
        data(f"Change {index}")
        if index == 0:
            for number in range(4000):
                line(f"M 100644 inline files/{number // 100:02}/{number:04}.txt")
                data(f"Tracked fixture file {number}\n")
        else:
            line("M 100644 inline counter.txt")
            data(f"Fixture counter {index}\n")
        line("")
    line("done")
    git(repo, "fast-import", "--quiet", payload=stream.getvalue())
    git(repo, "read-tree", "--reset", "-u", "HEAD")
    git(repo, "repack", "-adq")

    paths = [repo]
    for index in range(1, options.worktrees):
        path = destination / f"Harbor-{index:02}"
        git(repo, "worktree", "add", "-q", "-b", f"feature/bench-{index:02}", str(path), "HEAD")
        paths.append(path)
    for index, path in enumerate(paths):
        relative = "files/00/0000.txt"
        selected = path / relative
        selected.write_bytes(selected.read_bytes() + b"Staged content\n")
        git(path, "add", "--", relative)
        selected.write_bytes(selected.read_bytes() + b"Working content\n")
        for note in range(3):
            (path / f"untracked-{note}.txt").write_text(f"{index}:{note}\n")

    (destination / "paths.txt").write_text("".join(f"{path}\n" for path in paths))
    print(f"{len(paths)} worktrees in {destination}")


if __name__ == "__main__":
    main()
