# Shared dependency epoch with private Cargo homes: measured spike

2026-10-15, Windows 11, Cargo 1.99.0
(`5f94df4789f005f9a352888e8355ffc645b7ed0e`), MSVC host toolchain.

**The mechanism works in this small experiment.** Two fresh Cargo homes used
one frozen vendor directory and completed real `cargo check --frozen` commands
while an exclusive package-cache lock blocked the identical vendored check in
the laboratory's default Cargo home. This is evidence for an optional dependency
epoch design, not a production heli implementation or a workspace-scale speedup.

## Isolation and procedure

Everything written was inside the newly created laboratory
`E:\heli\target\dependency-epoch-spike-20261015-2212`. No user's Cargo cache lock
was opened or acquired. The setup copied only the existing `itoa 1.0.18` and
`ryu 1.0.23` sparse index entries, registry `config.json`, and `.crate` archives
from the user's cache into the private `default-home` directory. The originals
were read as ordinary files. No network access was needed.

Two independent local Rust projects each call both dependencies. Each project
has its own lockfile, target directory and explicit source replacement:

```toml
[source.crates-io]
replace-with = "epoch-1"

[source.epoch-1]
directory = "E:/heli/target/dependency-epoch-spike-20261015-2212/vendor-epoch-1"
```

The actual toolchain Cargo and rustc executables were selected explicitly,
bypassing rustup shims; changing `CARGO_HOME` therefore did not require copying
installed toolchains. `CARGO_INCREMENTAL=0` was set, inherited jobserver/wrapper/
target overrides were cleared, and all build output stayed in laboratory targets.

The setup ran `cargo generate-lockfile --offline` and then
`cargo vendor --offline --locked <epoch-directory>` against the private home.
Vendor files were marked read-only before the positive controls. These attributes
are an operational freeze, not a security boundary against the owning user.

