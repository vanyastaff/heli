# heli: one machine, many agents, one cargo budget

Status: design draft, 2026-10-13. "heli" is a working name.

`cargo nextest` took over what `cargo test` did per *test*: a process per
test, timeouts, groups, retries. heli takes over what nobody owns today: the
*machine*. Every cargo run on a host shared by many agents and worktrees draws
from one pool of compile and test slots, keeps its intermediate artifacts in
one budgeted store, and never waits behind an unrelated run.

```
cargo heli nextest run -p flui-view     # instead of: cargo nextest run -p flui-view
cargo heli clippy --all-targets         # instead of: cargo clippy --all-targets
cargo heli exec -- cargo xtask ci       # any other command, under the same budget
cargo heli status                       # who builds what; slots, memory, disk
```

Everything after the subcommand is cargo's or nextest's own arguments, passed
through unchanged.

## 1. The problem, measured

Host: Windows 11, 32 logical CPUs, 64 GB RAM. Workspace: flui, 39 members and
462 third-party packages on the host platform. 14 agent worktrees. Measured
2026-10-13.

| Symptom | Evidence | Cause |
|---|---|---|
| Runs wait 2-3 h | flui's xtask host lock: one exclusive heavy run per user | Mutual exclusion: a run waits for every run ahead of it, whatever their size. One workspace build peaks at 3.9 GB with 8 jobs (flui `design/build-footprint.md`), so the machine idles while the queue grows. |
| Memory still runs out | Running beside the lock: `cargo nextest run -p flui-animation`, another project's `cargo test`, one rustc at 4.3 GB | The lock covers only `cargo xtask`. Every other cargo sizes its own pool to all 32 CPUs, and nested cargo inside tests (template, facade and trybuild builds) does it again. |
| Disk fills | 592 GB of `target/` across 14 worktrees, 203 GB free on the volume. The largest is 208.8 GB: 114 GB `debug/incremental`, 31 GB `tests/trybuild`, 24 GB nested-build caches, 30 GB `debug/deps` | Nothing reclaims a build directory while its worktree lives. `incremental/` keeps a directory for every unit ever built (each feature set, each `-p` set), and all of it is regenerable. |
| Every new worktree builds cold | a new agent worktree has no `target/` | The build directory lives inside the worktree and dies with it. |
| Work repeats | agents re-run gates on unchanged trees; a tool timeout kills a long run, which is then restarted | Results are not remembered; runs are children of the agent's shell. |

None of the cargo issues that would fix this is close to landing as a whole
(§11 maps each part of heli to the issue it works around).

## 2. Goals and non-goals

Goals:

- **G1, no head-of-line blocking.** A run starts compiling at once, with at
  least one job, whatever else runs.
- **G2, one CPU and memory budget per host**, including cargo runs heli did
  not start.
- **G3, a disk budget enforced automatically**, deleting only what cargo
  regenerates.
- **G4, warm worktrees.** A new worktree starts warm, and two live worktrees
  never share a build directory.
- **G5, drop-in.** heli takes the same arguments as cargo and nextest. When
  heli is absent or broken, plain cargo runs; heli never fails a build.
- **G6, agent-proof.** A killed client neither kills nor wastes its run, and
  identical runs are not repeated.

Non-goals:

- Changing cargo's freshness rules, or sharing one build directory between
  live worktrees. That is unsound: flui `docs/testing.md`, "One target
  directory per checkout".
- Remote or cross-machine caching. sccache stays usable, because heli does not
  take `RUSTC_WRAPPER`.
