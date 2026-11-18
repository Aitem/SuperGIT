# Git read-path probe

SuperGIT's default build uses Git CLI for repository reads and writes. This
experiment compares a vendored libgit2 reader for the History ref snapshot and
sidebar status polls. A probe build requires both the Cargo feature and the
runtime setting. Staging, committing and other operations use the CLI,
including their validation reads.

```sh
cargo build --release --locked -p supergit --features git2-probe
SUPERGIT_GIT_BACKEND=git2 target/release/supergit /path/to/repo
```

The probe uses Git CLI when an environment override changes repository or
config resolution, libgit2 refuses the repository, or status involves filters,
gitlinks, ambiguous renames, unmerged entries, sparse or split indexes, or
nonstandard history. A global Git LFS filter triggers CLI status. In a probe
build, History can still use the libgit2 ref reader in this environment.

## Measurements

Apple Silicon, Git 2.50.1 (Apple Git), libgit2 1.9.7, Rust 1.100. These
numbers measure read operations. The History view's commit walk, patch parsing,
and app rendering have their own costs.

Both status fixtures have 650 commits and 4,001 tracked paths per checkout,
with mixed staged and working changes and three untracked files. The first
starts from a local clone of an existing fixture. The second starts from a
fresh checkout created by the script below. The benchmark opens the library
repository each time and runs the worktrees concurrently like the sidebar.
It compares the entire status model against Git CLI on every sample. Both
backends use repository-local configuration for the comparison.

These results use 30 warm samples with alternating backend order. Values are
p50 and p95 in milliseconds.

| Worktrees | Local clone CLI | Local clone libgit2 | Fresh checkout CLI | Fresh checkout libgit2 |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 57.04 / 59.37 | 17.77 / 18.82 | 19.35 / 20.31 | 17.76 / 19.22 |
| 8 | 428.25 / 443.31 | 49.15 / 53.92 | 68.43 / 72.50 | 49.49 / 59.90 |
| 20 | 1,376.95 / 1,480.35 | 127.51 / 172.57 | 166.04 / 176.40 | 126.11 / 150.40 |

For the History cache key, a separate fixture with 650 commits and eight
worktrees produced p50 of 11.338 ms via CLI and 0.581 ms via libgit2, with p95
of 11.714 and 0.602 ms. This measures one selected worktree over 125 warm
samples. Parity tests cover packed refs, chained symbolic refs, linked
worktrees, and detached or unborn HEAD. Status tests cover an independent
index, partial edits, an upstream, and fallback for gitlinks and ambiguous
renames.

### Full application build

Sequential clean release builds used the same locked dependencies and warm
OS file caches. The feature adds four Cargo packages. Size measurements
use the linked executable and its gzip archive at level 6.

| Metric | CLI | With libgit2 probe |
| --- | ---: | ---: |
| Clean build elapsed | 175.81 s | 165.15 s |
| Clean build user + system CPU | 903.02 s | 939.90 s |
| Executable | 33,786,992 B | 34,693,168 B |
| Gzip | 11,975,470 B | 12,376,073 B |
| Signed macOS app ZIP | 12,146,625 B | 12,541,884 B |

The probe added 0.86 MiB to the executable and 0.38 MiB to gzip. Its CPU cost
in this pair was about 4% higher. A separate run also finished sooner with
the probe, which ran second both times. Elapsed time depends on build order
and machine load. The ZIP measurement used the same app resources and plist,
ad-hoc signing, `codesign --verify --strict`, and `ditto -c -k --keepParent`.
The signed archive grew by 395,259 bytes (3.3%).

### Linux arm64 release

Debian Bookworm in the official Rust 1.100 arm64 Docker image, Git 2.39.5,
four Cargo jobs and empty target directories. Dependencies were fetched
before timing. The first pair built CLI first, the second built libgit2
first. Each pair built the same locked checkout with the feature toggled.

| Metric | CLI | With libgit2 probe |
| --- | ---: | ---: |
| Pair 1 elapsed | 258.81 s | 399.02 s |
| Pair 1 user + system CPU | 990.34 s | 1,497.00 s |
| Pair 2 elapsed | 271.95 s | 308.30 s |
| Pair 2 user + system CPU | 1,045.77 s | 1,188.33 s |
| Executable, pair 2 | 67,107,888 B | 68,199,712 B |
| Gzip, pair 2 | 23,073,110 B | 23,566,699 B |

The observed Linux size increases were 1.04 MiB for the executable and
0.47 MiB for gzip. `ldd` showed one additional system library, `libz.so.1`.
The second pair added 36.35 s elapsed and 142.56 s CPU, about 13–14%.
The larger first-pair difference coincided with other builds on the host.
Use these timings as observations from one host and architecture.

The default stays with CLI. Status gains vary with checkout conditions, the
fresh 20-worktree fixture improved p95 by about 15%, and a global LFS filter
uses the CLI fallback. The History ref lookup is a smaller operation with
consistent savings, and can be assessed independently of wider status support.

## Reproduce

Create a new fixture directory and build the release benchmark examples.

```sh
python3 scripts/bench-git-worktrees.py /tmp/supergit-bench
cargo build --release --locked -p sg_git --features git2-probe --examples

GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 \
  target/release/examples/measure_status compare /tmp/supergit-bench/Harbor /tmp/supergit-bench/Harbor-*

target/release/examples/measure_refs /tmp/supergit-bench/Harbor
SUPERGIT_GIT_BACKEND=git2 \
  target/release/examples/measure_refs /tmp/supergit-bench/Harbor
```

Choose the worktree paths passed to the status example to measure a subset.
The fixture layout and changed files are deterministic. Store each clean
release build in its own empty Cargo target directory and run the two
variants sequentially. Measure package size from the executable or archive
generated by the same profile on each build.
