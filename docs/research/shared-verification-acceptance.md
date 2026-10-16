# Shared verification acceptance — 2026-10-16

The repair covers duplicate checks requested by independent agents, including
agents reading the same worktree to plan their work. It also makes build
attachment source-aware while preserving artifact ownership.

## Frozen inputs and verification

- Heli code: `7326ac9d00229b46ace8bf1a6f85f1281a88ab08`.
- Release SHA-256: `d00c2df85961ecc681d3766deb30c4bc257d1a28fde693a4edc0d0d283932d66`.
- FLUI: `bf23bf57f8d21d1ee33055b63694f690885569d3`; its change normalizes
  nonempty `HELI_RUN` values in xtask's key while retaining managed versus
  standalone execution and all other keyed settings.
- Heli: `cargo clippy --all-targets --locked --offline -- -D warnings` and
  `cargo test --locked --offline --no-fail-fast`: **393 tests passed**.
- FLUI: the complete xtask suite and `cargo xtask check-changed --base
  d85ab1e73575388ba8112c1d86dbc7dd95491717`: **61 tests passed**, including
  formatting and Clippy in the scoped gate.

## Confirmed defects and public regressions

Each behavior fix retained a failing reproducer before its production change.
The checked-in regressions are in `tests/single_flight.rs`, `tests/results.rs`,
and FLUI's `tools/xtask/src/verified.rs`.

| Defect | Repair and regression evidence |
|---|---|
| Distinct Heli run IDs invalidated unchanged xtask verdicts | Normalize only nonempty `HELI_RUN`; test changed IDs, absent/empty values, other Heli settings and platform case rules |
| Cache-off checks ran twice | New `Live` wire policy keys sources and shares live work, without saved-result reads, writes or invalidation; test repetition, failure propagation, twin worktrees and failed inventory |
| Ordinary wrapper attachment ignored source changes | Explicit `exec --verify` uses a complete verification key and preserves wrapper argv/output; test changed sources and environment |
| Slow inventory triggered the client's five-second direct fallback | Acknowledge key preparation with `Preparing`; a gated six-second metadata reproducer no longer starts an unmanaged command |
| A new source generation overwrote a running Windows test executable | Queue differing keyed source generations in one worktree; real Cargo direct/wrapped tests execute the old and new revisions successfully |
| Rekeying after a formatter changed blocker relationships | Freeze admission source snapshots; the formatter/rekey regression retains the original barriers |
| A live-only follower inherited a queued cached owner's saved verdict | Follow cached owners only after actual execution starts; reproduce a twin's saved pass arriving during the queue |
| Build attachment returned an earlier source generation | Compile uses live keys with worktree/output ownership; test edits, features and all three Cargo output environment selectors |
| Sharing builds across owners could omit requested artifacts | Real Cargo test requires independent worktrees to get their own `.rlib`, while two clients of one owner share a run |

The explicit `--cache off` policy differs from `--no-cache`: the latter
retains force-new verification with recording. Legacy disabled wire requests
retain their old no-sharing contract. Missing complete source inventory
disables sharing rather than inventing a source identity.

## Two-worktree FLUI comparison

Every mode used the same two pinned worktrees, offline packages, ordinary
checkout-local targets and a separately copied warm registry. Targets began
empty per mode. Each mode used **one shared laboratory Cargo home for its
two agents**, so manual per-worktree home isolation earns no credit as Heli
behavior. Incremental compilation was disabled; user Cargo configuration and
native xtask parallelism defaults were retained. Heli's pool had 32 slots.

The changed-scope gate used base
`8c48ac7a3099837781d509558d6b0ccbcaf8f31e`, then repeated the exact command.
The native nested group selected 11 tests across 94 binaries. Platform and
feature skips remained explicit in the output. Heli used `--cache off exec
--verify`; its persistent result store was disabled while xtask caching stayed on.

Times measure earliest client submission to latest client exit, in seconds.

| Phase | Stock xtask lock | Native lock disabled | Heli live sharing |
|---|---:|---:|---:|
| Cold changed-scope gate | 728.9 | 505.5 | 334.7 |
| Exact cached repeat | 6.7 | 7.2 | 7.7 |
| Native nested group | 616.6 | 536.9 | 304.4 |