- Feature unification across `-p` sets (cargo#14774). cargo-hakari exists for
  that.
- Replacing nextest. heli schedules nextest, and nextest still runs the tests.

## 3. Shape

```
 agent shells (any worktree, any project)
   |  cargo heli nextest run ...            cargo build ...  (bare)
   v                                           |
 +---------------+   IPC: named pipe /         |  CARGO_MAKEFLAGS, written
 |  cargo heli   |   unix socket, owner only   |  by `cargo heli setup`
 | (thin client) |-----------------+           |
 +---------------+                 v           v
                     +------------------------------------------+
                     | heli daemon (one per user)               |
                     |  - host pool: jobserver with N slots     |<-- every cargo, rustc,
                     |  - memory governor, leak top-up          |    build script and
                     |  - runs: test slots, detach, dedup       |    nested cargo
                     |  - build store: budget, GC, recycling    |
                     |  - result store                          |
                     +------------------------------------------+
                                       | owns
                                       v
                <store>/<workspace-path-hash>/   build dirs, outside the worktrees
```

There are four parts, and each is usable alone:

1. The **host pool** (§4) serves G1 and G2, and covers bare cargo too.
2. **Runs** (§5), the `cargo heli <cmd>` front end, serve G1 and G6.
3. The **build store** (§6) serves G3 and G4.
4. The **result store** (§7) serves G6.

The daemon runs commands as its own user, in the client's working directory
and with the client's environment. Its IPC endpoint (a named pipe with an
owner-only ACL, or a socket with mode 0600) accepts only that user.

## 4. Host pool

### Mechanism (verified)

Cargo uses a jobserver inherited through `CARGO_MAKEFLAGS`, `MAKEFLAGS` or
`MFLAGS` instead of creating its own (cargo `src/context/mod.rs` 313-351,
`src/compiler/build_runner/mod.rs` 109). On Windows the handle is a named
semaphore opened by name (`jobserver` 0.1.35 `src/windows.rs` 121); on Unix it
is `fifo:PATH`. Cargo hands the same jobserver to rustc for parallel codegen
and to build scripts (cc-rs), and a nested cargo inherits it from the
environment. If the handle cannot be opened, cargo prints a warning and
creates its own pool.

The probe [`experiments/jobserver-probe.ps1`](experiments/jobserver-probe.ps1)
built a 34-package project on this host:

| Pool | Most rustc processes at once |
|---|---|
| 0 free slots | 1 |
| 5 slots released mid-build | 6 |
| no jobserver | 16 |

So the daemon creates one semaphore (Windows: `heli-<user-hash>` in the
session namespace) or one FIFO (Unix: `$XDG_RUNTIME_DIR/heli/jobserver`, else
`~/.cache/heli/jobserver`, mode 0600) with N slots and keeps it open. Every
cargo started with `CARGO_MAKEFLAGS=-j --jobserver-auth=<name>` draws from it.

### Properties

- **Progress.** Each process the make protocol starts owns one implicit slot
  outside the pool, so every cargo runs at least one job. No run starves, and
  none queues behind another (G1). The flip side is that k cargo processes can
  run N + k jobs at once.
- **Waiting costs no pool slot.** A cargo blocked on a build-directory lock
  holds no pool slot, because its implicit slot is not drawn from the pool.
  That makes cargo#6747 irrelevant here.
- **Size.** By default N is the number of logical CPUs. Average memory per job
  is low (3.9 GB at 8 jobs is about 0.5 GB per job), so CPUs run out first.
  Spikes, such as a single rustc at 4.3 GB, are the governor's job.
- **Memory governor.** The daemon samples available physical memory and, on
  Windows, available commit, because allocations fail at the commit limit
  before RAM is full.
  - Below the low watermark (default: 10 % of RAM or 6 GB, whichever is
    larger), it withholds slots: it acquires one per sample as they free up,
    down to 1 slot left in the pool.
  - Above the high watermark (the low one plus 4 GB), it returns them.
  - Jobs already running are never interrupted, so pressure eases as they
    finish.
