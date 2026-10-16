# Real Heli/FLUI dependency-epoch acceptance

2026-10-15, native Windows, Cargo/rustc 1.99.0, NTFS. This extends the
[two-package experiment](dependency-epoch-spike.md) to two real repositories
and four detached worktrees. It tests an execution mechanism; automatic
dependency isolation is not yet a production Heli feature.

## Reproducible workload

The committed [harness](../../experiments/dependency-epoch-spike.py) extends
the existing multi-project public-CLI experiment. It:

1. Creates two Heli and two FLUI worktrees at explicit commits, with hooks
   disabled for worktree management and separate writable output directories.
2. Reads ordinary registry files from a specified source Cargo home, checks
   each archive against Cargo.lock, and copies just the required archives and
   sparse-index entries into a new laboratory publisher home. It never opens
   or acquires the user's package-cache locks.
3. Runs offline, locked `cargo vendor --sync` for the complete two-workspace
   dependency union. Each consumer receives its own small Cargo home and the
   same read-only directory sources. Different repository toolchain selections
   are resolved before replacing Cargo home and retained explicitly.
4. Deliberately holds only the laboratory default home's `.package-cache`
   lock. A same-snapshot `cargo metadata --frozen` control must wait there.
5. Starts Heli check, the complete Heli suite, FLUI foundation tests, and a
   later FLUI foundation check through the frozen public Heli binary. Compiler
   events identify actual compiler invocations, rather than just overlapping
   client lifetimes. GC runs during the workload.
6. Builds the real FLUI xtask, tests its held-host-lock integration, runs
   repeated generated nested-Cargo workloads, and checks idle GC convergence.
7. Verifies that the control remains blocked through the workload, releases
   it, and requires successful completion. Source inventories and lockfiles
   must remain unchanged.

For an initial run, use a new output directory and a frozen binary:

```powershell
python experiments/dependency-epoch-spike.py `
  --heli E:/heli/target/epoch-tools-4c959b2/cargo-heli.exe `
  --heli-ref 4c959b2 `
  --flui-ref 2bb8689518ea68f2cc67e3d8a5c9dfabfbd971b4 `
  --output E:/heli/target/epoch-NEW-ID `
  --full-heli-suite --keep-worktrees