**All 18 client commands exited 0.** Every repeat reported an xtask cache hit.
The cold gate passed 106 tests with 7 explicit skips, and every executed
nested group passed all 11 selected tests. Heli's client pairs followed
`r5`, `r6` and `r7`, respectively: the second client streamed the actual
owner's output. No duplicate pipeline was needed for that verification.
Ordinary unlocked Cargo had 382.854 s of overlapping successful compiler
lifetimes in the cold gate and 481.600 s in nested tests. These intervals
are compiler lifetimes, not CPU time. Heli's zero cross-worktree compiler
overlap here means deduplication, not serialization of two needed pipelines.

The shared-home unlocked mode printed 41 package-cache lock messages; no
such messages appeared in the measured Heli output. This establishes no
general absence of Cargo locks: Heli executed one verification pipeline,
and passed-test output can hide waits inside fixtures.

Before cleanup, each native pair held about 29.4 GiB of target artifacts;
Heli held about 14.7 GiB in its one actual verification owner. This difference
comes from avoided duplicate work, not GC eviction or recycling. Four active
GC surveys reported `runtime_busy=true` and zero victims. The idle survey
reported `runtime_busy=false`. Its budget was 1 TiB with default eviction
ages: there was no pressure and no hard-quota claim. Minimum system-available
RAM was 22.3 / 20.9 / 27.2 GiB for locked / unlocked / Heli. System samples
include unrelated activity and are not per-process memory limits.

A single fixed-order Windows experiment with warm filesystem caches does
not establish a universal speedup or full FLUI CI parity. Linux/macOS were
not exercised locally. Heli adds startup/inventory overhead; the already
cached repeat was slightly slower than the native controls.

## Same-worktree counter and remaining boundaries

A separate standard-library-only Cargo fixture used the same frozen release.
All 12 clients exited 0. Native identical tests executed twice; direct and
wrapped Heli live-only pairs executed once. After editing the tracked revision,
three clients executed exactly two revisions, `one` then `two`; the new
generation waited for the old executable before rebuilding on Windows.
Checked-in regressions reproduce this behavior with real Cargo too.

Cargo already serializes one artifact directory and skips up-to-date builds.
For repeated `build`, Heli shares the live command and prevents attachment to
older sources; it does not claim a compilation saving where Cargo already
does none. Different packages/features/outputs remain separate requests.
Different worktrees must build their own artifacts. Distinct commands using
the same target may still wait for Cargo's artifact lock.

Concurrent editors still share files in one worktree. Heli does not create
immutable source snapshots; independent editing agents need separate
worktrees. Source-generation waits protect keyed runs and do not remove the
documented nested-run bypass needed to avoid parent/child deadlocks.
Shared Cargo homes retain resolver/download contention. Explicit dependency
epochs remain the production opt-in source-isolation profile; automatic safe
setup is unfinished. GC deliberately defers collection during managed runtime,
so continuous activity can exceed a soft disk budget.

## Retained local receipts and excluded preparation

The matching comparison and audit are retained at
`E:\heli\target\xfixshared1016`: `results.json`, `analysis.json`,
`verification.json`, `REPORT.md`, timestamped logs, compiler events and system
samples. Both worktrees finished clean and pinned; original source/config
digests matched. The owned workload/daemon roots were gone after cleanup.
The independent release counter is in
`E:\heli\target\same-worktree-frozen-1016\results.json`; gate and red/green
receipts are in `E:\heli\target\shared-verification-state`.

The first preparation, `E:\heli\target\xfix1016`, was rejected before any
measured workload: it assumed `[cargo] home="isolated"` from an earlier
experimental branch, absent in current production. It is retained as an
excluded setup failure. The previous `xdef1016` Heli arm used that prototype
and is not a matched before/after throughput control for this repair. The
new comparison uses the reviewed `xtask-shared-fixed-ab-20261016.py` and
`xtask-shared-protocol-20261016.py` with shared homes in every arm.