- **Leaked slots.** A slot held by a process that gets killed is lost:
  semaphores and FIFOs have no owner. Whenever no rustc and no build script
  has been alive for 2 s, no slot can be legitimately held, and the daemon tops
  the pool back up to N minus the withheld slots.
  - Windows: `NtQuerySemaphore` for the count, then `ReleaseSemaphore` up to
    the maximum.
  - Unix: `FIONREAD` on the FIFO, then writes.

  Runs started through heli (§5) are not killed when the agent's tool call
  times out, which removes the main source of leaks.

### Reach

- A run through `cargo heli` gets the variable from the daemon.
- A bare cargo gets it from the environment that `cargo heli setup` writes:
  the agent harness settings (Claude Code `settings.json`, `env`), the shell
  profile, or the user environment. This is the safety net for G2: an agent
  that forgets heli still shares the pool.
- Without a running daemon, a bare cargo prints cargo's jobserver warning and
  runs as it does today. `cargo heli` starts the daemon on demand, and
  `cargo heli setup` can also register it to start at logon (Task Scheduler,
  systemd `--user`, launchd) or as a harness session-start hook
  (`cargo heli ensure`).

## 5. Runs: `cargo heli <subcommand> [args...]`

A run is one cargo or nextest command that the daemon executes on behalf of a
client.

### Classes

Runs are classified by subcommand. `cargo heli exec -- <cmd>` covers anything
else, for example `cargo xtask check-changed`.

| Class | Subcommands | Pool | Test slots | Result store | Worktree lock |
|---|---|---|---|---|---|
| compile | build, run, doc | yes | no | no | shared |
| verify | check, clippy, fmt --check | yes | no | yes | shared |
| test | test, nextest run | yes | yes | yes | shared |
| mutate | fmt, fix, clippy --fix | no | no | no | exclusive |
| exec | anything else | yes (environment) | no | opt-in | shared |

### Lifecycle

1. **Key** (verify and test runs only). The key covers:
   - the worktree's tree with every change staged: `git add -A` into a
     scratch copy of the index, then `git write-tree`, as in flui#1619;
   - the normalized argv;
   - the toolchain versions;
   - the contents of the Cargo configuration;
   - the environment, minus a neutral list (§7).
2. **Result store.** On a hit, heli prints "already passed: <run>, <when>"
   and exits 0 (§7).
3. **Single-flight.** If a run with the same key is in flight, the client
   attaches to it: it streams that run's output and returns its exit code.
4. **Supersede.** A run from the same worktree, with the same argv and a
   different tree, that has not started yet is cancelled. It exits 3 with
   "superseded". A run that has started is never killed for this.
5. **Detach.** The daemon spawns the command, inside a Job Object on Windows
   or a process group on Unix, and the client streams its output.
   - A killed client leaves the run going, and re-running the same command
     attaches to it.
   - `cargo heli cancel <run>` kills the whole process tree.
6. **Compile phase.** The environment gets the pool, plus
   `CARGO_BUILD_JOBS` set to a fair share: max(2, ceil(N / active runs)) at
   start. Cargo cannot change its job count later, but the pool still balances.
7. **Test phase** (test class only).
   - heli first builds under the pool, with `nextest run --no-run <args>` or
     `cargo test --no-run <args>`.
   - It then takes K slots from the same pool. K is the smallest of the
     thread count asked for, the fair share, and what is free within 5 s, and
     at least 1.
   - It runs the tests with `--test-threads K` (nextest) or
     `-- --test-threads=K` (libtest) and holds the K slots until they finish.
   - The wait happens before nextest starts, so it never counts toward
     `slow-timeout`. nextest's test groups and per-test overrides still apply
     within K.
   - Nested cargo builds inside tests draw from the pool through the
     inherited environment. They can go beyond K, but stay within the pool.
   - The second invocation re-checks freshness; its cost is to be measured.
8. **Record.** heli records a passing verify or test run whose tree is
   unchanged at the end (§7), and writes a source-state record for its build
   directory (§6.4).