```

To reuse an already published laboratory snapshot, add
`--reuse-epoch-from <prior-lab>/manifest.json`. The harness compares its complete
inventory and requires unchanged lockfile graphs; it does not make a second
copy of the snapshot. Outputs and writable homes remain fresh.

## Confirmed defects and controls

The initial full run at `target/epoch-1015-real-2` used Heli source
`b3c68ee64de8e6a6c3ffd36834e98cfdfac852c5`. All four real agents entered
compiler intervals, including four-way and cross-repository overlap. No agent
log contained a Cargo file-lock wait message. Both FLUI commands passed, as did
xtask and the nested workloads. The final artifact survey was 2,069,670,407
bytes against a 2,147,483,648-byte budget.

That run is **not a pass**:

- Four assertions in `results` and `single_flight` failed because their fake
  toolchains inherited the experiment's real absolute `RUSTC`. Their fake
  PATH compiler no longer supplied the version being tested. Both fixtures
  now explicitly remove that override while real commands keep theirs.
- The reused instrumentation helper's control lock timed out after 120
  seconds. The control therefore did not remain blocked through the full
  suite. The helper now accepts an explicit timeout; the epoch harness gives
  it the workload timeout plus 600 seconds. The failed result is retained.
- FLUI tests reached their first observed compiler at 11.526 seconds, above
  the existing experiment's ten-second target. Zero cache-wait messages do
  not establish a universal cold-start latency bound. This result is retained
  rather than removing the latency criterion.

A separate public-CLI reproducer confirmed a production cache defect: modifying
a transitive Cargo configuration include returned cached exit 0 instead of
the actual compile error, exit 101. Commit `4c959b2` keys those include graphs,
including optional absence and inline/file-relative origins, declines incomplete
inventories, and invalidates older cache composition markers. DESIGN.md records
the contract. All 31 `results` tests and all ten `single_flight` tests passed
with an explicitly selected real compiler; all-target strict Clippy and fmt
also passed. Red/green logs are under `target/include-key-fix/`.

## Corrected full run

The corrected run **passed every acceptance check**. It is
`target/epoch-1015-real-3`, using Heli source `4c959b2`, a separately copied
binary, the same FLUI commit, fresh output directories and the verified snapshot
from the initial run. Binary SHA-256:
`4f07a30d80a5646b670fc0b6339971189ca0145c741c25b4483fbece5debee5c`.

| Check | Executed result |
|---|---|
| Complete Heli suite | 354 passed, 0 failed, 0 ignored across 36 test targets; outer command exit 0 in 479.7 s |
| Selected FLUI foundation tests | 18 passed; check also exited 0 |
| All ten managed workload commands | Exit 0, including real xtask and four nested Cargo runs |
| Compiler intervals | Four distinct real agents overlapped, including across repositories |
| First observed compiler | Heli check 1.403 s; Heli suite 1.449 s; FLUI tests 1.857 s; later FLUI check 1.369 s |
| Cargo file-lock wait messages in agent logs | 0 |
| Default laboratory home control | Blocked before and after the entire workload; exited 0 only after explicit release |
| Shared vendor inventory and two lockfiles | Unchanged before/after |
| Idle artifact GC | 2,070,633,313 accounted bytes against 2,147,483,648 |
| Minimum available physical RAM / commit | 34,223,247,360 / 24,383,905,792 bytes |

This repeated run reused an already scanned snapshot, so its startup timings
are not cold-publication measurements or a controlled speed comparison.
Compiler instrumentation adds overhead. Presence of a Heli run ID in an event
does not prove that every intentionally isolated test fixture inside the suite
shares the outer pool: the suite deliberately tests independent pools too.

Detailed evidence remains in the laboratory's `RESULTS.md`, `result.json`,
`dependency-epoch.json`, `compiler-events.json`, `manifest.json`, per-command
logs and GC surveys. The original failed run is retained separately.

## Storage and integration limits

The complete source union contains 736 registry package/version pairs,
39,237 files and 996,288,182 logical source bytes. Setup copied 128,249,130
archive bytes; after Cargo unpacked them, the publisher home occupied
1,160,333,281 logical bytes. Those setup copies are distinct from the shared
snapshot and must be accounted for; they are not small per-agent homes.
In the initial run the four real consumer homes each held 57,668 bytes and
no copied registry payload; in the corrected run they each held 57,666 bytes.

After the first experiment completed, its four clean worktrees were retired
through Git and its own artifact store reclaimed through public `heli gc`.
A fresh survey accounted for 768 remaining metadata bytes. Its private
publisher setup cache (1,160,333,428 bytes including the later config file) was
also removed after checking its exact owned path and absence of reparse points.
The shared snapshot, original user cache and all evidence were retained; the
corrected experiment continued using that snapshot. See the first laboratory's
`cleanup-*.json`, `retired-worktrees.json` and `publisher-cleanup.json`.

The two-GiB experiment budget covers Heli's configured artifact store and
registered target directories. It does not cover vendor sources, publisher
setup copies, checkout sources, unrelated host caches or every transient
directory used internally by the full suite. Report those scopes separately.
Available RAM/commit measurements show observed headroom, not enforced caps.

This is the full **Heli** suite, plus the specified FLUI package tests/check and
xtask probe; it is not a repeat of FLUI's whole workspace suite. That separate
validation is recorded in the earlier multi-project evidence. This experiment
does not migrate the original user-home configuration/credentials, implement
automatic snapshot publication/leases, validate git/alternate registries, or
enable selective runtime artifact GC. The production global runtime guard
continues protecting unenrolled consumers.

[Package-granular junction views](package-epoch-views.md) demonstrate a separate
way to avoid copying a full vendor directory for every epoch. They still need
a production ownership protocol before object reclamation is safe.
