# heli: one machine, many agents, one cargo budget

Status: design draft, 2026-10-13. "heli" is a working name.

This document is the single source of truth for heli's mechanisms and
contracts. Issue #1 states the problem, the user stories and the testing
decisions, and links here for everything else; the tickets link to the
sections they implement.

`cargo nextest` took over what `cargo test` did per *test*: a process per
test, timeouts, groups, retries. heli takes over what nobody owns today: the
*machine*. Every cargo run on a host shared by many agents and worktrees draws
from one pool of compile and test slots, keeps its intermediate artifacts in
one budgeted store, and never waits behind an unrelated run.

```
cargo heli nextest run -p my-crate      # instead of: cargo nextest run -p my-crate
cargo heli clippy --all-targets         # instead of: cargo clippy --all-targets
cargo heli exec -- cargo xtask ci       # any other command, under the same budget
cargo heli status                       # who builds what; slots, memory, disk
```

Everything after the subcommand is cargo's or nextest's own arguments, passed
through unchanged.

## 1. The problem, measured

heli is not tied to one project. The numbers below come from the host and
workspace that motivated it, and serve as the worked example throughout:
Windows 11, 32 logical CPUs, 64 GB RAM; the flui workspace (39 members, 462
third-party packages on the host platform); 14 agent worktrees. Measured
2026-10-13.

| Symptom | Evidence | Cause |
|---|---|---|
| Runs wait 2-3 h | flui's own host lock: one exclusive heavy run per user | Mutual exclusion: a run waits for every run ahead of it, whatever their size. One workspace build peaks at 3.9 GB with 8 jobs, so the machine idles while the queue grows. |
| Memory still runs out | Running beside the lock: a plain `cargo nextest run -p …`, another project's `cargo test`, one rustc at 4.3 GB | The lock covers only the project's task runner. Every other cargo sizes its own pool to all 32 CPUs, and nested cargo inside tests (generated projects, trybuild) does it again. |
| Disk fills | 592 GB of `target/` across 14 worktrees, 203 GB free on the volume. The largest is 208.8 GB: 114 GB `debug/incremental`, 31 GB `tests/trybuild`, 24 GB nested-build caches, 30 GB `debug/deps` | Nothing reclaims a build directory while its worktree lives. `incremental/` keeps a directory for every unit ever built (each feature set, each `-p` set), and all of it is regenerable. |
| Every new worktree builds cold | a new agent worktree has no `target/` | The build directory lives inside the worktree and dies with it. |
| Work repeats | agents re-run gates on unchanged trees; a tool timeout kills a long run, which is then restarted | Results are not remembered; runs are children of the agent's shell. |

None of the cargo issues that would fix this is close to landing as a whole
(§11 maps each part of heli to the issue it works around).

## 2. Goals and non-goals

Goals:

- **G1, no head-of-line blocking.** A run starts compiling at once, with at
  least one job, whatever else runs. The only places a run can wait at all are
  listed in §5, "Where a run can wait".
- **G2, one CPU and memory budget per host**, including cargo runs heli did
  not start.
- **G3, a disk budget enforced automatically**, deleting only what cargo
  regenerates.
- **G4, warm worktrees.** A new worktree starts warm, and two live worktrees
  never share a build directory.
- **G5, drop-in.** heli takes the same arguments as cargo and nextest. When
  heli is absent or broken, plain cargo runs: a failure of heli itself never
  fails a run.
- **G6, agent-proof.** A killed client neither kills nor wastes its run, and
  identical runs are not repeated.
- **G7, small output for agents.** An agent reads what it needs to act: the
  verdict, and on failure the failures. Everything else stays retrievable
  without running the command again.

Non-goals:

- Changing cargo's freshness rules, or sharing one build directory between
  live worktrees. That is unsound: the same unit built from two worktrees gets
  one artifact name and one fingerprint, and cargo's mtime freshness then
  links the other worktree's code (observed in flui).
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
                     | heli daemon (one per user, interactive)  |
                     |  - host pool: jobserver with N slots     |<-- bare cargo and
                     |  - run pools + broker, test slots        |    everything it starts
                     |  - memory governor, bounded top-up       |
                     |  - runs: detach, single-flight, logs     |<-- heli runs, through
                     |  - build store: budget, GC, recycling    |    their run pool
                     |  - result store                          |
                     +------------------------------------------+
                                       | owns
                                       v
                <store>/<workspace-path-hash>/   build dirs, outside the worktrees