9. **Report.** A header line (slots, wait, fair share, key), then the tool's
   own output, then a footer line (duration, peak memory of the process tree,
   slots held).

**Worktree lock.** heli runs that read a worktree's sources hold its lock
shared, and mutate runs hold it exclusive, so `cargo heli fmt` never rewrites
a file that another heli run is compiling. Bare cargo ignores this lock, so
the rule "one writing agent per worktree" stays.

## 6. Build store

### 6.1 Placement

`cargo heli setup` sets `build.build-dir = "<store>/{workspace-path-hash}"`
in the user's Cargo configuration, or through `CARGO_BUILD_BUILD_DIR`.
Verified: `build-dir` supports the templates `{workspace-root}`,
`{cargo-cache-home}` and `{workspace-path-hash}`, the last being a hash of the
canonical manifest path (cargo `src/context/mod.rs`, `custom_build_dir`; Cargo
Book, `build.build-dir`).

The intermediates (`deps/`, `incremental/`, `.fingerprint/`, `build/`) leave
the worktree. `target/` keeps the final artifacts and tool caches: doc,
trybuild, and flui's nested-build directories.

`<store>` must be on the same volume as the worktrees. Cargo hardlinks final
artifacts out of the build directory, and across volumes it copies them
instead. The default is `<volume>/.heli/build`, one store per volume that
holds worktrees.

This is the direction of cargo#16147. heli turns it on and manages what
accumulates.

### 6.2 Accounting

Each build directory holds `.heli/owner`: the workspace path, the repository
id (its git common dir), the creation time and the last run. The daemon keeps
sizes by category (incremental, deps, build scripts, fingerprints, other), for
build directories and for registered `target/` directories. It rescans a
directory after cargo releases its lock there.

A build directory is in one of three states:

- **Busy.** `<build-dir>/<profile>/.cargo-build-lock` cannot be locked; cargo
  holds it exclusively for a build (`src/compiler/layout.rs`).
- **Live.** Its workspace path exists.
- **Orphan.** Its workspace is gone, because the worktree was removed.

### 6.3 Budget and eviction

The budget is B GB for all build directories plus registered `target/`
directories, with a free-space floor F on each volume. Over budget, heli
evicts down to 85 % of B, cheapest to regenerate first:

1. `incremental/<unit>` directories untouched for 24 h, oldest first across
   the store. Each unit ever built leaves one; they make up the 114 GB above.
2. Orphans that no new worktree claimed within 24 h (§6.4), oldest first.
3. All of `incremental/` in build directories idle for 6 h.
4. Configured tool caches in idle worktrees' `target/`: `tests/trybuild`, and
   for flui `cli-template-check` and `facade-consumer-check`.
5. Units in `deps/` and `.fingerprint/` unused for 7 days. This is
   cargo-sweep's approach, planned for later.
6. Whole build directories idle for 7 days, oldest first.

Safety rules:

- **Never wait on a lock.** heli tries to lock `.cargo-build-lock` (build
  directory) or `.cargo-lock` (artifact directory) exclusively. If the lock is
  held, the directory is busy and skipped.
- **Rename, then delete.** Under the lock, heli only renames the victim into
  `<store>/.trash/`, which on one volume takes constant time, and releases the
  lock. It deletes the trash in the background at low I/O priority. A cargo
  that starts meanwhile finds the directory gone and rebuilds.
- **Narrow scope.** heli deletes only below registered roots, only the
  subpaths listed above, and never through a symlink or a junction.
- **Unknown layout.** Lock names change between cargo versions. If the layout
  is unknown, heli treats a directory as busy when anything in it changed
  within the last hour.

### 6.4 Recycling: warm starts without sharing

A worktree with no build directory can appear in three ways: the first
`cargo heli` run in it, the git `post-checkout` hook that `cargo heli setup`
installs (git runs it for `git worktree add`), or the daemon's periodic
`git worktree list`. heli gives it the most recently used orphan of the same
repository, renamed to the new worktree's `{workspace-path-hash}` path. On one
volume that is a rename: no copy and no extra disk. One build directory still
belongs to one live worktree.