A tiny standalone Rust helper exclusively locked only
`<lab>/default-home/.package-cache` using `std::fs::File::lock`. Cargo 1.99 uses
the same file-lock primitive; the pinned source identifies `.package-cache` as
the DownloadExclusive lock, distinct from the mutation/shared lock.
[Cargo cache locks](https://github.com/rust-lang/cargo/blob/5f94df4789f005f9a352888e8355ffc645b7ed0e/src/util/cache_lock.rs),
[Cargo file locks](https://github.com/rust-lang/cargo/blob/5f94df4789f005f9a352888e8355ffc645b7ed0e/src/util/flock.rs).

## Executed results

| Control | Actual observation |
|---|---|
| Default laboratory home, vendored `check --frozen` | Still running after 1.5 s; stderr reported `Blocking waiting for file lock on package cache` |
| Project A, fresh agent-A Cargo home, same vendor epoch | Exit 0 in 0.348 s while the default-home lock remained held |
| Project B, fresh agent-B Cargo home, same vendor epoch | Exit 0 in 0.319 s while that lock remained held |
| Default-home check after both agents completed | Still blocked; exit 0 only after the helper released the lock; total 2.349 s |
| Change one vendored `itoa/src/lib.rs`, then fresh-target frozen check | Exit 101 in 0.112 s, reporting the file's expected and actual checksum |

Agent checks were performed sequentially to minimize host load. Each used a fresh
target and compiled both registry dependencies; success was not merely a warmed
artifact lookup. The mutated file was restored and marked read-only after the
negative control. The checksum failure demonstrates detection, not safe mutation
during active builds. Cargo documents vendoring and explicit source replacement
as supported mechanisms and treats vendored sources as read-only inputs.
[Cargo vendor](https://doc.rust-lang.org/cargo/commands/cargo-vendor.html).

Logical file bytes, excluding target artifacts and filesystem allocation overhead:

| Component | Bytes |
|---|---:|
| Shared vendor epoch, 52 files | 238,924 |
| Agent-A writable Cargo home | 57,521 |
| Agent-B writable Cargo home | 57,521 |
| Setup-only private seeded home | 392,032 |

Each agent home contained a 57,344-byte `.global-cache` database, a 177-byte
registry tag, and empty package-cache lock files. Neither home contained copied
registry source trees or archives. The shared source payload was stored once.
This measures a tiny dependency graph; it does not predict large-workspace home
database sizes or total disk use, and compiled outputs remain per project.

## Reproduction artifacts

- Harness: [run.py](../../target/dependency-epoch-spike-20261015-2212/run.py).
- Complete commands, environments' selected Cargo homes, exit codes and timings:
  [results.json](../../target/dependency-epoch-spike-20261015-2212/results.json).
- [Blocked baseline stderr](../../target/dependency-epoch-spike-20261015-2212/baseline.stderr),
  [agent-A result](../../target/dependency-epoch-spike-20261015-2212/agent-a-frozen.json),
  [agent-B result](../../target/dependency-epoch-spike-20261015-2212/agent-b-frozen.json),
  [checksum negative control](../../target/dependency-epoch-spike-20261015-2212/mutated-snapshot.json).

Executed command:

```powershell
python E:\heli\target\dependency-epoch-spike-20261015-2212\run.py
```

The harness intentionally refuses implicit reuse through fresh-directory setup.
To repeat, copy only `run.py` into a new empty directory under
`E:\heli\target\dependency-epoch-spike-<new-id>` and run that copy. It derives
all writable paths from its own location and never deletes an existing lab.
The earlier one-dependency preliminary control is preserved separately in
`target/dependency-epoch-spike-20261015-2210`.

## What this establishes and leaves open

Private Cargo homes remove the demonstrated resolver-lock dependency on another
home while sharing supported directory sources. Merely vendoring while retaining
one shared Cargo home did not remove that lock. There was no mutable shared
registry cache with independent lock files.

A real integration still needs an explicit publisher/consumer lifecycle: collect
the complete multi-project dependency graph, publish a versioned immutable epoch,
pin active runs to it, and retire it only after its readers finish. Refresh must
publish another epoch rather than edit the active directory. Git dependencies,
alternate registries, credentials/configuration migration, feature/target/toolchain
coverage, Cargo extensions, build-script assumptions, cross-platform operation,
large workspaces and epoch retention were not validated here. Existing metadata
or Cargo invocations must receive the same source replacement and private home;
this experiment does not justify weakening heli's result-cache input inventory.

## Concurrent positive control

A separate fresh laboratory, `target/dependency-epoch-spike-20261015-2216`,
repeated the two-dependency experiment with both real Cargo checks launched
together, while the private default-home DownloadExclusive lock remained held.
Both agent processes were explicitly observed alive after launch. Windows
`GetProcessTimes` supplied their kernel process creation/exit timestamps, avoiding
the ambiguity of measuring exit only when the harness collects buffered output.

| Process | Kernel creation, UTC | Kernel exit, UTC | Kernel lifetime | Exit |
|---|---|---|---:|---:|
| Agent A | 22:15:41.680684 | 22:15:42.140333 | 0.459649 s | 0 |
| Agent B | 22:15:41.683749 | 22:15:42.139985 | 0.456237 s | 0 |

The Cargo process lifetimes overlapped by **0.456237 seconds**. Both checks
compiled `itoa`, `ryu` and their local project into separate previously empty
targets, against the same read-only vendor epoch and distinct writable Cargo
homes. This is concurrent Cargo invocation evidence; compiler-job overlap was
not independently instrumented. The baseline remained blocked after both agents
finished and exited 0 only after the holder released its private cache lock
(2.163403 seconds total). The fresh-target snapshot-mutation control again exited
101 with a checksum error, then restored the snapshot. Snapshot and per-agent
Cargo-home byte counts were identical to the sequential experiment.

The earlier sequential observations above remain separate measurements. The
concurrent run has its own [harness](../../target/dependency-epoch-spike-20261015-2216/run.py),
[complete results](../../target/dependency-epoch-spike-20261015-2216/results.json),
[agent-A timestamps and output](../../target/dependency-epoch-spike-20261015-2216/agent-a-parallel-frozen.json),
[agent-B timestamps and output](../../target/dependency-epoch-spike-20261015-2216/agent-b-parallel-frozen.json),
and [baseline wait log](../../target/dependency-epoch-spike-20261015-2216/baseline.stderr).
An initial concurrent harness run is also preserved in the `-2215` laboratory;
its output-collection timestamps were not used for the kernel overlap result.