```

There are four parts, and each is usable alone:

1. The **host pool** (§4) serves G1 and G2, and covers bare cargo too.
2. **Runs** (§5), the `cargo heli <cmd>` front end, serve G1, G6 and G7.
3. The **build store** (§6) serves G3 and G4.
4. The **result store** (§7) serves G6.

The daemon runs commands as its own user, in the client's working directory
and with the client's environment. Its IPC endpoint (a named pipe with an
owner-only ACL, or a socket with mode 0600) accepts only that user.

The daemon must run in the user's interactive session, never as a service:
runs inherit its session, and tests that drive real windows (flui's live
Win32 tests) need the user's desktop. It is started on demand by a client, or
by a logon task that runs only while the user is logged on (§10).

**One daemon per user**, and on Windows per logon session: a lock in the
per-user state directory (`%LOCALAPPDATA%\heli`, `$XDG_STATE_HOME/heli` or
`~/.local/state/heli`) admits one. Named pipes are visible machine-wide,
unlike semaphores, so the Windows endpoint carries the session:
`\\.\pipe\heli-<hash of the state directory>-<session id>`. `HELI_STATE_DIR`
and `HELI_ENDPOINT` override both, and `HELI_POOL` and `HELI_CONFIG` the host
pool (§4) and the configuration file (§5), which is how tests isolate their
daemons. A client resolves them, relative paths against its own working
directory, and passes them to the daemon it starts.

**The endpoint.** On Windows, a pipe whose DACL allows the user's SID alone
(without one, Everyone may read), which rejects remote clients and is
created as the first instance, so the daemon never joins a pipe someone else
created. A client connects at identification level and talks only to a pipe
owned by its own user. On Unix, a socket with mode 0600 in the 0700 state
directory; a client talks only to a socket owned by its uid.

**The protocol** starts with a handshake frozen across versions: `HELI` and
the protocol version, each way. On a mismatch the client runs the command
directly, with one warning line. A client newer than the daemon also asks it
to retire: the daemon takes no new runs and exits once its runs in flight
have finished, so the next call starts a current daemon. An older client asks
nothing, so two installed versions never take turns retiring each other's
daemon. The version stays 1 until the first release: until then a daemon
answers a request it does not know (one a newer client added, such as `log`)
with "unsupported", and the client treats that like a mismatch from an older
daemon. It asks the daemon to retire and does without it, after one warning
line: it runs the command, or reads the run log, itself.

**Starting on demand.** When no daemon answers, the client starts one, and
any client falls back to running the command directly when the daemon does
not start, listen or answer within 5 s.

- The client starts `cargo heli daemon --detach`, which starts the daemon and
  exits once it listens. The daemon's parent is then gone, so no tree walk
  from the client reaches it. Agent harnesses stop a command by killing its
  process tree found through parent processes: observed with Claude Code on
  Windows, where stopping a command killed the client, the daemon it had
  started directly, and that daemon's run.
- It inherits no handle from the client (on Windows it is started with
  `CreateProcessW` and no inheritance, since the standard library lets a
  child inherit every inheritable handle). A daemon holding the client's
  stdout would keep an agent's tool call waiting until the daemon exits.
- It runs in the state directory, so it keeps no worktree busy, and leads a
  process group (Unix) or has a console of its own without a window
  (Windows), so the client's Ctrl-C does not reach it.
- On Windows it leaves the client's job object when the job allows breakaway.
  Claude Code's does not: its one job holds the whole app and every command
  it runs, with no kill-on-close limit, so a daemon started from an agent's
  command stays in that job and outlives the command. A harness that
  terminated that job as a whole would take the daemon and every run with
  it; only a daemon started outside the job, by the logon task, avoids that
  (§10).

**A run** is a child of the daemon, started in the client's working
directory with exactly the client's environment and no stdin. Its stdout and
stderr stream back to the client, which exits with the run's code (on Unix,
of its signal). A run is over when its process exits: output still arriving
within 500 ms is forwarded, and processes it left behind holding its pipes do
not hold the client. Once the daemon has told the client that the run
started, the client never runs the command itself: if the daemon is lost
mid-run, it exits 1.

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

**`-j` has no effect under an inherited jobserver.** Cargo requests a token
for every pending job beyond the first (`src/compiler/job_queue/mod.rs` 582)
and runs as many jobs as it holds tokens, plus one (`has_extra_tokens`, 613).
`-j` and `CARGO_BUILD_JOBS` only size the pool cargo creates when it inherits
none (`build_runner/mod.rs` 109). So the pool a cargo draws from is the only
lever on its parallelism. That is why §4 "Run pools" exists.

The probe [`experiments/jobserver-probe.ps1`](experiments/jobserver-probe.ps1)
built a 34-package project on the example host:

| Pool | Most rustc processes at once |
|---|---|
| 0 free slots | 1 |
| 5 slots released mid-build | 6 |
| no jobserver | 16 |

So the daemon creates one semaphore (Windows: `heli-<hash of the state
directory>` in the session namespace, so one per user, maximum count N) or one
FIFO (Unix: `$XDG_RUNTIME_DIR/heli/jobserver`, else `~/.cache/heli/jobserver`,
mode 0600 in a directory of mode 0700) with N slots and keeps it open.
`HELI_POOL` overrides the name or the path. This is the **host pool**. Every
cargo started with `CARGO_MAKEFLAGS=-j --jobserver-auth=<name>` (Unix:
`fifo:<path>`) draws from it. That value ends at the first space and must be
UTF-8, and cargo opens a semaphore with `OpenSemaphoreA`, so the daemon refuses
a name with a space, or a semaphore name that is not ASCII.

The daemon reads the free count with `NtQuerySemaphore` (ntdll; the handle
needs `SEMAPHORE_QUERY_STATE`, which the one `CreateSemaphoreW` returns has,
and the ones cargo opens do not) or `FIONREAD` on its own nonblocking file
description of the FIFO. Counts are snapshots.

**Adoption.** A restarted daemon adopts a pool that some process still holds,
as it is: the slots that process holds come back to the same pool, so a
restart never doubles it.

- A semaphore lives while any handle to it is open, so one that exists is
  held. `CreateSemaphoreW` opens it (`ERROR_ALREADY_EXISTS`), and it keeps the
  maximum it was created with: a new N takes effect once nobody holds it.
- A FIFO keeps its bytes only while some process holds it open; its path
  outlives them (verified in WSL: write 3, close every descriptor, reopen:
  `FIONREAD` 0). So the daemon first opens it write-only and nonblocking,
  which fails with `ENXIO` when nobody has it open for reading; every
  jobserver client opens it for reading and writing. A held FIFO is adopted as
  it is. One that nobody held is empty, and the daemon writes N bytes into it.
- Slots held by a process that died are not recovered here; that is the
  top-up's job (below).

### Run pools

Each heli run gets a **run pool** of its own: a second semaphore or FIFO,
which the run's processes see through `CARGO_MAKEFLAGS` instead of the host
pool. A broker in the daemon moves slots between the host pool and the run
pools, every 20 ms:

- **Lend.** A run whose pool has no free slot may be waiting for one. The
  broker lends it one from the host pool, preferring the waiting run with the
  fewest slots in use.
- **Return.** Free slots in a run pool beyond one go back to the host pool.
- **Share.** While other runs wait, a run already using more than its share
  (the host pool's capacity divided by the number of active runs) gets no new
  slots, and its slots return as its jobs finish. Nothing running is
  interrupted. A run alone can borrow every slot.
- **Exact accounting.** A run uses `lent − free(run pool)` slots. When a run
  ends, or is cancelled or killed, all of `lent` goes back to the host pool,
  whatever its processes held. Leaks are impossible for heli runs.
- **Latency.** A waiting cargo gets a slot within one broker tick, which is
  small next to a rustc job. To be measured.

Bare cargo, started without heli, still draws from the host pool directly.

### Properties

- **Progress.** Each process the make protocol starts owns one implicit slot
  outside any pool, so every cargo runs at least one job. No run starves, and
  none queues behind another (G1). The flip side is that k cargo processes can
  run N + k jobs at once.
- **Waiting costs no pool slot.** A cargo blocked on a build-directory lock
  holds no pool slot, because its implicit slot is not drawn from a pool. That
  makes cargo#6747 irrelevant here.
- **Size.** By default N is the number of logical CPUs heli may use (as for
  cargo's own default `-j`), and `[pool] slots` in the configuration sets it,
  from 1 to 1024 (a FIFO's capacity must hold N bytes). Average memory per job
  is low (3.9 GB at 8 jobs is about 0.5 GB per job), so CPUs run out first.
  Spikes, such as a single rustc at 4.3 GB, are the governor's job.
- **Memory governor.** The daemon samples available physical memory and, on
  Windows, available commit, because allocations fail at the commit limit
  before RAM is full.
  - Below the low watermark (default: 10 % of RAM or 6 GB, whichever is
    larger), it withholds slots: it acquires one per sample from the host pool
    as they free up, down to 1 slot left.
  - Above the high watermark (the low one plus 4 GB), it returns them.
  - Jobs already running are never interrupted, so pressure eases as they
    finish.
- **Bounded top-up.** Only bare cargo can leak a slot: a process killed while
  holding one, and semaphores and FIFOs have no owner. The host pool's target
  of free slots is `N − withheld − Σ lent`.
  - The daemon raises the free count to the target only at quiescence: no
    heli run active, and no rustc and no build script alive for 2 s.
    Windows: `NtQuerySemaphore` for the count, then `ReleaseSemaphore`.
    Unix: `FIONREAD` on the FIFO, then writes.
  - The pool never grows beyond the target. On Windows the semaphore's
    maximum count is N, and a surplus release simply fails (the jobserver
    client ignores the error). A FIFO has no maximum, so on Unix the daemon
    also drains, at any time, every byte above the target. A token that a
    cargo still held during a top-up (between taking it and starting the job,
    or held by a jobserver client the process scan does not see) then comes
    back as surplus and is removed.

### Reach

- A run through `cargo heli` gets its run pool from the daemon.
- A bare cargo gets the host pool from the environment that
  `cargo heli setup` writes: the agent harness settings (Claude Code
  `settings.json`, `env`), the shell profile, or the user environment. This is
  the safety net for G2: an agent that forgets heli still shares the pool.
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
else, for example a project's own task runner.

| Class | Subcommands | Run pool | Test slots | Result store | Worktree lock |
|---|---|---|---|---|---|
| compile | build, run, doc | yes | no | no | shared |
| verify | check, clippy, fmt --check | yes | no | yes | shared |
| test | test, nextest run | yes | yes | yes | shared |
| mutate | fmt, fix, clippy --fix | no | no | no | exclusive |
| exec | anything else | yes | no | opt-in | shared |

How a command line is read:

- The subcommand comes after a `+toolchain`, if there is one. Cargo's
  built-in aliases count as their subcommands (`b`, `c`, `d`, `r`, `t`), and
  so does nextest's `r` for `run`. Aliases a project defines are exec.
- `fmt` is verify when `--check` is among its arguments: cargo-fmt's own, or
  rustfmt's after `--`.
- `clippy` is mutate when `--fix` comes before `--`; what follows `--` is the
  compiler's.
- `cargo heli exec -- <cmd>` is exec whatever the command, even `cargo fmt`.

### Lifecycle

1. **Key** (verify and test runs only). The key covers the worktree's tree,
   the normalized argv, the toolchain, the Cargo configuration and the
   environment; §7 lists exactly what goes in and how.
2. **Result store.** On a hit, heli prints "already passed: <run>, <when>"
   and exits 0 (§7).
3. **Single-flight.** If a run with the same key is in flight anywhere on the
   machine, the client attaches to it: it streams that run's output and
   returns its exit code.
4. **Detach.** The daemon spawns the command, inside a Job Object on Windows
   or a process group on Unix, and the client streams its output.
   - A killed client leaves the run going, and re-running the same command in
     the same worktree attaches to it.
   - `cargo heli cancel <run>` kills the whole process tree.
5. **Compile phase.** The environment gets the run pool (§4) and
   `HELI_RUN=<run id>`. heli sets no `-j`: under a jobserver, cargo ignores it.
6. **Test phase** (test class only).
   - heli first builds under the run pool, with `nextest run --no-run <args>`
     or `cargo test --no-run <args>`.
   - It then takes K slots for the test threads. K is the smallest of the
     thread count asked for, the run's share, and the slots free within 5 s.
     With no slot free after 5 s, the tests run with one thread on the run's
     implicit slot and hold no pool slot (G1).
   - It runs the tests with `--test-threads K` (nextest) or
     `-- --test-threads=K` (libtest) and holds the K slots until they finish.
   - The wait happens before nextest starts, so it never counts toward
     `slow-timeout`. nextest's test groups and per-test overrides still apply
     within K.
   - Nested cargo builds inside tests draw from the run pool through the
     inherited environment. They can go beyond K, but stay within the pool.
   - The second invocation re-checks freshness; its cost is to be measured.
7. **Record.** heli records a passing verify or test run whose tree is
   unchanged at the end (§7), and writes a source-state record for its build
   directory (§6.4).
8. **Report.** A header line (slots, share, wait, key), then the tool's
   output in the chosen output mode (see "Output" below), then a footer line
   (duration, peak memory of the process tree, slots held, and the run id for
   `cargo heli log`). Header and footer go to stderr, so stdout stays the
   tool's own, and the footer starts a line of its own. So far the footer
   holds the run id, the exit (`exit <code>`, or on Unix `signal <n>`) and
   the duration:
   `cargo-heli: r17 exit 0 in 41.2 s (output: cargo heli log r17)`. A run
   heli executes directly, without the daemon, has no footer and no record.

**Worktree lock.** heli runs that read a worktree's sources hold its lock
shared, and mutate runs hold it exclusive, so `cargo heli fmt` never rewrites
a file that another heli run is compiling. Bare cargo ignores this lock, so
the rule "one writing agent per worktree" stays.

- **The worktree** is the one `status` shows: the nearest directory holding
  `.git`, else the working directory. Worktrees are compared by canonical
  path, so another spelling of the same directory (on Windows, another case)
  is the same worktree.
- **Order.** Runs take their turns in the order the daemon admitted them: a
  run waits for the runs admitted before it in the same worktree that
  conflict with it, two runs conflicting when either is exclusive. So a
  waiting `fmt` is never overtaken, and a stream of builds cannot starve it.
- **Saying so.** A run that waits prints one line on stderr before anything
  else, naming the runs it waits for:
  `cargo-heli: waiting for r17 (cargo build -p foo) to finish with this worktree`.
  `status` shows the wait (§8).
- **Nested runs.** A run asked for from inside another heli run (its
  `HELI_DEPTH` is above 1) does not wait: the run around it may hold the
  lock, and `cargo heli exec -- cargo heli fmt` would then wait for itself
  forever. It still takes its turn, so later runs wait for it. Since only
  runs that are not nested wait, and only for runs admitted before them, no
  wait closes a cycle.

### Where a run can wait

These are the only waits; nothing else makes a heli run wait for another run.

1. **Test slots:** up to 5 s before the test phase, after which it proceeds
   with one thread.
2. **The worktree lock:** a mutate run waits for heli runs using the same
   worktree, and they wait for it, in the order they came.
3. **Cargo's own build-directory lock:** two runs building the same worktree
   at once are serialized by cargo. The waiting cargo holds no pool slot.

### Command line and contracts

```
cargo heli [--output full|compact|quiet] [-q] [--no-cache] <subcommand> [tool args…]
cargo heli exec [heli flags] -- <command…>
cargo heli status [--json]
cargo heli log <run> [--errors | --test <name> | --tail <n> | --grep <re>]
cargo heli cancel <run>
cargo heli gc [--dry-run]
cargo heli setup [--print] [--autostart | --no-autostart]
cargo heli ensure
cargo heli daemon [--detach]                       (internal)
```

- **Exit codes.**
  - The tool's own code.
  - 127 when the command does not exist, 126 when it cannot be started.
  - 2 for heli's usage errors and refusals: `heli` given as its own
    subcommand, or nesting deeper than 16 (`HELI_DEPTH`). The daemon and its
    detaching starter count as levels too. For `log`, also a run id that is
    not `r<number>` (or a bare number), a bad regular expression, or two
    selectors at once.
  - 1 when the daemon is lost during a run (§3), and when `log` has no
    record of the run or cannot read it.
- **Environment given to runs:**
  - `CARGO_MAKEFLAGS`: the run pool;
  - `HELI_RUN`: the run id, for tools that skip their own host lock under
    heli;
  - `HELI_DEPTH`: the parent's depth plus one, a backstop against heli
    running itself in a loop.
- **Configuration**, a per-user TOML file in the OS configuration directory:
  `%APPDATA%\heli\config.toml` on Windows, `$XDG_CONFIG_HOME/heli/config.toml`
  else `~/.config/heli/config.toml` elsewhere, or `HELI_CONFIG`. The daemon
  reads it as it starts. A value it cannot use keeps it from starting, so
  commands run directly and the daemon's log says why. Keys it does not know
  are ignored, so a file written for a newer heli still works. It holds:
  - pool slots (`[pool] slots`, §4);
  - memory watermarks;
  - disk budget and free-space floor;
  - the store per volume;
  - tool-cache patterns (§6.3);
  - eviction ages;
  - the default output mode;
  - log retention (`[log] runs`, the records kept, default 200, and
    `[log] bytes`, their total size in bytes, default 2 GiB; "Output"
    below).

### Output (G7)

Prior art: [rtk](https://github.com/rtk-ai/rtk) (Apache-2.0). A PreToolUse
hook rewrites an agent's shell commands into `rtk …`, which filters the
output (for `cargo test`: failures only) and keeps the full output for
`rtk recall`. Its cargo rewrite rule matches only
`cargo (build|test|clippy|check|fmt|install)` (`src/discover/rules.rs`), so
rtk passes `cargo heli …` through untouched. heli must keep its own output
small, and output is never filtered twice.

**Modes**, chosen by `--output=full|compact|quiet` or `-q` before the
subcommand, by `HELI_OUTPUT`, or by configuration. Flags before the
subcommand belong to heli; flags after it belong to the tool.

- **full**: the tool's output as it streams, plus heli's header and footer.
  This is the default on a terminal.
- **compact**: the default when stdout is not a terminal, which covers every
  agent tool call.
- **quiet**: one line on success, for example
  `ok  nextest run -p my-crate: 812 passed, 3 skipped, 41 s  [r17]`, and the
  compact output on failure.

**How compact output is produced**, most robust source first:

1. **The tools' own verbosity controls.** Nothing is parsed, so nothing can
   be misreported.
   - cargo: `--quiet` drops the `Compiling …` line per unit (442 of them in a
     cold build of the example workspace).
   - nextest: `--cargo-quiet`, `--status-level fail`,
     `--final-status-level fail`, `--success-output never` and
     `--failure-output final`. All five exist in nextest 0.9.146 (`--help`),
     and each has an environment variable.

   heli adds a flag only when the user's argv does not already set it.
2. **Structured messages.** For compile phases heli passes
   `--message-format json` to cargo and `--cargo-message-format json` to
   nextest, and renders diagnostics from the JSON fields (level, code, primary
   span, message, rendered):
   - Errors: the first 3 in rustc's full rendering, the rest as one short line
     each (`path:line:col error[E0308] message`), 30 at most.
   - Warnings, for `clippy` and `check`, where the findings are the point:
     short lines grouped by lint, 50 at most.
   - Warnings, for `build` and `test`: counts per lint, for example
     `12 warnings: 9 unused_imports, 3 dead_code`.
3. **Text**, only where no structure exists: libtest's own output under
   `cargo test` (its `failures:` blocks), and `cargo heli exec` of arbitrary
   commands, where compact means the first 20 and last 60 lines around a cut
   marker.

Some things never change: the exit code, the stdout/stderr split, and output
heli does not recognize, which passes through.

**Nothing is lost.** The daemon records each run's complete output, for
`cargo heli log` and so that clients can attach to it (lifecycle step 4).
The output modes only change what the client prints, and the footer names
the run. `cargo heli log <run>` prints the record by run id, without
re-running: stdout on stdout and stderr on stderr, byte for byte. At most one
selector picks a part of it:

- `--errors`: errors only;
- `--test <name>`: one test's output;
- `--tail N`: the last N lines;
- `--grep <re>`: matching lines, each matched without its line end.

- **The record** is `runs/<run>.log` in the state directory. The daemon
  writes it as the output arrives, whether or not a client still listens,
  so `log` shows a run in flight up to now. It is a sequence of numbered
  events, each the bytes one read of the run's stdout or stderr returned, in
  the order the daemon received them: exact within each stream, and across
  the two as close as two pipes allow. Selectors work on lines: one stream's
  bytes up to a `\n` (or its unended last bytes), ordered by the event that
  holds their last byte, so `--tail` counts both streams together.
- **Reading.** `log` asks the daemon; when none answers, the client reads
  the record itself, and starts no daemon. Records outlive the daemon.
- **Retention.** The record holds what the tools printed in the chosen mode,
  so the dropped per-unit and per-passing-test lines are not in it. heli
  keeps the last 200 run logs or 2 GB, whichever limit is hit first: when a
  run starts, the oldest records (lowest run number) are deleted first. The
  record of a run in flight is never deleted.

**With rtk installed.** A bare `cargo test` that rtk rewrites to
`rtk cargo test` should still draw from the heli pool, as long as rtk passes
the environment through to cargo; this is still to be verified. Anyone who
prefers rtk's filters to heli's can run `cargo heli --output=full …` under
`rtk test` or `rtk err`, rtk's generic wrappers.

## 6. Build store

### 6.1 Placement

`cargo heli setup` sets `build.build-dir = "<store>/{workspace-path-hash}"`,
either in the user's Cargo configuration or for one repository (open question
2). Verified: `build-dir` supports the templates `{workspace-root}`,
`{cargo-cache-home}` and `{workspace-path-hash}`, the last being a hash of the
canonical manifest path (cargo `src/context/mod.rs`, `custom_build_dir`; Cargo
Book, `build.build-dir`).

The intermediates (`deps/`, `incremental/`, `.fingerprint/`, `build/`) leave
the worktree. `target/` keeps the final artifacts and tool caches: doc,
trybuild, and whatever a project's own tests build there.

`<store>` must be on the same volume as the worktrees. Cargo hardlinks final
artifacts out of the build directory, and across volumes it copies them
instead. The default is `<volume>/.heli/build`, one store per volume that
holds worktrees.

**Risk: tools that look in `target/<profile>/deps`.** Once the intermediates
move, anything that searches `target/<profile>/deps` for rlibs, test binaries
or `.d` files stops finding them. In flui, the candidates are the hot-reload
fixtures, trybuild and the nested-cargo checks. Before phase 2, a real
workspace's full test suite (flui's) must pass with `build-dir` set (§12).

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
   the store. Each unit ever built leaves one; in the example they make up
   114 GB of one worktree.
2. Orphans that no new worktree claimed within 24 h (§6.4), oldest first.
3. All of `incremental/` in build directories idle for 6 h.
4. Tool caches in idle worktrees' `target/`, named by patterns in the
   configuration and nowhere else: for example `tests/trybuild`, and for flui
   `cli-template-check` and `facade-consumer-check`.
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

What makes this sound, verified in cargo's source:

- **Paths cargo tracks.** Cargo's own dep-info encodes each input path
  relative to the package root, the workspace root or the build root. It keeps
  absolute paths only for files outside the workspace, such as registry
  sources and the sysroot, and those are the same for every worktree on the
  host (`src/compiler/fingerprint/dep_info.rs`, `DepInfoPathType`).
- **Build scripts.** Moves of a build script's `OUT_DIR` are handled through
  the `root-output` file (`src/compiler/custom_build.rs`).
- **Unit hashes.** They do not depend on the workspace's absolute path: flui
  saw identical artifact names across worktrees.
- **Freshness** is decided by mtime, including the rule that a unit is stale
  when a dependency is newer than it (`src/compiler/fingerprint/mod.rs`,
  `StaleDependency`).

**What cargo does not track: paths compiled into binaries.** Cargo drops the
environment variables it sets itself from its own freshness tracking:
`OUT_DIR`, `CARGO_MANIFEST_DIR`, `CARGO_BIN_EXE_*`, `CARGO_TARGET_TMPDIR`. Its
source says so: "technically buggy … we won't recompile that if you move the
target directory" (`dep_info.rs` 335). After a rename, a unit that stays fresh
keeps the donor's paths compiled in:

- `env!("CARGO_MANIFEST_DIR")` points into the removed worktree. flui has
  about 20 files that do this, mostly tests locating their fixtures.
- `env!("OUT_DIR")` points under the old build directory path.

rustc still records these values in its own dep-info (`# env-dep:NAME=value`
lines in `deps/*.d`, observed on the example host). So right after the
rename, heli scans the rustc dep-info. It deletes the fingerprint of every
unit with an `env-dep` value under the donor's workspace or the old build
directory path, and cargo rebuilds exactly those units. This applies in both
modes below.

