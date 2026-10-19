# Acceptance records, October 2026

Dated evidence gathered while heli was built: the measurements that
motivated it, the FLUI validations and the acceptance runs. The behaviour
they check is described in [the architecture](../architecture.md).

## The problem, measured 2026-10-13

heli is not tied to one project. The numbers below come from the host and
workspace that motivated it, and serve as the worked example throughout:
Windows 11, 32 logical CPUs, 64 GB RAM; the flui workspace (39 members, 462
third-party packages on the host platform); 14 agent worktrees. Measured
2026-10-13.

FLUI is one motivating consumer. The user's workload spans many projects
with the same contention and disk-growth problem, currently spread over
multiple PCs. Acceptance therefore needs simultaneous runs from different
repositories on one host, as well as multiple worktrees of one repository.
Repository opt-in for build placement ([placement](../build-store.md#placement)) does not partition the shared
CPU/memory pool or give each repository an independent disk budget.

| Symptom | Evidence | Cause |
|---|---|---|
| Runs wait 2-3 h | flui's own host lock: one exclusive heavy run per user | Mutual exclusion: a run waits for every run ahead of it, whatever their size. One workspace build peaks at 3.9 GB with 8 jobs, so the machine idles while the queue grows. |
| Memory still runs out | Running beside the lock: a plain `cargo nextest run -p …`, another project's `cargo test`, one rustc at 4.3 GB | The lock covers only the project's task runner. Every other cargo sizes its own pool to all 32 CPUs, and nested cargo inside tests (generated projects, trybuild) does it again. |
| Disk fills | 592 GB of `target/` across 14 worktrees, 203 GB free on the volume. The largest is 208.8 GB: 114 GB `debug/incremental`, 31 GB `tests/trybuild`, 24 GB nested-build caches, 30 GB `debug/deps` | Nothing reclaims a build directory while its worktree lives. `incremental/` keeps a directory for every unit ever built (each feature set, each `-p` set), and all of it is regenerable. |
| Every new worktree builds cold | a new agent worktree has no `target/` | The build directory lives inside the worktree and dies with it. |
| Work repeats | agents re-run gates on unchanged trees; a tool timeout kills a long run, which is then restarted | Results are not remembered; runs are children of the agent's shell. |

None of the cargo issues that would fix this is close to landing as a whole
([upstream map](../architecture.md#upstream-map) maps each part of heli to the issue it works around).

## Acceptance status, 2026-10-16

The current acceptance priority is independent progress across projects and
worktrees. Disk and RAM caps are secondary to that goal. Their safety
contracts still apply, but an experiment showing concurrency does not by
itself establish a need for disk optimization or prove a resource bound.

G1, G3 and G4 ([goals](../architecture.md#goals-and-non-goals)) are design goals,
not unconditional properties established by the current acceptance runs.
Independent projects can complete beside long-running work ([acceptance runs](#plan-and-acceptance-runs)), but
cold preparation and Cargo's own waits can delay the first compiler. The
recorded full FLUI run took 184.677/210.297 seconds to that point; these
are end-to-end delays, not separately measured download times. That run
does not pass the strict ten-second first-compiler criterion of the
[acceptance runs](#plan-and-acceptance-runs).
An existing worktree's retained home and an eligible orphan build directory
can be reused; an entirely new worktree without a donor is not guaranteed
to start warm. Artifact GC reclaims eligible regenerable data and protects
what runs in flight may use: their workspaces' directories, and whatever
changed since the oldest of them started ([eviction](../build-store.md#eviction)). Protected residual outputs
can exceed the budget even at idle, and native source homes are outside
that budget ([eviction](../build-store.md#eviction), [worktree Cargo homes](../build-store.md#worktree-cargo-homes)).
The recorded 2-GiB FLUI acceptance remains a failed budget check. These
limits must not be advertised as solved merely because concurrent commands
or narrower acceptance tests pass.

## FLUI build-dir validation, 2026-10-15 (#27)

The experiment used a separate, initially clean FLUI worktree at
[`b653a7ad8ddfb17c2f43b9cec8c3b9c56e5284a0`](https://github.com/vanyastaff/flui/commit/b653a7ad8ddfb17c2f43b9cec8c3b9c56e5284a0),
on Windows x86_64 with Rust/Cargo 1.99.0 and cargo-nextest 0.9.146. The
final tested source is recorded in FLUI commit
`a0dcb509ff5b8a1d0f593587c2ce710654a351f2` (the baseline above plus the
Android lint allowance below). The worktree and store were on the same
volume. Every Cargo invocation inherited:

```text
CARGO_INCREMENTAL=0
CARGO_TARGET_DIR=<worktree>/target
CARGO_BUILD_BUILD_DIR=<store>/{workspace-path-hash}
```

`cargo metadata --format-version 1 --no-deps` reported different
`target_directory` and `build_directory` paths, with the latter under the
store and its workspace hash expanded. Child rustc invocations also wrote
their intermediates there, including generated nested workspaces. The
experiment used Cargo directly; it did not measure heli's governor, GC or
recycling. Neither the main FLUI checkout nor user-wide Cargo configuration
was changed.

```text
cargo xtask check-changed --full --no-cache --base 1670492ddaba70e88cbcebc3bec2ac919e37aa97
cargo xtask test --nested --nested-group native
```

The base is a root commit of FLUI's history, chosen to force whole-workspace
scope in a clean worktree. The dry-run explicitly reported
`packages: <whole workspace>`; `--full` alone would still use change scope.
The CLI name `native` selects nextest's `group(nested-cargo)` (generated CLI
projects, external facade consumers and iOS Cargo-artifact fixtures).
Its prescribed test feature graph also enables `flui-devtools/agent`, beyond
the full check-changed workspace test graph.
The host lock stayed enabled, with 28 build jobs and 16 nextest threads;
heavy commands ran sequentially.

**Failure inventory.** The first full invocation passed fmt, both strict
Clippy steps, 59 xtask tests, 981 workspace tests, strict rustdoc and
814 doctests, then stopped at the macOS cross-check: `cargo-zigbuild` was
installed but Zig was missing (`Failed to find zig`). Installing Zig 0.16.0
in an experiment-local tools directory and adding it only to that process's
`PATH` made `cargo xtask cross-typecheck --target aarch64-apple-darwin` pass.
No FLUI source fix was needed for that prerequisite failure. Long-running
trybuild cases were reported as slow, not failed tests; they passed without
changing timeouts.

The next full invocation reached Android Clippy and failed in
`crates/flui-testing/tests/a11y_query.rs`: `missing_const_for_thread_local`
reported an initializer that already used `const { ... }`. A standalone
`RefCell<Option<String>>` thread-local reproduced the same error with
`clippy-driver --target aarch64-linux-android -D warnings`, after unsetting
`CARGO_BUILD_BUILD_DIR`; this is independent of artifact placement and matches
[Clippy #13422](https://github.com/rust-lang/rust-clippy/issues/13422).
The fix applies the existing FLUI Android-only lint allowance to that one
declaration, retaining the const initializer and the accessibility regression
test. The original Android cross-typecheck then passed. Its strict Clippy
invocation is the regression check for this compiler-lint compatibility fix.

**Final full-gate result: exit 0**, with that Android-only allowance:

| Check | Result |
|---|---|
| Workspace fmt and both strict Clippy steps | Passed |
| xtask nextest | 59 passed |
| Workspace nextest | 981 passed, 29 skipped; 13 slow, none failed |
| Hot-reload fixture and lifecycle tests | Built; load/unload/reload tests passed within the workspace run |
| Strict rustdoc | Passed with `-D warnings` |
| Doctests | 814 passed, 264 ignored, none failed |
| Required native cross-typechecks | macOS ARM64, Android ARM64 and Windows x86_64 passed |
| wasm32 workspace Clippy and facade hot-reload check | Passed |
| `flui-platform` compiler guards | 1 passed |

**Separate nested-cargo result: exit 0.** All 11 selected tests passed
(1 slow); 1001 tests were skipped by the configured selection. The Windows
xtask prelude had no member of this group (0 run, 59 skipped); the 11 tests
ran in the workspace stage. Generated basic, empty, widget and counter
projects, facade consumers and iOS Cargo-artifact fixtures passed with the
inherited build directory setting.

**Limits.** This is a Windows-host compatibility experiment, not Linux or
macOS runtime evidence. The full plan omits iOS typechecking without a genuine
Apple SDK and the native `flui-platform` suite that needs Linux/Xvfb.
Ignored tests remain ignored. Coverage instrumentation and an exhaustive
optional-feature matrix are not part of these commands. This evidence does
not justify changing every project's build placement globally: the Q2
decision is repository opt-in, with a scoped environment for nested cargos.

## Adoption in flui (worked example)

1. Run `cargo heli setup`: pool environment for the agents, `build-dir`, a
   store on D:.
2. Update AGENTS.md: `cargo heli nextest run -p <crate>` while iterating, and
   `cargo heli exec -- cargo xtask check-changed` before a PR.
3. In xtask, skip `host_lock` when it runs under heli (`HELI_RUN` is set) or
   when the pool variable is present. The fixed `CARGO_BUILD_JOBS=28` from
   flui#1619 has no effect under a jobserver ([host pool](../host-pool.md)).
4. `cargo xtask worktree prune` keeps handling branches. Deleting `target/` is
   no longer its job.

## Plan and acceptance runs

The Windows acceptance spike on 2026-10-15 used heli `e04452b`, FLUI
`1800e914f`, two repositories and four detached worktrees with GC enabled.
Four agents compiled concurrently; their first actual compiler started in
0.48–1.13 s under `--cache off`. Six nested Cargo workloads converged to
1,668,322,816 accounted bytes under B = 2,147,483,648 bytes. An isolated held
FLUI host lock blocked bare xtask but not `heli exec` (0.153 s). Full heli
suite: 346 local tests passed. See `docs/research/multi-project-resource-isolation.md`
for exact refs, binary hash, overlapping compiler intervals and caveats.
This focused workload does not establish every exit criterion below: it used
zero GC ages, did not run a simultaneous full FLUI gate or measure throughput
against the old lock, and is not a universal resource bound or multi-OS load test.

The subsequent runtime-safety repeat used heli `fb5748a` and FLUI `2bb868951`:
four independent Cargo clients still had overlapping compiler invocations,
all twelve commands passed, and idle GC converged to 1,668,335,579 bytes
under the same 2 GiB budget. First compiler: 0.340–8.562 s; package-cache
waits remain. The full local heli suite passed 350 tests plus strict Clippy.
Public regressions separately cover long-running nested tests, another
workspace progressing while one test waits, and a daemon killed while its
test survives. This strengthens runtime safety but conservatively defers GC
until managed activity is idle ([eviction](../build-store.md#eviction)); continuous load has no hard disk cap.
Exact refs, binary hash, overlapping intervals and cleanup evidence are in
the research report. Q2 remains repository opt-in, with per-workspace paths.

**FLUI controls, 2026-10-16.** Windows FLUI `70c6a45` through frozen heli
`3b0a26b` used manually prepared shared vendor sources and private Cargo
homes, with separate artifacts per worktree. This was not automatic
production epoch preparation. Of eight original concurrent clients, both
fmt, clippy and check commands exited 0. A telemetry filesystem TOCTOU
cancelled the two tests; both resumed full workspace tests later exited 101.
The observed `flui-material` `failure_cases.rs` test hardcodes its readiness
marker under the checkout's `target/`. The exact same binary failed 101
without heli; creating only the missing marker parent made that individual
native test exit 0. This establishes a consumer fixture path assumption, not
a heli collision or a successful full-suite rerun.

Managed-build GC acceptance for this lab is **invalid setup**: explicit
`build/a` and `build/b` do not have the required
`<store>/<2 hex digits>/<hex digits>` layout. GC was enabled but did not
own or enumerate the measured 43.1 GB of build trees. Their non-collection
does not establish a managed-store GC defect or a passed disk acceptance.

A separate small `flui-foundation check --lib --offline --locked` control
used fresh artifacts, identical manual isolation and `-j 4` in both modes,
in native/heli then heli/native order. Native pairs took 21.498/17.009 s;
heli pairs took 18.422/17.133 s. All eight commands exited 0, with actual
compiler overlap in every pair. A separate ordinary native pair with one
fresh shared lab Cargo home and default jobs also overlapped and exited 0
in 13.036 s; each reported one package-cache wait. Different jobs preclude
a strict ratio with that pair. Manual isolation gets no credit as heli
behavior; these controls establish no full-suite ratio or general speedup.

**Real download contention, 2026-10-16.** The public CLI fixture
`experiments/real-download-hol.py` uses a localhost sparse registry and real
Cargo downloads, without an artificial cache-lock holder. While A waits for
an HTTP archive response, B has all its dependencies cached and runs
`check --offline --locked` with fresh artifacts. With a shared home, B
reports a package-cache lock wait and makes no build-script progress until
the response is released. Both an ordinary private home with actual
`cargo vendor` source configuration and Heli's imported dependency epoch
let B finish while A's response remains gated. Native Windows evidence is
in `target/real-hol-public-1016-0339/results.json`; all commands succeeded
and B's lockfile remained unchanged. This establishes a real Cargo
head-of-line blocking case and the benefit of source isolation, not a
unique Heli mechanism or a throughput speedup. Imported epochs require
explicit preparation and selection; the native private-home profile in
[worktree Cargo homes](../build-store.md#worktree-cargo-homes) prepares an isolated home automatically after explicit selection. The
dependency experiment workflow repeats this public fixture on three OSes.

The FLUI marker fixture repair is published in PR #1683 at `8c48ac7a3`:
runtime scratch now uses a unique temporary path rather than assuming a
checkout-local `target/`. The same public test is red before and green
after the change with relocated Cargo outputs. All four library tests and
the 14-test changed-scope gate through Heli passed locally. This is scoped
repair evidence, not a successful rerun of both full concurrent suites.

**Native private-home FLUI repeat, 2026-10-16.** Frozen Heli `c86d153`
and FLUI `8c48ac7a3` run eight commands across two real Git worktrees:
`fmt --all`, `clippy --workspace --all-targets --locked`,
`check --workspace --all-targets --locked`, and
`test --workspace --locked --no-fail-fast` in each worktree. Each worktree
has its own native Cargo home and managed build directory; no vendor import
or source-cache seeding precedes the commands. Both fmt, clippy and check
runs exited 0. Paired compiler start/end records prove actual compiler
overlap across worktrees, not merely overlapping command lifetimes.

The B test run exited 101 after 3259.7 seconds; A passed its full test
command, including trybuild and doctests, after 3630.1 seconds. Seven of
the eight commands therefore passed. B reported five failing tests across four
targets: the CLI HTTP server contract (Windows connection reset), two
hot-reload host-wake tests, Windows text-services COM teardown, and the
runtime sampling diagnostic-event assertion. The existing hot-reload
binary reproduced the same two failures with parallel test threads but
passed 30 serial repetitions; two hook tests omit the existing lock for
their process-global rebuild callback. The original runtime test binary also reproduces its
same diagnostic assertion without Heli: one failure in 12 parallel
repetitions, versus 20 passing serial repetitions. A standalone tracing
probe reproduces a missing event when another thread without a subscriber
first registers its callsite; reinstalling the subscriber restores delivery.
These observations do not by themselves establish cross-worktree
interference or a Heli defect.

Final evidence contains 20,199 completed compiler intervals, with no orphan,
ambiguous or malformed events, and a successful cross-worktree overlap pair.
Nonzero compiler exits include expected compile-fail tests; command results
remain the authority for test success. Original configuration, credentials
and project lockfile digests stayed unchanged. Idle GC reclaimed the managed
build directories. An initial 6,394,047,229-byte trash debt settled to zero
after one retry. The two live checkout targets retained 4,771,850,254 bytes
of final artifacts, above the configured 2-GiB budget: this is an unmet budget,
not permission to remove protected outputs. Native source homes are outside
that budget. The driver removed both clean worktrees after measurement,
with no reported cleanup error. The whole run is `FAIL`, rather than full
concurrent-suite or resource-budget acceptance.
Evidence is retained in `target/flui-native-c86d153-1` and the isolated
consumer-repair checkout. This run does not supersede the full xtask and
nested-Cargo compatibility scope in [placement](../build-store.md#placement) or the repository opt-in Q2 decision.

**Consumer repairs and multi-project progress, 2026-10-16.** FLUI PR #1683
at `d85ab1e73575388ba8112c1d86dbc7dd95491717` repairs the observed test
fixtures. Both missing rebuild-hook guards now use the existing process
lock. Runtime diagnostic tests retain a dormant tracing dispatcher to
avoid upstream tracing callsite-interest issue #3611; a fresh-child
regression reproduces the missing event before that repair. Each affected
hot-reload and runtime target passed 50 parallel repetitions. Native
Windows platform tests passed 26 tests, with one interactive test ignored:
the COM oracle distinguishes an owned STA lease from an implicit MTA, and
the window oracle enumerates the owning thread's windows instead of
mistaking another worktree's same-title window for a leak. Deliberate
lease, shutdown and foreign-window negative controls still fail. The
test-only HTTP reader accepts a connection reset only after a complete,
unambiguously length-framed response; the native CLI target passed 12
repetitions of all 17 tests. The changed-scope gate through Heli passed
formatting, strict Clippy and 106 tests, with seven configured skips. This
does not change production COM or HTTP behavior or substitute for the
full concurrent repeat.

That repeat uses frozen Heli `c86d153` and two clean FLUI worktrees at
`d85ab1e73`, with automatic native homes and an explicit 8-GiB artifact
budget. During its two full workspace test commands, two additional,
independent Git projects submitted ordinary `heli test --offline --locked`
commands through the same daemon, pool, store and activity namespace.
Both std-only canaries reached their gated tests while both FLUI tests
were active: four worktrees across three repositories, with four distinct
native homes. An explicit GC reported `runtime_busy=true` and no victims.
Both canaries then read their build-script assets after GC and exited 0
while both FLUI tests remained active. The original Cargo configuration
digest was unchanged; no original credential file was present in this
sidecar's input manifest, so this is not an authentication check. Evidence is in
`target/multi-project-d85-sidecar`; the full FLUI repeat is in
`target/flui-native-d85ab1e73-2`. Its terminal result follows below.
This establishes independent completion of short projects alongside long
FLUI tests, not a matched throughput improvement or multiple full gates
on three large projects. Busy accounted usage exceeded the 8-GiB budget;
the global runtime guard deliberately postpones reclamation ([eviction](../build-store.md#eviction)).
Busy preservation is separate from the idle-budget acceptance below.
Native source homes remain outside that budget.

**Completed repaired repeat, 2026-10-16: PASS.** Driver session 57174
terminated with exit 0. All eight normal built-in commands succeeded at
FLUI `d85ab1e73575388ba8112c1d86dbc7dd95491717`, using frozen Heli
`c86d15376091cb0d802a77c6e5dc1d443aace678` (binary SHA-256
`7a7925dcd4e8be4df9451c4423dafb82fd1e9292a31e17bd2c258dfe37c59721`).
The native source homes were reused at their prior canonical paths; this
is not a cold-source-cache timing comparison. Command wall times include
their own worktree's Cargo locks and scheduling, not just compiler time:

| Command | Worktree A, seconds | Worktree B, seconds |
|---|---:|---:|
| `fmt --all` | 6.7 | 6.9 |
| `clippy --workspace --all-targets --locked` | 640.6 | 202.5 |
| `check --workspace --all-targets --locked` | 659.9 | 696.2 |
| `test --workspace --locked --no-fail-fast` | 3628.8 | 3630.8 |

Both full tests include their normal nested Cargo, trybuild and doctest
work. All five cases that failed in the prior run now pass in both
worktrees. An independent audit reparsed all 40,408 raw compiler events
into 20,204 complete pairs: no malformed, unpaired, ambiguous or invalid
records. The union of successful compiler invocation lifetimes from each
worktree overlaps for 2,356.5887101 seconds across 380 disjoint segments.
This is actual simultaneous in-flight compiler work, not CPU utilization
or a speedup against native Cargo. Nonzero internal compiler exits include
compile-fail cases; the eight outer command exits determine test success.
Seven before/after input digests match, and both worktrees were clean at
the pinned commit before their removal.

All 136 during-run GC surveys reported runtime busy and selected no
victims; 78 managed build directories were observed. The sampled store
peak was 133,582,046,287 bytes, or 140,652,790,817 bytes when including
native homes and checkout targets. After activity became idle, GC reclaimed
the build directories, leaving 75 administrative bytes in the store.
Accounted artifacts, measured before removing the worktrees, were
**4,771,735,920 bytes under B = 8,589,934,592 bytes**, with F = 0 and no
pending trash. No deletion retry was needed. The retained native homes
were another 2,299,008,964 bytes outside B. The original 2-GiB acceptance
failure remains valid; this explicit larger-budget result does not
retroactively pass it or establish a hard limit during activity.

Across 2,028 system-memory samples, available physical memory never fell
below 26,192,412,672 bytes and available commit never fell below
15,231,492,096 bytes. These are host observations, including other host
activity, not per-agent RAM quotas or proof of an enforced hard cap. Both
clean worktrees were removed after measurement; the owned daemon exited,
and the audit found no cleanup error. Exact receipts, the raw events and
`FINAL-AUDIT.md` remain in the full-repeat evidence directory.

The checked-in `experiments/flui-worktree-acceptance.py` reproduces the
same eight-command workload in a fresh lab; it deliberately does not
adopt old labs. `experiments/flui-mixed-project-sidecar.py` reproduces the
short-project progress/GC check on Windows. Their dry-run and cleanup
safety checks run in the three-OS experiment workflow; the heavy FLUI run
is explicit, not an automatic CI job. See `docs/multi-agent.md` for the
commands and cache-warmth limits. This is a passed workload acceptance,
not a claim that every strict phase criterion or every external Cargo
tool profile is complete.

FLUI main still had a common per-user host lock in
`tools/xtask/src/host_lock.rs` when checked on this date.
[FLUI PR #1683](https://github.com/vanyastaff/flui/pull/1683), containing the
heli integration, was open and unmerged. Direct Cargo runs do not exercise
that xtask queue. Local evidence is in
`target/flui-two-3b0a26b-current/REPORT.md`, its `final-summary.json`, and
`matched-control-20261016T081315547457Z/results.json` beneath that lab.

**Shared verification acceptance, 2026-10-16.** Heli `7326ac9` and FLUI
`bf23bf57f` passed a fresh three-mode comparison using two pinned worktrees,
ordinary checkout-local targets, and one shared owned Cargo home per mode.
All 18 client commands exited 0. The pair's cold changed-scope gate took
728.9 s with the stock xtask host lock, 505.5 s with that lock disabled,
and 334.7 s through `exec --verify` with Heli's saved verdicts disabled.
The exact repeats hit xtask's cache in all modes (6.7/7.2/7.7 s).
The native nested group took 616.6/536.9/304.4 s, respectively; its 11 tests
passed in every executed group. Heli served each matching pair from one
actual run, rather than compiling and testing both worktrees. Ordinary
unlocked Cargo also compiled concurrently; parallelism alone is not a
benefit unique to Heli. The measured Heli benefit here is live sharing of
identical verification, with xtask's managed-result cache restored.

The frozen release's same-worktree counter separately passed all 12 clients:
native Cargo executed identical tests twice; live-only direct and wrapped
Heli executed once. An edit required a second source generation, queued
behind the first before rebuilding. Real Cargo regression tests also prove
that compile commands produce an artifact for each independent worktree.
Cargo already reuses up-to-date artifacts for ordinary repeated builds;
Heli does not claim to eliminate a second compilation that Cargo would skip.

Four busy GC surveys reclaimed nothing; an idle survey resumed afterward.
Minimum system-available RAM was 22.3/20.9/27.2 GiB across the three modes.
This was not a pressure or hard-quota test. Heli's complete local suite
passed 393 tests, Clippy passed with warnings denied, and FLUI's scoped
gate passed all 61 xtask tests. The FLUI experiment covers the changed-scope
gate and native nested group, not full platform/feature CI parity. It was
one fixed-order Windows run; Linux/macOS execution remains unverified here.
See [the acceptance report](shared-verification-acceptance.md)
for pins, regression mapping, the excluded prototype-profile preparation,
retained receipts and remaining interference boundaries. Q2 stays opt-in;
this ordinary-layout experiment does not change its build-dir acceptance.

- **Phase 0**, no code, one day, tracked as its own issue. A semaphore kept
  open, `CARGO_MAKEFLAGS` in the agents' environment, flui's host lock turned
  off. It measures whether a shared pool alone ends the queues and bounds
  memory. Known gaps: no governor, no leak top-up, no run pools, test threads
  unbounded.
- **Phase 1.** Daemon, host pool, run pools and broker, governor, bounded
  top-up, `cargo heli build/check/clippy/test/nextest/exec`, detach and
  attach, `status`, output modes with the run log and `cargo heli log`. Exit
  criteria, with at least 4 agents across multiple Cargo repositories
  (including flui), and multiple worktrees of a shared repository:
  - no run waits more than 10 s for its first rustc;
  - available commit never drops below 10 %;
  - no out-of-memory failure;
  - total wall time no worse than with the lock;
  - compact output at least 80 % smaller than full output on green runs of
    a one-crate nextest run and of `clippy --all-targets`;
  - on a failing run, the compact output alone is enough to locate each
    failure.
- **Phase 2.** Build store: placement, accounting, the eviction ladder,
  conservative recycling, then precise recycling. Entry gate: flui's full test
  suite passes with `build-dir` set ([placement](../build-store.md#placement)). Operational acceptance uses the
  same shared-host workload as phase 1: at least four concurrent agent runs
  across repositories and distinct worktrees, mixing short checks, a full
  gate and nested cargos.
  Repeat worktree creation, builds and retirement across multiple cycles.
  With B and F explicitly configured, record all managed build directories,
  residual targets and nested outputs, GC decisions and busy-directory skips.
  After builds become idle and GC completes, usage must satisfy the budget
  and free-space policy. Insufficient eligible reclamation is an explicitly
  unmet budget, not a passed acceptance check; live builds and unregistered
  directories must survive. Record each run's
  time to first compiler, completion, memory/commit headroom and cache reuse
  alongside disk growth. The single-worktree compatibility gate in #27
  establishes placement compatibility, not this multi-agent acceptance.
- **Phase 3.** Result store and single-flight.
- **Phase 4.** Donation by block cloning, per-job memory calibration.
  Weighted shares by priority are done ([host pool](../host-pool.md), "Priorities").

Measure throughout:

- time to the first rustc;
- run wall time;
- pool utilization;
- broker latency;
- minimum available memory and commit;
- disk by category per day;
- units rebuilt after a recycle;
- output bytes per run in each mode.