Why this is sound, verified in cargo's source:

- **Paths.** Cargo's own dep-info encodes each input path relative to the
  package root, the workspace root or the build root. It keeps absolute paths
  only for files outside the workspace, such as registry sources and the
  sysroot, and those are the same for every worktree on the host
  (`src/compiler/fingerprint/dep_info.rs`, `DepInfoPathType`).
- **Build scripts.** Moves of a build script's `OUT_DIR` are handled through
  the `root-output` file (`src/compiler/custom_build.rs`).
- **Unit hashes.** They do not depend on the workspace's absolute path: flui
  saw identical artifact names across worktrees (`docs/testing.md`).
- **Freshness** is decided by mtime, including the rule that a unit is stale
  when a dependency is newer than it (`src/compiler/fingerprint/mod.rs`,
  `StaleDependency`).

There are two modes:

- **Conservative**, always available. The new worktree's files are newer than
  every artifact, so all 39 workspace members rebuild and the 462 third-party
  packages stay fresh. This is correct by cargo's own rules.
- **Precise.** The donor has a *source-state record*: the blob id and mtime of
  every tracked or untracked file.
  - The record counts only if it was taken after the donor's last build: its
    time is at least the newest mtime in the build directory. Otherwise heli
    ignores it.
  - For each file in the new worktree whose blob matches the record, heli sets
    the recorded mtime. Every other file keeps the current time.
  - Every file with the donor's content then looks to cargo exactly as it did
    in the donor, and every changed file is newer than any artifact. Only the
    changed crates and their dependents rebuild.
  - heli writes records after every run it executes, and the daemon writes one
    when it sees a build lock released.
  - Ignored inputs, such as an `include_str!` of an ignored file, are not
    recorded. Cargo rebuilds when one is missing; one that is present keeps
    the current time and also triggers a rebuild.
  - With `-Zchecksum-freshness` (cargo#14136), cargo compares content itself,
    and precise mode becomes unnecessary.

**Later: cloning.** On volumes with block cloning (Windows Dev Drive on ReFS,
btrfs, XFS, APFS), recycling can clone instead of renaming, preserving mtimes.
A *live* worktree can then donate, forking a warm build directory. The same
rules apply.

## 7. Result store

- For each passing verify or test key, heli keeps a marker
  `<state>/verified/<digest>` holding the run id, the worktree and the time.
  It keeps at most 256 markers and drops the least recently used first.
- The key is the one from §5. One rule from flui#1619 is kept: an
  environment variable is part of the key unless it is listed as neutral (job
  counts, terminal styling, harness session ids). An unknown variable then
  misses the cache instead of reusing a result.
- A marker is written only when the tree at the end equals the tree at the
  start.
- Every hit prints the caveats: a flaky test that passed once stays green for
  that key (`--no-cache` runs it again), and ignored files are not part of the
  key.
- Compile runs are never recorded, because their product is the artifact, not
  the verdict.

## 8. Observability

`cargo heli status`:

```
pool   32 slots: 21 compiling (4 runs), 6 testing (1 run), 0 withheld   memory 41.2/64 GB, commit 52 %
runs   r17  agent-a64b...  nextest run -p flui-widgets   test     6 slots  4m12s
       r18  flui-anim...   clippy --all-targets          compile  -j 8     1m03s
       r19  agent-a10...   check -p flui-view            attached to r12 (same key)
disk   D:\.heli\build  148/150 GB (incremental 61, deps 52, ...)  next: incremental >24h, ~40 GB
```

There is also `cargo heli status --json`, a rotating log of admissions,
withholdings, top-ups and evictions, and `cargo heli gc --dry-run`.

## 9. Adoption in flui

1. Run `cargo heli setup`: pool environment for the agents, `build-dir`, a
   store on D:.
2. Update AGENTS.md: `cargo heli nextest run -p <crate>` while iterating, and
   `cargo heli exec -- cargo xtask check-changed` before a PR.
3. In xtask, skip `host_lock` when it runs under heli (`HELI_RUN` is set) or
   when the pool variable is present. The pool replaces the fixed
   `CARGO_BUILD_JOBS=28` from flui#1619.
4. `cargo xtask worktree prune` keeps handling branches. Deleting `target/` is
   no longer its job.

## 10. Failure modes

| Failure | Effect | Handling |
|---|---|---|
| Daemon not running | A bare cargo warns and uses its own pool, as today | `cargo heli` starts the daemon; logon or session hook |
| Daemon crashes mid-build | Running cargos keep their handles: a Windows semaphore lives while any handle is open, and a FIFO path persists | The restarted daemon adopts the existing semaphore or FIFO |
| Slot leak | The pool shrinks | Top-up at quiescence (§4) |
| Memory spike | New jobs wait | Slots are withheld; running jobs finish |
| GC races a build | none | Try-lock, rename to trash, skip busy directories |
| Recycled directory with a bad record | Could link stale code | A record counts only if it is newer than every file in the build directory; otherwise conservative mode |
| False green from the cache | A flaky pass is reused | Printed caveat; `--no-cache` |
| Another logon session (SSH, a service) | On Windows, a separate session namespace, so a separate pool | Documented; a global namespace needs admin rights (later) |

## 11. Upstream map

| Issue | heli's part | When it lands |
|---|---|---|
| cargo#5931, per-user artifact cache | §6.4 recycling, one owner at a time | heli's store can defer to it |
| cargo#16147, build-dir out of the workspace | §6.1 | heli stops setting it |
| cargo#14136, checksum freshness | §6.4 precise mode | Precise mode becomes cargo's |
| cargo#4282, #17508, finer locking | Not solved: one build directory per worktree, and the pool makes waiting cheap | Concurrent runs in one worktree get faster |
| cargo#6747, jobserver token while waiting on a file lock | Irrelevant under the pool (§4) | none |
| cargo#13136, target GC | §6.3 | heli's eviction ladder can call it |
| cargo#14774, feature unification | Out of scope (cargo-hakari) | Fewer duplicate units for §6.3 |
| rust#113349, parallel front end | The pool already lends rustc its threads | none |

## 12. Plan

- **Phase 0**, no code, one day. A semaphore kept open, `CARGO_MAKEFLAGS` in
  the agents' environment, the xtask lock turned off. It measures whether the
  pool alone ends the queues. Known gaps: no governor, no leak top-up, test
  threads unbounded.
- **Phase 1.** Daemon, pool, governor, top-up,
  `cargo heli build/check/clippy/test/nextest/exec`, detach and attach,
  `status`. Exit criteria, on flui with at least 4 agents:
  - no run waits more than 10 s for its first rustc;
  - available commit never drops below 10 %;
  - no out-of-memory failure;
  - total wall time no worse than with the lock.
- **Phase 2.** Build store: placement, accounting, the eviction ladder,
  conservative recycling, then precise recycling.
- **Phase 3.** Result store, single-flight, supersede.
- **Phase 4.** Donation by block cloning, fair-share priorities, per-job
  memory calibration.

Measure throughout: time to the first rustc, run wall time, pool
utilization, minimum available memory and commit, disk by category per day,
and units rebuilt after a recycle.

## 13. Open questions

1. The name.
2. `build-dir` in the user's Cargo configuration affects every project on the
   machine. Should it be opt-in per repository instead?
3. The default budget B (proposed: 150 GB on D:) and floor F (proposed: 50 GB).
4. Should supersede ever kill a run that has already started in the same
   worktree with the same command?
5. Is a pool per logon session enough, or do agents run in other sessions
   (SSH, services)?