There are two modes:

- **Conservative**, always available. The new worktree's files are newer than
  every artifact, so every workspace member rebuilds. Third-party packages stay
  fresh, except those invalidated by the scan above (a crate that compiles an
  `OUT_DIR` path in, including through `include!`). This is correct by cargo's
  own rules plus the scan. The cost of the `OUT_DIR` invalidations is to be
  measured.
- **Precise.** The donor has a *source-state record*: the blob id and mtime of
  every tracked or untracked file.
  - The record counts only if it was taken after the donor's last build: its
    time is at least the newest mtime in the build directory. Otherwise heli
    ignores it.
  - For each file in the new worktree whose blob matches the record, heli sets
    the recorded mtime. Every other file keeps the current time.
  - Every file with the donor's content then looks to cargo exactly as it did
    in the donor, and every changed file is newer than any artifact. Only the
    changed crates, their dependents and the units the scan invalidated
    rebuild.
  - heli writes records after every run it executes, and the daemon writes one
    when it sees a build lock released.
  - Ignored inputs, such as an `include_str!` of an ignored file, are not
    recorded. Cargo rebuilds when one is missing; one that is present keeps
    the current time and also triggers a rebuild.
  - With `-Zchecksum-freshness` (cargo#14136), cargo compares content itself,
    and precise mode becomes unnecessary. The path scan does not.

**Later: cloning.** On volumes with block cloning (Windows Dev Drive on ReFS,
btrfs, XFS, APFS), recycling can clone instead of renaming, preserving mtimes.
A *live* worktree can then donate, forking a warm build directory. The same
rules apply.

## 7. Result store

For each passing verify or test key, heli keeps a marker
`<state>/verified/<digest>` holding the run id, the worktree and the time. It
keeps at most 256 markers and drops the least recently used first.

**The key.** These rules carry over the lessons of flui#1619, where each one
was found by a failing test.

- **Encoding.** Every field is hashed as its name's length, the name, the
  value's length and the value. Values are raw bytes, never lossy UTF-8:
  environment values, Cargo configuration files, tool output.
- **Tree.** The worktree with every change staged: `git add -A` into a
  scratch copy of the index (`GIT_INDEX_FILE`), then `git write-tree`. The real
  index is never written (`GIT_OPTIONAL_LOCKS=0`).
- **Base.** For a command that diffs against a base, the merge base too.
- **Machine.** Host name, OS and architecture, because a state directory in a
  shared home must not certify another machine.
- **Toolchain.** `rustc -vV`, and the versions of cargo and of the tools the
  run uses (clippy, nextest), each asked in the worktree, so that the pinned
  toolchain answers. A missing tool is keyed as absent.
- **Cargo configuration.** The content of every `config.toml` and `config` on
  cargo's discovery path: `.cargo/` in the worktree and every directory above
  it, then the Cargo home.
- **Argv**, normalized.
- **Environment.** Every variable except a neutral list:
  - Names are case-folded on Windows only.
  - `PATH`, `LD_LIBRARY_PATH` and the `DYLD_*` search paths are keyed without
    their entries inside the worktree, which cargo prepends for the programs
    it runs.
  - Neutral, and so not keyed:
    - job and thread counts holding a value the tool accepts (an invalid one
      fails the run, so it is keyed);
    - `CARGO_MAKEFLAGS`;
    - heli's own variables (`HELI_*`);
    - cargo's terminal styling (`CARGO_TERM_*`);
    - what cargo sets for the program it runs (`CARGO`, `CARGO_MANIFEST_DIR`,
      `CARGO_PKG_*` and the like);
    - locations whose content is keyed elsewhere (`CARGO_HOME`,
      `CARGO_TARGET_DIR`, `RUSTUP_*`);
    - shell bookkeeping (`PWD`, `OLDPWD`, `SHLVL`, `_`);
    - per-session identifiers (terminal and harness session ids).
  - Any variable not on the list, a test control such as `PROPTEST_CASES`
    among them, is keyed, so it misses the cache instead of reusing a result.
- **The plan**, when the command has one, rendered, including the steps it
  skips on this host.

**When the key is computed.** It is checked twice: before the run waits for
anything, and again once admitted, because another run may have recorded it
meanwhile. The tree is snapshotted before planning and again after the steps.
A marker is written only when the run passed and both snapshots match.

**Caveats**, printed on every hit: a flaky test that passed once stays green
for that key (`--no-cache` runs it again), and ignored files are not part of
the key.

Compile runs are never recorded, because their product is the artifact, not
the verdict.

## 8. Observability

`cargo heli status`:

```
pool   32 slots: 21 compiling (4 runs), 6 testing (1 run), 0 withheld   memory 41.2/64 GB, commit 52 %
runs   r17  agent-a64b...  nextest run -p my-crate     test     6 slots  4m12s
       r18  agent-f1c0...  clippy --all-targets        verify   9 slots  1m03s
       r19  agent-a10...   check -p other-crate        attached to r12 (same key)
       r20  agent-a64b...  fmt                         mutate   waiting for r17  12s
disk   D:\.heli\build  148/150 GB (incremental 61, deps 52, ...)  next: incremental >24h, ~40 GB
```

`cargo heli status --json` has a versioned schema (`schema`, now 1). Times
are whole seconds since 1970 (UTC); `status` never starts a daemon.

- `daemon`: `pid`, `protocol` and `started_at`, or `null` when none answers;
- `pool`: `slots`, `free`, `withheld`, `in_use` (`slots − free − withheld`,
  never below 0), or `null` when no daemon answers;
- `memory`: `available`, plus `commit_available` on Windows;
- `runs`: one entry per run, with `id`, `worktree` (the nearest directory
  holding `.git`, else the working directory), `argv`, `class`, `phase`
  (running for a run of one phase; building, testing, done), `slots`,
  `share`, `waiting_since` (`null` unless the run waits), `waiting_for`
  (the ids of the runs it waits for at the worktree lock),
  `attached_clients` and `started_at`;
- `disk`: `stores`, one entry per store, with `path`, `budget`, `used` and
  `by_category`;
- `last_gc`: `at`, and `evicted`, one entry per path with its tier and bytes.

There is also a rotating log of admissions, loans, withholdings, top-ups,
drains and evictions, and `cargo heli gc --dry-run`.

## 9. Adoption in flui (worked example)

1. Run `cargo heli setup`: pool environment for the agents, `build-dir`, a
   store on D:.
2. Update AGENTS.md: `cargo heli nextest run -p <crate>` while iterating, and
   `cargo heli exec -- cargo xtask check-changed` before a PR.
3. In xtask, skip `host_lock` when it runs under heli (`HELI_RUN` is set) or
   when the pool variable is present. The fixed `CARGO_BUILD_JOBS=28` from
   flui#1619 has no effect under a jobserver (§4).
4. `cargo xtask worktree prune` keeps handling branches. Deleting `target/` is
   no longer its job.

## 10. Failure modes

| Failure | Effect | Handling |
|---|---|---|
| Daemon not running | A bare cargo warns and uses its own pool, as today. On Unix, a FIFO left by an earlier daemon still opens but holds no bytes, so a bare cargo runs one job at a time, without a warning | `cargo heli` starts the daemon, which fills the FIFO again (§4); logon or session hook |
| Daemon unreachable, silent for 5 s, of another protocol version, or older and without a request the client sends | `cargo heli` cannot hand over the run, or the request | The command runs directly (`log` reads the record itself) after one warning line; a newer client asks an older daemon to exit once idle (§3) |
| Daemon lost during a run | The client cannot tell whether the command finished | The client exits 1 and does not run the command again |
| An agent harness stops the command that started the daemon | Observed (Claude Code, Windows): it kills the command's process tree, found through parent processes | The daemon is started through a short-lived intermediate, so it descends from no client (§3) |
| An agent harness terminates its whole job object | On Windows, a daemon started on demand inside a job that refuses breakaway (Claude Code's) dies with it, and every run with it | Not seen in normal operation: that job holds the whole app and has no kill-on-close limit. The logon task starts the daemon outside any agent's job |
| Daemon started outside the user's interactive session (a service, or a task that runs while the user is logged off) | Runs lose the desktop: tests that drive real windows fail | The daemon runs only in the interactive session: started by a client, or by a logon task restricted to the logged-on user |
| Daemon crashes mid-build | Running cargos keep their handles: a Windows semaphore lives while any handle is open, and a FIFO keeps its bytes while any process holds it open (its path persists either way) | The restarted daemon adopts a pool still held, as it is; a FIFO that nobody held has lost its bytes, and the daemon fills it again (§4) |
| Slot leak by bare cargo | The host pool shrinks | Top-up at quiescence, bounded by the target (§4) |
| Run killed while holding slots | none | The run pool's loans go back in full (§4) |
| Unix FIFO holds more than the target | Oversubscription | The daemon drains the surplus (§4) |
| Memory spike | New jobs wait | Slots are withheld; running jobs finish |
| GC races a build | none | Try-lock, rename to trash, skip busy directories |
| Recycled unit with a donor path compiled in | Tests read the removed worktree | The rustc dep-info scan invalidates the unit (§6.4) |
| Recycled directory with a stale record | Could link stale code | A record counts only if it is newer than every file in the build directory; otherwise conservative mode |
| Tools looking in `target/<profile>/deps` | They miss moved intermediates | Validated on a real workspace before phase 2 (§6.1) |
| False green from the cache | A flaky pass is reused | Printed caveat; `--no-cache` |
| heli runs itself in a loop | Unbounded processes | `heli` is refused as a subcommand; `HELI_DEPTH` stops at 16 |
| Another logon session (SSH, a service) | On Windows, a separate session namespace, so a separate pool, and a separate daemon (its endpoint and lock carry the session id) | Documented; a global namespace needs admin rights (later) |

## 11. Upstream map

| Issue | heli's part | When it lands |
|---|---|---|
| cargo#5931, per-user artifact cache | §6.4 recycling, one owner at a time | heli's store can defer to it |
| cargo#16147, build-dir out of the workspace | §6.1 | heli stops setting it |
| cargo#14136, checksum freshness | §6.4 precise mode | Precise mode becomes cargo's; the path scan stays |
| cargo#4282, #17508, finer locking | Not solved: one build directory per worktree, and the pool makes waiting cheap | Concurrent runs in one worktree get faster |
| cargo#6747, jobserver token while waiting on a file lock | Irrelevant under the pool (§4) | none |
| cargo#13136, target GC | §6.3 | heli's eviction ladder can call it |
| cargo#14774, feature unification | Out of scope (cargo-hakari) | Fewer duplicate units for §6.3 |
| rust#113349, parallel front end | The pool already lends rustc its threads | none |

## 12. Plan

- **Phase 0**, no code, one day, tracked as its own issue. A semaphore kept
  open, `CARGO_MAKEFLAGS` in the agents' environment, flui's host lock turned
  off. It measures whether a shared pool alone ends the queues and bounds
  memory. Known gaps: no governor, no leak top-up, no run pools, test threads
  unbounded.
- **Phase 1.** Daemon, host pool, run pools and broker, governor, bounded
  top-up, `cargo heli build/check/clippy/test/nextest/exec`, detach and
  attach, `status`, output modes with the run log and `cargo heli log`. Exit
  criteria, on flui with at least 4 agents:
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
  suite passes with `build-dir` set (§6.1).
- **Phase 3.** Result store and single-flight.
- **Phase 4.** Donation by block cloning, priorities beyond equal shares,
  per-job memory calibration.

Measure throughout:

- time to the first rustc;
- run wall time;
- pool utilization;
- broker latency;
- minimum available memory and commit;
- disk by category per day;
- units rebuilt after a recycle;
- output bytes per run in each mode.

## 13. Open questions

1. The name.
2. `build-dir` in the user's Cargo configuration affects every project on the
   machine. Should it be opt-in per repository instead?
3. The default budget B (proposed: 150 GB on D:) and floor F (proposed: 50 GB).
4. Is a pool per logon session enough, or do agents run in other sessions
   (SSH, services)?
5. Should the default without a terminal be `compact` or `quiet`?
