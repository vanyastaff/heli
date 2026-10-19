# heli: one machine, many agents, one cargo budget

Status: design draft, updated 2026-10-16. "heli" is a working name.

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

FLUI is one motivating consumer. The user's workload spans many projects
with the same contention and disk-growth problem, currently spread over
multiple PCs. Acceptance therefore needs simultaneous runs from different
repositories on one host, as well as multiple worktrees of one repository.
Repository opt-in for build placement (§6.1) does not partition the shared
CPU/memory pool or give each repository an independent disk budget.

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

The current acceptance priority is independent progress across projects and
worktrees. Disk and RAM caps are secondary to that goal. Their safety
contracts still apply, but an experiment showing concurrency does not by
itself establish a need for disk optimization or prove a resource bound.

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
  fails a run, except when the runtime/GC safety interlock cannot be acquired
  (§6.3). That case refuses the managed command with a diagnostic: silently
  starting it unprotected could let another GC delete its runtime assets.
  An invalid explicit activity namespace is a configuration error. This is
  a deliberate safety exception to unconditional fallback.
- **G6, agent-proof.** A killed client neither kills nor wastes its run, and
  identical runs are not repeated.
- **G7, small output for agents.** An agent reads what it needs to act: the
  verdict, and on failure the failures. Everything else stays retrievable
  without running the command again.

**Acceptance status, 2026-10-16.** G1, G3 and G4 above are design goals,
not unconditional properties established by the current acceptance runs.
Independent projects can complete beside long-running work (§12), but
cold preparation and Cargo's own waits can delay the first compiler. The
recorded full FLUI run took 184.677/210.297 seconds to that point; these
are end-to-end delays, not separately measured download times. That run
does not pass §12's strict ten-second first-compiler criterion.
An existing worktree's retained home and an eligible orphan build directory
can be reused; an entirely new worktree without a donor is not guaranteed
to start warm. Artifact GC reclaims eligible regenerable data and protects
what runs in flight may use: their workspaces' directories, and whatever
changed since the oldest of them started (§6.3). Protected residual outputs
can exceed the budget even at idle, and native source homes are outside
that budget (§6.3, §6.6).
The recorded 2-GiB FLUI acceptance remains a failed budget check. These
limits must not be advertised as solved merely because concurrent commands
or narrower acceptance tests pass.

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
by the logon registration `cargo heli setup --autostart` makes: a task that
runs only while the user is logged on, or its systemd and launchd
counterparts (§4, "Autostart"). Once that registration exists, clients start
the daemon through it too (below).

**One daemon per user**, and on Windows per logon session: a lock in the
per-user state directory (`%LOCALAPPDATA%\heli`, `$XDG_STATE_HOME/heli` or
`~/.local/state/heli`) admits one. Named pipes are visible machine-wide,
unlike semaphores, so the Windows endpoint carries the session:
`\\.\pipe\heli-<hash of the state directory>-<session id>`. `HELI_STATE_DIR`
and `HELI_ENDPOINT` override both, and `HELI_POOL` and `HELI_CONFIG` the host
pool (§4) and the configuration file (§5), which is how tests isolate their
daemons. A client resolves them, relative paths against its own working
directory, and passes them to the daemon it starts. `HELI_SCHTASKS`,
`HELI_SYSTEMCTL` and `HELI_LAUNCHCTL` name the programs heli runs as
`schtasks`, `systemctl` and `launchctl` (§4, "Autostart"); tests point them
at a fake, so that none registers anything with the real ones.

On Windows, a process an MSIX-packaged app starts sees `%LOCALAPPDATA%`
and `%APPDATA%` through package virtualization: the default state
directory and configuration are then the app's own, under
`...\Packages\<family>\LocalCache\`. Such a child need not have the
package's identity (`GetCurrentPackageFullName` can say it has none). The
path the file system gives for the file is the test, which `doctor` and
`ensure` run: a canonical path in a package's `LocalCache` that the spelled
one is not. Both then say to set `HELI_STATE_DIR` and `HELI_CONFIG` outside
`AppData` for every process (#70).

**The endpoint.** On Windows, a pipe whose DACL allows the user's SID alone
(without one, Everyone may read), which rejects remote clients and is
created as the first instance, so the daemon never joins a pipe someone else
created. The daemon sets its owner to the user's SID, elevated or not (an
elevated process would otherwise give it to the Administrators group). A
client connects at identification level and talks only to a pipe owned by
its own user's SID, elevated or not: never to one owned by the
Administrators group, which any administrator's elevated process can
create. On Unix, a socket with mode 0600 in the 0700 state directory; a
client talks only to a socket owned by its uid.

**The protocol** starts with a handshake frozen across versions: `HELI` and
the protocol version, each way. On a mismatch the client runs the command
directly, with one warning line. A client newer than the daemon also asks it
to retire: the daemon takes no new runs and exits once its runs in flight
have finished, so the next call starts a current daemon. As it exits it
first stops listening and releases the state directory's lock: the process
itself may take a while to be gone (seen to take over 10 s on a loaded
Windows host), and until then it would hold its endpoint, where a client
would connect and get no answer, and the lock, which a new daemon needs.
An older client asks
nothing, so two installed versions never take turns retiring each other's
daemon. The current protocol is 2: a dependency-epoch request must never be
accepted by an old daemon that ignores its optional ownership field. A daemon
answers a request it does not know (one a newer client added, such as `log`)
with "unsupported", and the client treats that like a mismatch from an older
daemon. It asks the daemon to retire and does without it, after one warning
line: it runs the command, or reads the run log, itself. Fields added to a
message are optional, so both sides still read each other's: a run request
without a class (§5) comes from a client older than run classes, its run
counts as exec, and the daemon does not send it the "waiting" message, which
it could not read. Values added to a set are read as unknown: a run's phase
or class in `status` that the client does not know shows as `unknown` and
as no class, and a reply it does not know is skipped while it follows a run
(elsewhere it is an answer it did not expect). A daemon still sends a new
reply only to a client whose request says it reads it.

**Starting on demand.** When no daemon answers, the client starts one, and
any client falls back to running the command directly when the daemon does
not start, listen or answer within 5 s.

- The client starts `cargo heli daemon --detach`, which starts the daemon and
  exits once it listens. The daemon's parent is then gone, so no tree walk
  from the client reaches it; the client goes on only once that stage has
  exited, within the same 5 s. Agent harnesses stop a command by killing its
  process tree found through parent processes: observed with Claude Code on
  Windows, where stopping a command killed the client, the daemon it had
  started directly, and that daemon's run.
- A daemon that finds the lock held exits at once (code 3). The holder may
  be starting, and will listen; or going away, killed or retired, and
  holding the lock until its process is gone, after its endpoint has gone.
  So while nothing listens, `daemon --detach` starts the daemon again, after
  a pause that grows from 50 ms to 400 ms, within the same 5 s. A daemon
  that finds its pipe already there on Windows (a predecessor's last
  connections, as its process goes) tries again to create it within 5 s too.
- An endpoint that is there but busy (on Windows every instance of the pipe
  taken: between two of the daemon's accepts, or on a host so loaded that
  the daemon has not run since) is a daemon, not an absent one: the client
  waits for a free instance and tries again, within the same 5 s as for an
  answer, and only then runs the command directly. The daemon creates the
  next instance before it serves the client it accepted, and creates its
  pipe only once it is ready to accept.
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
  (§10). Hence the next point.
- **Through the registration.** When `setup --autostart` has registered the
  daemon for the client's own program and locations (its record in the state
  directory says which), the client does not start the daemon itself: it asks
  the service manager to (`schtasks /Run /TN <task>`,
  `systemctl --user start heli.service`,
  `launchctl kickstart gui/<uid>/<label>`), with no stdio, and waits for the
  daemon to listen within the same 5 s. That daemon belongs to the service
  manager, outside any agent's job, process tree or control group. A client
  that asked never also starts one: if none listens in time, it runs the
  command directly. When there is no such record (no registration, another
  program such as a second install, other locations), or the service manager
  refuses or does not answer within 5 s (the registration was removed behind
  heli's back), the client starts the daemon itself, as above.

**A run** is a child of the daemon, started in the client's working
directory with exactly the client's environment and no stdin, as the first
process of a process tree of its own (§5, lifecycle step 4). Its stdout and
stderr stream back to the client through the run's record, and the client
exits with the run's code (on Unix, of its signal). The run belongs to the
daemon: a client that goes away does not stop it. A run is over when its
process exits: output still arriving within 500 ms is forwarded, and
processes it left behind holding its pipes do not hold the client. The
daemon tells the client when it accepts the run, and again when the run's
command has started (its first process, after any wait for the worktree
lock or for recycling, §5): a client that loses the daemon in between runs
the command itself, after one warning line, since the command never
started. Once the daemon has said the command started, the client never
runs it itself: if the daemon is lost then, it exits 1. A client asks for
the second message in its run request, and a
daemon older than it says in its first that none follows: with one, the
run counts as started once accepted, as before.

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
and the ones cargo opens do not) or `FIONREAD` on a nonblocking file
description of the FIFO open for reading only: on one open for reading and
writing, macOS answers for the FIFO's write side, which never holds a byte
(its `fifo_ioctl` asks the read socket, then the write socket, and the last
answer wins). Counts are snapshots.

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

**Lifetime.** A pool lives while some process holds it, and no longer. Once
its daemon is gone and nobody holds it, a bare cargo pointed at it (§4
"Reach") cannot open it, prints cargo's warning, and builds with a pool of
its own, as without heli.

- A semaphore goes with its last handle.
- A FIFO's path stays, and once nobody holds it, it has no bytes: a bare
  cargo would open it, find no slot, and build one job at a time, without a
  warning. So each daemon starts a **warden** as it opens the pool:
  `cargo heli daemon --warden`, a process of its own (its own process group,
  no stdio but the read end of a pipe whose write end only the daemon
  holds). However the daemon ends (it retires, is killed, crashes), the
  warden reads the end of the pipe. That end comes as the kernel closes the
  dying daemon's files one by one, when the pool's lock (below) may still be
  held, as by a daemon that owns the pool; so the warden first waits (at
  most 5 s) until the daemon, whose process id it is given, is no longer its
  parent: the process is gone then, with every file it held. It then
  removes the FIFO as soon as no process holds it (the `ENXIO` probe above),
  looking again every 250 ms while one does: a cargo still building keeps
  the pool alive, as it would keep a semaphore.
- **The pool's lock**, `<fifo>.lock`, keeps a warden from removing a FIFO
  that a daemon owns. A daemon takes it before it opens the FIFO and holds
  it for life (it waits up to 1 s for it, since a warden holds it for a
  moment; longer means another daemon owns the pool, and this one does not
  start). A warden holds it only while it looks, and exits once a daemon
  owns it again.
- A warden killed together with its daemon (a kill of both, a power loss)
  leaves the FIFO behind. The next daemon fills it again (Adoption, above);
  until then, a bare cargo builds one job at a time. A session-start hook
  that runs `cargo heli ensure`, or the logon task, keeps that window short.

### Run pools

Each heli run gets a **run pool** of its own: a second semaphore or FIFO,
which the run's processes see through `CARGO_MAKEFLAGS` instead of the host
pool. It is named after the host pool and the run, `<pool>-r17` (on Unix a
FIFO beside the host pool's, which the daemon removes when the run ends), and
starts with no free slot. A run pool is never adopted: a semaphore of that
name that some process holds is refused, and a FIFO left at the path by an
earlier daemon is replaced. A run whose run pool cannot be made draws from
the host pool, and the daemon's log says why.

A broker in the daemon moves slots between the host pool and the run pools,
every 20 ms. It cannot see the processes blocked on a pool, so a run whose
pool has no free slot counts as **waiting** for one; a run is **active**
while it uses a slot or waits. Each run has a **weight**, its priority's
(below): interactive 4, normal 1, background ¼. A run's **share** is the
capacity (the host pool's free slots, plus the slots the active runs hold)
times its weight over the sum of the active runs' weights; with equal
weights, the capacity divided by the active runs. A run that is not yet
active counts itself in, with what it holds. Withheld slots and slots bare
cargo holds are not in the capacity. Runs are compared by the slots they use
for their weight, `in_use / w`.

- **Lend.** A free slot of the host pool goes to the waiting run with the
  fewest slots in use for its weight; between equals, the heavier, then the
  oldest. While another run waits, a run
  already using its share or more gets none. A run alone can borrow every
  slot. A process waiting on a semaphore takes a slot as it is put in, so in
  the same pass the broker lends again to a run that still waits.
- **Return.** Free slots in a run pool beyond one go back to the host pool
  once they have stayed free for a tick. Sooner, they may be slots that a
  run with work pending just gave back, which its processes waiting on the
  pool have not yet woken to take: a process blocked on a FIFO takes a
  byte only once the scheduler runs it, which on a busy host can be after
  the next count. Returned, such a slot would go to another run and then be
  taken back (seen on a macOS CI runner: a third slot taken back where two
  balance the runs). An idle run keeps one, for the next process that asks.
  That one goes back too when the host pool has none free and a run that
  waits would, with it, still use no more slots for its weight than this
  one.
- **Take back.** A run with work pending never shows a free slot: each slot
  one of its processes gives back is taken at once by another waiting on the
  same pool. So while the host pool has no free slot and a run waits, a run
  using more than its share, which with one slot fewer would still use as
  many for its weight as the neediest waiting run would with one more (with
  equal weights, at least two slots more than it), gives slots back as its
  processes finish with them. The
  broker waits on that run's pool in line with its processes (a thread
  blocked on the semaphore and a cancel event, or in a blocking read of the
  FIFO), and the slot it gets goes to the host pool and on to the poorer run.
  That wait can be long after the decision, so the broker counts the pools
  again as it gets the slot, and decides again: if no poorer run waits at
  that moment (its process is between two slots, or has not yet taken a
  slot just lent to it), the slot goes back to the run it came from. Sent
  to the host pool, it would be lent straight back to that run, the one
  run waiting, and count as taken back (what still took a third slot back
  on macOS once Return waited a tick). Where it goes then follows
  from that same count, so a slot taken back always goes to another run.
  Nothing running is interrupted. The margin (of two, with equal weights)
  keeps an odd split (3 and 2) from moving back and forth. Cargo keeps its tokens from one job to
  the next while it has jobs ready (`job_queue`, `spawn_work_if_possible`),
  so a cargo's slots come back when it has no job ready and as rustc's
  codegen and build scripts' own jobs end; clients that take a token per job,
  as make and cc-rs do, give them back at every job.
- **Exact accounting.** A run uses `lent − free(run pool)` slots, plus its
  test slots (§5). When a run ends, or is cancelled or killed, everything lent
  to it goes back to the host pool at once, whatever its processes held.
  Leaks are impossible for heli runs; slots lent by a daemon that died come
  back through the top-up.
- **Latency.** For every slot it lends, the broker records how long the run
  may have waited for it: from when its pool was last seen with a free slot
  (an upper bound). The `loans` event gives each run's median and longest
  wait when it ends (§8). A waiting process gets a slot within one broker
  tick while the host pool has one free. Measured on the example host
  (Windows, N = 8, `cargo heli build` of heli itself, 7.3 s): 49 loans, at
  most 8 slots in use, every wait at most 20.6 ms, one tick. Cargo's
  jobserver helper asks for one token at a time and is not yet waiting
  again when the broker counts once more, so a run gains about one slot
  per tick: 32 slots take some 0.65 s, short next to a rustc job.

Bare cargo, started without heli, still draws from the host pool directly.

### Priorities

A run's priority sets its weight in the shares (#93):

| Priority | Weight | For |
|---|---|---|
| `interactive` | 4 | a person waits at a terminal |
| `normal` | 1 | agents |
| `background` | ¼ | long and not urgent: mutation testing, full sweeps |

The client chooses it: `--priority <level>`, else `HELI_PRIORITY`, else
`background` for a tool whose built-in profile says so (cargo-mutants),
else `interactive` when stdin and stdout are terminals and neither
`CLAUDECODE` nor `CI` is set, else `normal`. A `HELI_PRIORITY` that is no
priority is a usage error (2). The run request carries it (absent:
`normal`, so an older client's runs are normal), and a test run's build and
test phases keep it. A client that attaches to a run in flight leaves the
run's priority as it was.

With the pool saturated by two background runs, an interactive run gets the
next slot one of them gives back (Take back) and settles at 4/4.5 of the
capacity; the background runs keep one slot each. Weights change only how
the broker shares: a run alone still borrows every slot, a background run
gets slots whenever nobody else waits, and test slots come from the weighted
share (§5). The memory governor treats every priority alike.

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
  before RAM is full. It samples once a second: Windows
  `GlobalMemoryStatusEx` and `K32GetPerformanceInfo` (system-wide
  `CommitLimit − CommitTotal`), Linux `MemAvailable` in `/proc/meminfo`,
  macOS `host_statistics64`.
  - Below the low watermark (default: 10 % of RAM or 6 GB, whichever is
    larger), it withholds slots: it acquires one per sample from the host pool
    as they free up, down to 1 slot left. Available commit below the low
    watermark counts as pressure too.
  - Above the high watermark (the low one plus 4 GB), it returns them, all
    at once; available commit, where there is one, must be above it too.
    Between the two nothing changes, so it never swings back and forth.
  - Jobs already running are never interrupted, so pressure eases as they
    finish.
  - `[memory] low_watermark_mb` and `high_watermark_mb` in the configuration
    set the watermarks, in megabytes (2^20 bytes); the high one must be above
    the low one. `status` shows the last sample, the watermarks and the
    slots withheld (§8).
- **Bounded top-up.** Only bare cargo can leak a slot: a process killed while
  holding one, and semaphores and FIFOs have no owner. The host pool's target
  of free slots is `N − withheld − Σ lent`, where Σ lent counts the test
  slots held for runs (§5) too.
  - The daemon raises the free count to the target only at quiescence,
    observed at its samples for 2 s: no heli run active, and either no
    process but the daemon holds the pool open, or no rustc, rustdoc,
    clippy-driver, build script or cargo is alive. A pool nobody else holds
    can have no slot that will come back, whatever compiles elsewhere on the
    machine (other users' pools, other agents' builds without heli); the
    process scan alone would wait for every compiler in the session. A
    cargo counts because it takes a slot before it starts the job the slot
    is for: between the two it holds a slot while no compiler runs, and a
    top-up then would make the pool exceed N once it gives the slot back
    (#26). The price: while some process holds the pool, any live cargo
    (`cargo watch`, a `cargo run` of a server) delays the top-up.
  - The top-up counts the free slots first, and only then confirms the
    quiescence once more, before it gives any: a cargo that takes a slot
    after the count is alive at the confirmation, and the top-up waits for
    it; one that takes a slot later takes one the count had as free, which
    the top-up does not replace. Looked at the other way round, a cargo
    that started between the look and the count would have taken slots the
    top-up then replaced (a stress test of killed cargos on a macOS runner
    held 6 slots of 3 at once).
    - Who holds the pool: on Windows, the semaphore's handle count
      (`NtQueryObject`) above the daemon's one; on Linux, a descriptor of
      the FIFO under `/proc/<pid>/fd` of the user's processes; on macOS, the
      same through `proc_pidinfo`.
    - The process scan: on Windows `WTSEnumerateProcessesW`, the processes
      of the logon session (where the semaphore lives); on Linux
      `/proc/<pid>/comm` of the user's processes, cut to 15 bytes
      (`build-script-bu`); on macOS the file name of `proc_pidpath`.
    - Windows: `NtQuerySemaphore` for the count, then `ReleaseSemaphore`.
      Unix: `FIONREAD` on the FIFO, then writes.
  - The pool never grows beyond the target. The daemon drains, at any time,
    every free slot above the target. A FIFO has no maximum, and a
    semaphore's maximum count N bounds it only while nothing is withheld or
    lent. A token that a cargo still held during a top-up (between taking it
    and starting the job, or held by a jobserver client the process scan
    does not see) then comes back as surplus and is removed.
  - The broker counts a loan as it takes the slot from the host pool, and
    stops counting it as it gives the slot back, each as one step under the
    host pool's lock; the top-up and the drain read Σ lent under that lock
    too. Read apart, a slot on its way back between the two reads would look
    like a surplus, and the drain would remove it.

### Reach

- A run through `cargo heli` gets its run pool from the daemon.
- A bare cargo gets the host pool from the environment that
  `cargo heli setup` writes. This is the safety net for G2: an agent that
  forgets heli still shares the pool. A bare cargo that finds the pool says
  so: cargo warns that it ignores a `-j` given beside an external jobserver.
- Without a running daemon, a bare cargo prints cargo's jobserver warning and
  runs as it does today (Lifetime, above). `cargo heli` starts the daemon on
  demand, and so does `cargo heli ensure`, for a harness's session-start
  hook. `cargo heli setup --autostart` can also register it to start at
  logon (Task Scheduler, systemd `--user`, launchd; "Autostart", below).

**`cargo heli setup [--print]`** offers `CARGO_MAKEFLAGS`, with the value
every run gets for the host pool (`-j --jobserver-auth=<semaphore>`, or
`fifo:<path>`), and writes it only with consent:

- **Where.** Claude Code's user settings, `env` in `settings.json` in
  `CLAUDE_CONFIG_DIR`, else in `~/.claude` (on Windows `USERPROFILE`, where
  Claude Code looks), offered only when that directory exists. setup
  rewrites the file as Claude Code does, as JSON with two-space indentation,
  keeping every other setting in its order. And the profile of the shell
  `SHELL` names: `~/.bashrc` for bash, `$ZDOTDIR/.zshrc` (else `~/.zshrc`)
  for zsh, fish's `config.fish` (`set -gx`), `~/.profile` for any other.
  setup adds two lines at its end, a comment that marks them as heli's and
  the assignment, and replaces lines an earlier setup wrote with another
  value. On Windows, Git Bash, MSYS2 and Cygwin set `SHELL`; without it
  (PowerShell, cmd) setup prints the PowerShell line that sets the variable
  in the user environment. And on Windows the user environment itself
  (`HKCU\Environment`), which every process the user starts reads
  (terminals, editors, rust-analyzer, Task Scheduler's tasks): setup shows
  the assignment, asks `Set it there? [y/N]` with the same consent, writes
  the value as `setx` does and broadcasts the change, so that programs
  started afterwards get it (#82).
- **Consent.** For each file, setup first shows exactly what it would write:
  the file's whole new content ("would become:") or the lines it would end
  with ("would end with:"), followed by an empty line. It then asks
  `Write <path>? [y/N]` and reads the answer from stdin, so a script can
  answer. Only `y` or `yes` writes; anything else, the end of input, or no
  input at all leaves the file as it was. A file that already sets the
  value is left alone without a question, and so is one setup cannot edit
  safely (settings that are not a JSON object), with the reason.
- **`--print`** shows the same, asks nothing and writes nothing.
- setup exits 1 when a file it was told to write cannot be written.

**`cargo heli ensure`** reaches the daemon, starting it as a run would, and
exits 0 without a word, since a session-start hook's output may land in the
agent's context. With a daemon running it only connects and exchanges
versions. When no daemon can be reached, or the one there speaks another
protocol version (an older one is asked to retire, as for a run), it exits
1 with one line saying why.

**`cargo heli stop [--now [--yes]]`** asks the daemon to retire, as a newer
client does (§3): it takes no new runs (they run directly, after one
warning line) and exits once its runs in flight have ended; the daemon
answers with how many. A daemon older than `stop` answers "unsupported",
and the client asks it to retire as after a mismatch. With `--now` the
client cancels the runs in flight (as `cancel` does) after asking
`Cancel N run(s) in flight (r1, …)? [y/N]` on stderr, read from stdin;
`--yes` does not ask, and anything but a yes stops nothing (exit 1). With no
daemon, `stop` says so and exits 0. **`cargo heli restart`**, with the same
flags, stops the daemon, waits until it has exited (or another client has
started a new one), then starts one as `ensure` does, through the autostart
registration when its record names this program, and names its pid and
version. `status` shows the daemon's version and program (`version` and
`executable` in the JSON document, `null` from a daemon older than them),
and the text form says when the daemon is older than the client, with
`restart` as the fix (#87).

### Autostart

**`cargo heli setup --autostart`** registers the daemon to start when the
user logs on, in their own session, never as a service and never while they
are logged off: a daemon there would have no desktop. Clients then start the
daemon through the registration too (§3, "Starting on demand").

- **Windows**: the Task Scheduler task `heli-daemon-<hash of the state
  directory>`, in the root folder as other per-user tasks are (OneDrive's).
  Its definition, `autostart-task.xml` in the state directory (UTF-16, as
  Task Scheduler exports tasks), is registered with
  `schtasks /Create /TN <task> /XML <file> /F`.
  - A logon trigger for the user's SID, and the principal `InteractiveToken`
    with `LeastPrivilege`: it runs only while the user is logged on, in
    their interactive session, not elevated, and no password is stored.
  - It overrides the defaults that would hurt every build: `Priority` 5, a
    normal priority class (the default 7 is below normal, with low I/O and
    memory priority, which every run would inherit); `ExecutionTimeLimit`
    `PT0S` (by default a task is stopped after 72 h); and
    `DisallowStartIfOnBatteries` and `StopIfGoingOnBatteries` false.
  - `MultipleInstancesPolicy` `IgnoreNew` and `AllowStartOnDemand`, for
    `schtasks /Run`.
  - The action is `cargo-heli daemon --detach`. Task Scheduler gives a
    console program started in the user's session a console window; the
    detaching stage holds it only until the daemon listens, and the daemon
    gets a console without a window, as when a client starts it. The
    detaching stage creates the state directory if need be.
- **Linux**: the systemd user unit `heli.service` in
  `$XDG_CONFIG_HOME/systemd/user` (else `~/.config/systemd/user`), enabled
  as `systemctl --user enable` would, by the link
  `default.target.wants/heli.service` beside it, then
  `systemctl --user daemon-reload`. It runs `cargo-heli daemon` in the
  foreground (a detaching start would end the unit, and systemd would stop
  the daemon with it), with `SuccessExitStatus=3`, the exit of a daemon that
  finds another one running, and `KillMode=process`, so stopping or losing
  the daemon does not kill its already spawned runs. The user manager lives
  from the first login to the last logout; heli does not enable lingering.
- **macOS**: the launchd agent `io.github.vanyastaff.heli`, in
  `~/Library/LaunchAgents`, loaded at once with
  `launchctl bootstrap gui/<uid> <file>`: `cargo-heli daemon` in the
  foreground, `RunAtLoad`, `LimitLoadToSessionType` `Aqua`,
  `AbandonProcessGroup`, and `ProcessType` `Interactive`: without it launchd
  throttles an agent's CPU and I/O, and every build's with it. No
  `KeepAlive`: a daemon that exits stays down until a client starts it.

Rules:

- **The locations every process has.** A task cannot hand its program
  variables, so the registration starts the daemon a plain `cargo heli` of
  the user finds, and setup refuses to register or unregister (exit 1, one
  line saying why) while `HELI_STATE_DIR`, `HELI_ENDPOINT`, `HELI_POOL` or
  `HELI_CONFIG` is set for setup's process only. On Windows a variable set
  in the user environment (`HKCU\Environment`) with the value setup's
  process has is every process's, the logon task's included, and does not
  refuse: these are the locations §3 tells a packaged app's user to set
  (#70, #82). The refusal names the difference. The unit and the agent name
  the locations setup resolved, so that the service manager's environment,
  which lacks what a shell's profile sets, cannot move them.
- **The record**, `autostart` in the state directory, names the program and
  the locations setup registered. A client starts the daemon through the
  registration only when the record names its own (§3). setup removes the
  record before it changes anything and writes it once the registration is
  complete, so no client relies on half of one.
- **Again.** setup asks the service manager whether the registration is in
  place with the same definition (`schtasks /Query`, the unit and its link,
  `launchctl print`) and if so says "registered already" and changes
  nothing. A registration of another definition (heli installed elsewhere)
  is replaced; on macOS that unloads the old agent first, which stops a
  daemon launchd started.
- **`--no-autostart`** removes the registration, its definition and the
  record (`schtasks /Delete`; the unit, its link and a reload;
  `launchctl bootout`, which stops a daemon launchd started), and finds
  nothing to do the second time.
- **Consent** is the flag: setup shows the definition and each step, then
  takes them, and asks nothing. Without `--autostart`, setup registers
  nothing and does not reach the service manager. `--print` shows the steps
  of a first registration, or of a removal, and takes none.
- setup exits 1, with one line naming the command and what it said, when a
  step fails.

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
  `exec --verify` explicitly declares a read-only verification instead. It
  uses verify's complete input key and defaults to live sharing without
  persisted verdicts. Only use it for commands whose result is a verification,
  such as `cargo xtask check-changed`; it does not classify a wrapper's steps.

### Tool profiles

Beyond cargo's own subcommands, heli ships built-in profiles for popular
cargo tools (spec #38). They apply to everyone who installs heli, with no
configuration. Each is one declarative record in `TOOLS` (`src/class.rs`): its names, a summary for `doctor`, its options that take a value, and rules from a condition on the arguments (the first word, any word, or an argument) to a class or to a direct run. Adding a tool is adding a record. A tool that
is missing gets a profile in heli itself, so every user gets it, not only
one project. Users and repositories add profiles of their own (below).
Tools' caches are the build store's: cargo-mutants' scratch copies are the
ladder's tier 0, and cargo-llvm-cov's `target/llvm-cov-target` a built-in
tool cache (§6.3).

**Usage memory.** Every run carries where its class came from, its
`profile`: `cargo` (cargo's own subcommands), `built-in`, `user`,
`repository`, `unknown` (a tool no profile names) or `exec`.
`cargo heli status` shows it after the class, as in `exec (unknown)`, for all
but `cargo` and `exec`. The event log records it with each admission, and
with each run a client makes directly in the pool (`direct`).
`cargo heli doctor` counts the `unknown` runs by tool and lists the ten used
most (`unknown_tools` in `--json`). That shows which profile is worth
writing, for the user, the repository, or heli itself.

| Tool | Profile |
|---|---|
| `add`, `remove`, `rm`, `update`; cargo-edit's `upgrade`, `set-version`; `release`, `autoinherit` | mutate |
| `sort` | verify with `--check` or `-c`; else mutate |
| `hakari` | `generate`, `manage-deps` mutate; `verify` verify; else exec |
| `machete`, `shear` | verify; mutate with `--fix` |
| `udeps`, `public-api` | verify (the `+toolchain` is in the argv, so in the key) |
| `miri` | `test`, `nextest` verify, keyed by the toolchain; else exec |
| `ndk` | `build` compile, `check`/`clippy` verify; keyed by the ABIs and API level (argv), the NDK's location (environment) and version (`source.properties`); else exec |
| `zigbuild` | compile, keyed by the target (argv) and `zig version` |
| `hack` | verify when `check` or `clippy` is among its arguments; else exec |
| `insta` | `accept`, `reject` mutate; `review` runs directly in the pool |
| `sqlx` | `prepare` mutate; else exec |
| `sweep`, `clean-all` | mutate, holding heli's eviction lock for the run (below) |
| `dx`, `trunk` | `serve` runs directly in the pool; else exec |
| `leptos` | `watch`, `serve` run directly in the pool; else exec |
| `tauri` | `dev` runs directly in the pool; else exec |
| `watch` | runs directly in the pool |
| `outdated`, `semver-checks`, `vet`, `geiger` | exec, never cached: the result depends on the registry |
| `audit` | verify, keyed by the advisory database; `fix` mutate; `bin` exec |
| `deny` | `check` verify, keyed by the advisory database; else exec |

**Advisory checks.** A verdict of `cargo audit` or `cargo deny check` is a
verdict on the lockfile and on the advisory database, which gains advisories
for unchanged dependencies every day. A pass cached by the tree alone would
hide them, so the key also holds the commit of each database the run reads:
cargo-audit's `--db`, else `advisory-db` in the Cargo home, and every
`advisory-dbs/*` there for cargo-deny. It also holds the `audit.toml` and
`deny.toml` contents and the tool's version.

A run that would fetch the database as it starts has the daemon fetch it
first, before the key (`src/daemon/advisory.rs`). cargo-deny fetches with
`cargo deny fetch db`. cargo-audit has no fetch-only command, so the daemon
runs `cargo audit --file <a lockfile without packages>`. The key then names
the database the run checks against: unchanged dependencies and an unchanged
database reuse the pass, and a new advisory misses. The daemon fetches one at
a time per tool and Cargo home, so agents checking together share one fetch.
A fetch stays fresh for `[advisories] fetch_interval_minutes` (default 0),
and for 30 s at least, since a run computes its key more than once. Within
the interval a check reuses a pass without the network, at the price of
seeing an advisory up to that late. A fetch that fails leaves the run
unkeyed, so not cached. Runs that cannot fetch (`--no-fetch`,
`--disable-fetch`, `--offline`, a `deny check` of bans, licenses or sources
only) key the database as it is. A configuration that moves the database or
forces fetching (an `audit.toml` `[database]` table, `db-path`, `db-urls`)
makes the run unkeyed.

**Cleaners.** cargo-sweep and cargo-clean-all delete build output themselves. Run through heli, each is a mutate run that also holds heli's eviction lock (`evict.lock` in the state directory) from before it starts until its client ends. heli's own eviction and the tool never delete at the same time: the daemon's passes skip while it is held, `cargo heli gc` waits, and the tool waits for a pass in progress. heli says first what the tool cannot see. cargo-sweep sees only `target/`, since the intermediates are in the build store, which `cargo heli gc` evicts. cargo-clean-all deletes whole `target/` directories, other worktrees' included, whose locks heli cannot hold. A client killed during the run releases the lock early.

**Profiles from configuration.** A user writes profiles in heli's
configuration file, and any repository that uses heli commits its own in
`.config/heli.toml` at its worktree root. Both use the same table:

```toml
[[profiles.tool]]
tool = "xtask"              # the word after `cargo`
args = ["check-changed"]    # optional: the arguments it starts with
class = "verify"            # compile, verify, mutate or exec
```

Resolution is built-in, then the user's, then the repository's. Within one
source the rule with the longest `args` prefix wins. `test` cannot be given:
a test run needs a runner heli knows. A repository only narrows: its rule
applies only when it caches no more and locks no less than the user's or
built-in class, so it may make a tool mutate or exec, never verify. With
`trust_repository = true` under `[profiles]`, the user lets the repository's
rules apply as they are. An invalid repository file is a warning and is
ignored; an invalid user `[profiles]` table is a usage error. A tool that a
profile names is not unknown, so it never runs directly at a terminal.

**Direct in the pool.** The client runs the command itself, as the direct
fallback does, but with the host pool in `CARGO_MAKEFLAGS` (the daemon is
reached first, so the pool exists). No run is registered, cached or
attached to, and the worktree lock is not taken. Interactive and service
tools (`insta review`, `dx serve`, `trunk serve`, `leptos watch|serve`,
`tauri dev`, `cargo watch`) always run this way. So does
any other command no profile names, when stdin is a terminal; cargo's
own exec subcommands (`bench`, `tree`, `install`, ...) stay in the daemon.
Without a terminal an unknown tool is exec in the daemon, as before.

**Survivors.** A `sccache` server started inside a run would end with the
run's process tree, and `cargo heli cancel` would cold-start everyone's
cache. When a run's compiler wrapper is `sccache`, the daemon runs
`sccache --start-server` before the run, outside any run's tree. The wrapper
is read as cargo reads it: `RUSTC_WRAPPER` or `RUSTC_WORKSPACE_WRAPPER` (and
their `CARGO_BUILD_*` forms), else `build.rustc-wrapper` or
`build.rustc-workspace-wrapper` from the nearest Cargo configuration. A
variable set empty turns the wrapper off. The daemon waits at
most 5 s and ignores failure, including "already running".

**Compatibility matrix.** `.github/workflows/compatibility.yml` installs
cargo-hack, machete, shear, deny, audit, llvm-cov, mutants and insta on
Linux, macOS and Windows. It runs them through heli against the `tools`
fixture (`tests/tool_compatibility.rs`, ignored by the main suite). Each
must exit as it does without heli. The cached verifications (`hack check`,
`machete`, `shear`, `deny check`, `audit` with heli's fetch) must hit the
second time. `mutants --list` must print the same mutants, `llvm-cov` must
build in `target/llvm-cov-target`, and `insta review` must run directly. It
runs weekly, so a tool release that breaks under heli shows up, and on
changes to classes, profiles, the client, the daemon and the store. `hakari`
runs in the same job against a workspace-hack crate it creates (`generate`
must mutate as directly, `verify` must hit). The pool must bound a real tool: two `cargo heli hack check` runs at once,
with two slots, run no more than N + 1 build scripts per cargo at once, and
they must overlap. `insta review` must end as it does directly on a real
terminal (ConPTY, `posix_openpt`). The main suite checks that an unknown tool
at a terminal runs directly in the pool: it gets the pool's `CARGO_MAKEFLAGS`
and no `HELI_RUN` (#64). A nightly job runs `udeps`,
`public-api` and `miri test` through `cargo heli +nightly`, each a cached
verification keyed by the toolchain.

`cargo heli +<toolchain> ...` is honoured however heli was started. Cargo
hands an external subcommand its toolchain's own binary in `CARGO`
(`.../toolchains/<name>/bin/cargo`), which does not take `+toolchain`. heli
then runs rustup's proxy, `cargo` on the search path, instead. Any other
`CARGO` is kept.

`cargo heli doctor [--json]` lists the installed `cargo-*` tools with their
profile, and explains `jobs`, a global `target-dir`, a user's own `build-dir`
(never overwritten by `setup`), `RUSTC_WRAPPER`, mold or lld, `CARGO_HOME` and
rust-analyzer. It warns about a `CARGO_MAKEFLAGS` (else `MAKEFLAGS`,
`MFLAGS`, the one cargo reads) whose jobserver is neither the host pool nor a
run pool in it: the pool's name follows the state directory, so a value
written before the directory moved names a pool that no longer exists, and a
cargo with it builds unbounded after one warning. The action names the value
`setup` writes now; `setup` replaces its own earlier values (#86). It reads
configuration and changes nothing. The `--json` document has `schema` 1,
`tools` and `findings`.

### Lifecycle

1. **Key** (compile, verify and test runs). The key covers the worktree's tree,
   the normalized argv, the toolchain, the Cargo configuration and the
   environment; §7 lists exactly what goes in and how.
2. **Result store.** On a hit, heli prints "already passed: <run>, <when>"
   and exits 0 (§7).
3. **Single-flight.** If a run with the same key is in flight anywhere on the
   machine, the client attaches to it (step 4, "Attach"): it streams that
   run's output and returns its exit code. The key leaves the worktree's
   path out, so the run may be another worktree's; the notice names its
   worktree, and says that `--no-cache` runs it anew.
   - The key is the one the result store's first lookup computed, so a
     miss costs no snapshot of its own.
   - A run takes the clients of its key from other worktrees only once it
     has the key it runs on. A run that waits at its worktree lock is keyed
     again after the wait (§7), since the runs it waits for (a `fmt`) may
     change its tree; until then it takes clients from its own worktree
     only, whose tree such a change reaches too. A run that waits for
     nothing keeps its key.
   - A `--no-cache` run attaches to no run. Its key is prepared before
     admission to identify its source generation; it takes clients of the
     key only once admitted and rekeyed after any conflicting wait.
   - Compile keys additionally include the worktree and selected artifact
     directories. Identical builds in one worktree share; another worktree
     or output directory must build its own artifacts. No complete source
     inventory means no build sharing. Compile runs never reuse saved verdicts.
   - A live-only request can follow a cached request only after its process
     starts: a queued cached request may still resolve to a saved verdict.
4. **Detach.** The daemon spawns the command as the first process of a
   process tree of its own, and the client follows its output.
   - **The tree.** On Windows, a Job Object per run: the process starts
     suspended, joins the job, then runs, so nothing it starts escapes (the
     job allows no breakaway, and cargo's and nextest's own jobs nest in
     it). The job does not kill on close, so runs outlive a daemon that
     dies. On Unix, a session of its own (`setsid`): a process group would
     not do, since nextest puts each test in a group of its own. Each phase
     of a test run (step 6) is a tree of its own, and cancel kills the one
     that runs. The runtime guardian (§6.3) is the tree's first process;
     the actual command is its child in the same tree/session.
   - **Following.** The run writes its output only to its record (see
     "Output" below), and every client follows the record from its first
     byte, each in a thread of its own. A client that stops reading or is
     killed holds up nothing; a killed client is noticed within 200 ms, even
     while the run is silent. Output that cannot be recorded is kept in
     memory, so clients still get all of it. During retirement, once a run
     has ended for at least one second, a follower stalled on writes is
     disconnected: it cannot pin the retiring daemon. Otherwise a paused
     reader can resume and receive the full stream and exit code. The full
     recorded output remains available through `log`.
   - **Attach.** Re-running the same command attaches to the run in flight:
     the client prints one notice line (the run, how long it has been going,
     and `cargo heli cancel <run>` to run it anew), then the run's output
     from its first byte, and exits with its code. The same command is the
     same working directory, argv and environment, apart from what a shell
     keeps for itself (`PWD`, `OLDPWD`, `SHLVL`, `_`); on Windows variable
     names ignore case. The environment counts because it holds the target
     directory, flags and tool settings. A run still waiting for its turn
     at the worktree lock takes clients too, which hear what it waits for.
     Every client gets the run's whole story: heli's notes, and its end
     (its exit, a pass found in the result store meanwhile, or a command
     that could not start). A retiring daemon starts no run but still
     attaches.
     - This holds for runs using command identity: mutate and ordinary exec runs,
       and requests for help. The same command may have started before the
       agent's last edit; the notice's age tells.
     - A compile, verify or test run attaches by key instead (single-flight, step
       3): the same tree too, and from any worktree. The same command after
       an edit is a run of its own. Compile additionally keys artifact
       ownership, so its outputs belong to the requesting worktree. A
       force-new request or one without a complete key attaches to no run.
   - **Cancel.** `cargo heli cancel <run>` kills the whole tree and answers
     once none of its processes is left. On Windows, `TerminateJobObject`,
     waiting on the processes themselves too (a job stops counting a
     process a moment before it has ended). On Unix, SIGTERM to every
     process of the session, so cargo and nextest can stop their children,
     then SIGKILL to those left after 2 s. Its clients report the run as
     cancelled, and it takes no new clients. A run cancelled while it waits
     for its worktree never starts, and its clients exit 1. A process that
     starts a session of its own (on Unix) is not the run's any more.
5. **Compile phase.** The environment gets the run pool (§4) and
   `HELI_RUN=<run id>`. heli sets no `-j`: under a jobserver, cargo ignores it.
6. **Test phase** (test class only).
   - heli first builds under the run pool, with `nextest run --no-run <args>`
     or `cargo test --no-run <args>` (`--no-run` right after the
     subcommand); `status` shows the run `building`. A build that fails ends
     the run with its exit code. `cargo test --doc`, which cargo cannot build
     apart, goes straight to its test slots, and a command that has
     `--no-run` already is a build, run once.
   - nextest's build leaves out what only the tests read: under `--no-run`
     nextest ignores it with a warning (`ignoring --status-level because
     --no-run is specified`), or refuses it (`--no-fail-fast`, `--fail-fast`,
     `--max-fail`). That is the thread count (`-j`, `--test-threads`,
     `--jobs`), `--retries`, `--flaky-result`, `--no-tests`, `--max-fail`,
     the reporter's `--status-level`, `--final-status-level`,
     `--success-output` and `--failure-output`, the switches
     `--no-fail-fast`, `--fail-fast` and `--no-capture` (`--nocapture`, also
     after `--`), and the variables that set them
     (`NEXTEST_TEST_THREADS`, `NEXTEST_RETRIES`, `NEXTEST_FLAKY_RESULT`,
     `NEXTEST_NO_TESTS` and the reporter's four). Checked against nextest
     0.9.146; `--cargo-quiet` and `--cargo-message-format` stay, and work in
     both phases.
   - It then takes K slots for the test threads (`status`: `testing`). K is
     the smallest of the thread count asked for, the run's share (whole
     slots, at least one), and the slots it gets within 5 s; it stops waiting
     once it has them all. The threads asked for are nextest's `-j`,
     `--test-threads` or `--jobs`, else `NEXTEST_TEST_THREADS` (a number below
     1 counts down from the logical CPUs, as in nextest), or libtest's
     `--test-threads`, else `RUST_TEST_THREADS`. Free slots in its own run
     pool count first; then it waits like any waiting run (§4), and the
     broker takes slots back for it as for any. With no slot after 5 s, the
     tests run with one thread on the run's implicit slot and hold no pool
     slot (G1).
     An invalid thread count, including a missing flag operand, runs
     unchanged so the native runner reports its own failure and exit code;
     heli never replaces an invalid option with a valid count.
   - It runs the tests with `--test-threads K` (nextest, in place of the
     user's own) or, for libtest, `RUST_TEST_THREADS=K` (and `--test-threads=K`
     after `--` only in place of the user's own), and holds the K slots until
     they finish. What follows `--` reaches every test binary, and through a
     target's `runner` (`wasm-bindgen-test-runner`) or to a `harness = false`
     test a libtest flag may be refused, so heli adds none; for the same
     reason the test phase drops the `--quiet` compact output adds, which
     cargo passes on to each test binary, and the compact view leaves out
     cargo's `Finished`/`Running`/`Doc-tests` lines instead (#98).
     A header line on stderr says so before the tests start:
     `cargo-heli: r17: testing on 4 threads, 4 slots (share 8), waited 0.2 s`,
     or `… testing on 1 thread, no pool slot free (share 2), waited 5.0 s`.
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
   tool's own, and the footer starts a line of its own. So far only a test
   run has a header: its test phase's line (step 6). The footer holds the
   run id, the exit (`exit <code>`, or on Unix `signal <n>`) and the
   duration:
   `cargo-heli: r17 exit 0 in 41.2 s (output: cargo heli log r17)`, or for
   a cancelled run `r17 cancelled, exit 1 in …`. Peak memory and slots
   join it once heli measures them. After a quiet run that succeeded,
   quiet's one line takes its place ("Output" below); never after a
   cancelled one. A run heli executes directly, without the daemon, has no
   footer and no record, and its output is the tool's own, untouched.

**Run ids** are `r<number>`, unique for the life of the state directory:
records, markers and `cancel` name runs by them, and on Windows every logon
session has a daemon of its own on the same state directory. Each number is
read from and advanced in `next-run` in the state directory under a file
lock (`next-run.lock`), and is never below one past the highest record in
the run log, so a lost or stale counter costs no record. A number that
cannot be written back is not given: the client runs the command directly.

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
  conflict with it. Two runs conflict when either is exclusive, or when
  their complete verification source inventories at admission identify
  different source states. These barriers stay fixed through rekeying, so
  a run cannot gain a new wait after deciding to keep its input key.
  The latter prevents rebuilding a test executable still running
  on Windows; equal source states keep shared access. Separate worktrees
  do not wait on this guard. So a
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
   worktree, and they wait for it, in the order they came. Keyed verification
   runs of different source states in that worktree also wait in admission
   order, then rekey before starting; this prevents replacing live binaries.
3. **Cargo's own build-directory lock:** two runs building the same worktree
   at once are serialized by cargo. The waiting cargo holds no pool slot.
4. **Recycling:** the first run from a directory, while a store holds an
   orphan of its own repository (the one git names for the directory),
   waits until its worktree's build directory has been recycled or will
   not be (§6.4): for its `cargo metadata`, and with a donor for staging the
   worktree, a scan and a rename, and with a donor's source-state record
   for setting times in the worktree. At most 30 s; later runs from there
   never wait for it, and an orphan of another repository keeps no run
   waiting. While a store holds any orphan, the first run asks git for its
   repository before it starts.
5. **The source-state record:** as a run ends, its clients hear the end
   once the record of its build directory is written (§6.4), at most 10 s
   later. This waits for no other run.
6. **Cargo resolution and verification preparation:** Cargo serializes
   dependency resolution on its package-cache lock, including offline runs.
   The full metadata and source inventory of keyed verification modes add
   preparation before compilation; they are not bounded by the scheduler's
   first-job latency target. `--cache off` also inventories inputs, to share
   only current checks. The daemon acknowledges this preparation before the
   client's five-second first-answer deadline; preparation is not a command
   start and loss before start still permits direct fallback.
   This does not remove Cargo's own resolver
   lock or promise zero startup work. See `docs/multi-agent.md` and the
   pinned Cargo-source analysis in `docs/research/multi-project-resource-isolation.md`.

### Command line and contracts

```
cargo heli [--output full|compact|quiet] [-q] [--cache on|refresh|off | --no-cache] [--priority interactive|normal|background] <subcommand> [tool args…]
cargo heli [--changed[=REF] | --affected[=REF]] <subcommand that takes -p> [tool args…]
cargo heli changed [--affected] [--base REF] [--json]
cargo heli exec [heli flags] -- <command…>
cargo heli [--cache on|refresh|off | --no-cache] exec --verify [output flags] -- <verification command…>
cargo heli status [--json]
cargo heli log <run> [--errors | --test <name> | --tail <n> | --grep <re> | --raw]
cargo heli cancel <run>
cargo heli rerun [<run>] [--failed [--then-all]]
cargo heli gc [--dry-run] [--json]
cargo heli setup [--print] [--autostart | --no-autostart]
cargo heli ensure
cargo heli stop [--now [--yes]]
cargo heli restart [--now [--yes]]
cargo heli daemon [--detach | --warden]            (internal)
cargo heli recycle                                 (internal: the post-checkout hook, §6.4)
```

- **Changed packages (#90).** `--changed` runs the subcommand on the
  workspace members whose files changed; `--affected` adds every member that
  depends on them, transitively, through normal, dev and build dependencies
  (`cargo metadata --no-deps`).
  - The base is the merge base of `HEAD` with `REF`: by default
    `@{upstream}`, else `origin/HEAD`; without either heli refuses and says
    to give `=REF`. The changes are what `git diff <base>` names (commits
    since the base, staged and unstaged) and the untracked files git does
    not ignore.
  - A file belongs to the deepest member whose manifest directory holds it.
    `Cargo.lock`, the root `Cargo.toml`, and `.cargo/` or `rust-toolchain*`
    in the workspace root or above it in the repository select the whole
    workspace (`--workspace`). Other files (docs, `.github/`) select nothing.
  - The selection goes to cargo as `-p a -p b`, sorted so the run key
    (§7) is stable, before any `--`. One line on stderr says what it chose:
    `cargo-heli: --affected since origin/main (3f2a1c9): -p a -p b (+2
    dependents)`. When nothing is selected, heli prints `nothing changed
    since <base>` and exits 0 without running anything.
  - It works with build, check, clippy, test, bench, doc, `nextest run`,
    `nextest list` and llvm-cov; heli refuses (exit 2) any other subcommand,
    and `-p`, `--package`, `--workspace`, `--all`, `--exclude` or nextest's
    `-E` beside it.
  - `cargo heli changed` prints the selection, each package with why (its
    changed files, or the member it depends on), and the files in no
    package; `--json`, a versioned document (`schema` 1).
- **Exit codes.**
  - The tool's own code.
  - 127 when the command does not exist, 126 when it cannot be started.
  - 2 for heli's usage errors and refusals: `heli` given as its own
    subcommand, or nesting deeper than 16 (`HELI_DEPTH`). The daemon and its
    detaching starter count as levels too. An output mode that is not one
    (`--output`, `HELI_OUTPUT`), or `-q` with `--output`. A priority that
    is not one (`--priority`, `HELI_PRIORITY`, §4). For `log`, also a
    run id that is not `r<number>` (or a bare number), a bad regular
    expression, or two selectors at once. A `--changed` or `--affected`
    heli refuses or cannot resolve (no git repository, no base).
  - 1 when the daemon is lost once a run's command has started (§3), and
    when `log` has no record of the run or cannot read it. For `rerun`, the
    rerun's own code; 1 when there is nothing to rerun ("Reruns" below).
  - For `cancel`: 0 once the run's processes are gone; 1 when the run is
    not in flight (it ended, or never was), when no daemon runs (`cancel`
    starts none), or when the daemon cannot cancel it (too old: it is asked
    to retire); 2 for a run id that is not one. The client of a cancelled
    run exits as its process did: on Windows 1, on Unix of SIGTERM (or
    SIGKILL).
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
  - memory watermarks (`[memory] low_watermark_mb`, `high_watermark_mb`,
    §4);
  - the event log's size (`[event_log] max_kb`, §8);
  - disk budget and free-space floor (`[store] budget_mb` and `floor_mb`,
    in megabytes of 2^20 bytes; no default, §6.3);
  - the store per volume (`[store] paths`: absolute paths; on each volume
    the first one there serves it, §6.1);
  - tool-cache patterns (`[store] tool_caches`: paths below `target/`, `/`
    between components, `*` and `?` within one; §6.2, §6.3);
  - eviction ages, in hours (`[store] unit_hours`, default 24;
    `orphan_hours`, 24; `idle_hours`, 6; `dir_hours`, 168; §6.3);
  - the default output mode (`[output] mode`, "Output" below; the client
    reads it too, and ignores a file it cannot use);
  - log retention (`[log] runs`, the records kept, default 200, and
    `[log] bytes`, their total size in bytes, default 2 GiB; "Output"
    below).

  Debug builds also read a `[test]` table, the one seam tests have into the
  daemon: memory figures (`[test.memory]`) and a clock (`clock_ms`,
  milliseconds since 1970), read again at each look of the memory governor,
  which counts only the time between its readings, and of the eviction
  ladder, for which it is the time now (§6.3; `gc` reads it too). Release
  builds ignore it.

### Output (G7)

Prior art: [rtk](https://github.com/rtk-ai/rtk) (Apache-2.0). A PreToolUse
hook rewrites an agent's shell commands into `rtk …`, which filters the
output (for `cargo test`: failures only) and keeps the full output for
`rtk recall`. Its cargo rewrite rule matches only
`cargo (build|test|clippy|check|fmt|install)` (`src/discover/rules.rs`), so
rtk passes `cargo heli …` through untouched. heli must keep its own output
small, and output is never filtered twice.

**Modes**, chosen by `--output=full|compact|quiet` or `-q` before the
subcommand (or after `exec`), else by `HELI_OUTPUT`, else by `[output] mode`
in the configuration, else by whether the client's stdout is a terminal.
Flags before the subcommand belong to heli; flags after it belong to the
tool.

- **full**: the tool's output as it streams, plus heli's header and footer.
  This is the default on a terminal.
- **compact**: the default when stdout is not a terminal, which covers every
  agent tool call.
- **quiet**: one line on success, and the compact output on failure. The
  line goes to stderr in place of the footer: the verdict, the command as
  given, what the view counted, the duration and the run, for example
  `ok  clippy --all-targets: 3 warnings, 41.2 s  [r17]` or
  `ok  nextest run -p my-crate: 812 passed, 3 skipped, 41.0 s  [r17]`. The
  client holds the rendered output back until the run ends, and on failure
  prints it, then the footer.

**Views.** Before a run starts, the client picks the view that shows its
output, from the command line and the mode, and adds to the command what
the view needs. The daemon runs and records the command so changed,
`status` and the worktree lock's waiting line show it so, and the result
store keys it so (§7): a compact and a full run of one command have keys of
their own. Full mode passes every run through. In compact and quiet mode:

- build, check, clippy and doc (and cargo's aliases `b`, `c`, `d`; after a
  `+toolchain` too) show cargo's diagnostics (below);
- `nextest run` (`nextest r`) shows nextest's output under its own controls,
  and `test` (`t`) libtest's failures, both phases of the run, with cargo's
  diagnostics as for a compile run ("Test runs" below);
- `cargo heli exec` shows the head and the tail of the command's output,
  and adds nothing to the command, which runs as given, even
  `exec -- cargo …` ("exec" below);
- anything else passes through untouched, with the footer, in every mode:
  `run`, whose output is the program's; `clippy --fix`, `fix` and `fmt`,
  which say what they rewrote in lines `--quiet` drops; and cargo
  subcommands heli knows nothing of (`cargo heli tree`).
- So does every cargo run (nextest's aside) whose arguments already choose
  cargo's output: `--message-format`, `--quiet` or `-q`, `--verbose` or
  `-v`, also in a cluster of short flags such as `-vv` or `-rq`, before any
  `--`. heli must not add its own there: cargo refuses a second `--quiet` or
  `--message-format`. A cluster counts even when the letter is in a value
  (`-pserver`), which costs compactness, never the run. nextest's flags
  are its own (`-v` is nextest's verbosity, cargo's is `--cargo-verbose`):
  heli adds each of its controls the user did not set (below).

The run's class (§5, "Classes") decides how warnings show: counted for
compile runs, by lint for verify runs. Within those classes, `run` and
`fmt --check` pass through: their output is not cargo's diagnostics.

**How compact output is produced**, most robust source first:

1. **The tools' own verbosity controls.** Nothing is parsed, so nothing can
   be misreported.
   - cargo: `--quiet` drops the `Compiling …` line per unit (442 of them in a
     cold build of the example workspace).
   - cargo under `cargo test`: `--quiet` also has libtest print one
     character per test in place of a line.
   - nextest: `--cargo-quiet`, `--status-level fail`,
     `--final-status-level fail`, `--success-output never` and
     `--failure-output final`. All five exist in nextest 0.9.146 (`--help`).
     A failing test's output (its stdout, and its stderr with the panic
     message) then comes once, under its name, after nextest's summary.

   heli adds a flag only when the user did not set it: for cargo, in the
   argv; for nextest, in the argv or in the variable nextest reads for it
   (`NEXTEST_STATUS_LEVEL`, `NEXTEST_FINAL_STATUS_LEVEL`,
   `NEXTEST_SUCCESS_OUTPUT`, `NEXTEST_FAILURE_OUTPUT`). `--cargo-quiet` is
   not added beside the user's `--cargo-quiet` (twice silences cargo's
   errors too) or `--cargo-verbose`. Where nextest would ignore a flag with
   a warning, heli does not add it: none of the reporter's four to a
   command that only builds (`--no-run`), and neither output level to one
   that does not capture (`--no-capture`, `--nocapture`, also after `--`).
   A nextest profile's settings for the same four give way to heli's flags
   in compact and quiet mode; full mode keeps them. heli adds its flags
   after the user's arguments, so that cargo reports a mistake in them
   exactly as without heli, and before any `--`, after which the arguments
   are another program's.
2. **Structured messages.** For compile phases heli passes
   `--message-format json` to cargo and `--cargo-message-format json` to
   nextest, and renders diagnostics from the JSON fields (level, code, primary
   span, message, rendered), on stderr, where cargo prints them:
   - Errors, as they arrive: the first 3 in rustc's full rendering, the next
     30 as one short line each (`path:line:col error[E0308] message`), then,
     when there are more, `N more: cargo heli log <run> --errors`.
   - Warnings, once the run is over. For `clippy` and `check`, where the
     findings are the point: grouped by lint, a line per lint with its count
     and where its warnings point,
     `  dead_code (3): src/a.rs:4:5, src/b.rs:9:1, 1 more`, under a
     `12 warnings:` line. 50 locations at most, shared out one per lint in
     turn, so that every lint shows some.
   - Warnings, for `build`, `doc` and `test`: counts per lint, for example
     `12 warnings: 9 unused_imports, 3 dead_code`.
   - Lints with the most warnings come first, then by name; a warning of no
     lint is known by its message. A diagnostic that several targets report
     (a library and its unit tests) is shown once, as cargo's human output
     does. Notes ("for more information about this error") are dropped, and
     so are cargo's other messages (artifacts, build scripts, the build's
     end). A line on stdout that is not cargo's JSON, and cargo's own stderr,
     pass through.
3. **Text**, only where no structure exists: libtest's own output under
   `cargo test` (its `failures:` blocks), and `cargo heli exec` of arbitrary
   commands, where compact means the first 20 and last 60 lines around a cut
   marker.

**Test runs.** Both phases of a test run (§5, lifecycle step 6) write to one
view, so a diagnostic that the build and the tests' own cargo both report
shows once, and heli's header line passes through between them.

- **nextest.** nextest's own output, on stderr, passes through under heli's
  controls: its header, a line per failing test, the summary, then each
  failing test's output. So does a line on stdout that is not cargo's JSON
  (a test's output under `--no-capture`). Quiet mode's facts are the counts
  of nextest's summary line as nextest words them (`812 passed, 3
  skipped`), then the warnings.
- **libtest.** Of libtest's text on stdout, heli keeps each `failures:` (or,
  under `--show-output`, `successes:`) section up to the binary's
  `test result:` line, with each test's captured output and panic message,
  and the `test result:` line of a binary whose tests failed. It drops what
  libtest prints per test (`running 3 tests`, `test x ... ok`, progress
  dots, `x --- FAILED`), the blank lines between, and the `test result:`
  line of a binary whose tests passed. Any other line passes through. Once
  the run is over, a line on stderr sums the `test result:` lines of every
  binary: `tests: 812 passed, 1 failed, 3 ignored` (passed always, the
  other counts when there are some). Quiet mode's facts are the same
  counts, then the warnings.

**exec.** heli counts lines over both streams together, in the order they
came, as `log --tail` does, and prints each on its own stream: the first 20
as they come; once the run is over, the last 60, after a line on stderr
when lines were cut between them: `cargo-heli: 1234 lines cut (cargo heli
log r17 has them all)`. An output of 80 lines or fewer shows whole. Quiet
mode's fact is the count of lines: `1314 lines`.

Some things never change: the exit code, the stdout/stderr split, and output
heli does not recognize, which passes through. Beside lines heli renders on
stderr, the tool's own stderr passes through a whole line at a time, so that
a line the tool writes in pieces is not cut by one of heli's.

**Colours.** A run writes to pipes, so cargo would print no colours even for
a client on a terminal. When a run's output passes through to a client whose
stdout and stderr are both terminals, heli sets `CARGO_TERM_COLOR=always` in
the run's environment, unless the client's environment sets
`CARGO_TERM_COLOR` or `NO_COLOR`. A variable rather than `--color`: it
reaches nextest and every cargo the run starts, including under `exec`, and
needs no knowledge of where a flag may go (`cargo run -- …`). It is neutral
for the result store (§7). Rendered compact output has no colours.

**Nothing is lost.** The daemon records each run's complete output, for
`cargo heli log` and so that clients can attach to it (lifecycle step 4).
The output modes only change what the client prints, and the footer names
the run. `cargo heli log <run>` prints the record by run id, without
re-running: stdout on stdout and stderr on stderr, as a person would have
seen the run. Every line is as recorded, except cargo's JSON messages, which
heli asks for in compact and quiet mode. Each diagnostic becomes its
rendering, on stderr where cargo prints it, once however many targets
reported it. Artifact, build-script and build-finished records are left
out (#69). At most one selector picks a part of it:

- `--raw`: the record byte for byte, cargo's JSON included, for tools and
  for runs that asked for `--message-format json` themselves;
- `--errors`: every error, as rustc rendered it, on stderr. From cargo's
  JSON in the record of a compact or quiet run, once however many targets
  reported it; from text otherwise, each from a line that starts with
  `error:` or `error[` to the blank line after it, the next warning, or the
  end;
- `--test <name>`: one test's output, as its runner wrote it. `<name>` is
  the test's path (`tests::it_works`), or nextest's binary and path
  (`my-crate tests::it_works`). Under libtest, the test's section of a
  `failures:` or `successes:` list on stdout, from `---- <name> stdout ----`
  to the next section or the list of names that ends them. Under nextest,
  on stderr, the test's status line and the output under it, to the next
  status line or line of nextest's own (a separator, `warning: …`: lines
  indented less than the output's 4 spaces). Blank lines that end it are
  left out. nextest prints a status line twice when the output comes at
  the end (`--failure-output final`): outputs with more than their status
  line come first, else the status lines alone. With none, one line says
  so:
  ``cargo-heli: no output of test `<name>` in r17`` (a compact run holds no
  passing test's output);
- `--tail N`: the last N lines, as `log` prints them;
- `--grep <re>`: matching lines, as `log` prints them, each matched without
  its line end.

- **The record** is `runs/<run>.log` in the state directory. The daemon
  writes it as the output arrives, whether or not a client still listens,
  so `log` shows a run in flight up to now; a run `status` lists before its
  command starts (waiting for its turn) shows nothing yet, not "no such
  run". It is a sequence of numbered
  events, each the bytes one read of the run's stdout or stderr returned, in
  the order the daemon received them: exact within each stream, and across
  the two as close as two pipes allow. Before them comes a start event: the
  run's command line as the user typed it (heli's flags, the subcommand and
  its arguments, before the client added anything for its output mode),
  its working directory, its class, and the run it reruns, if it is a rerun
  ("Reruns" below). Never its environment, which may hold secrets. Readers
  skip events they do not know, so a record without one, written by an
  older heli, reads as before. Unless `--raw` asks for the record byte for
  byte, `log` of a rerun begins with one line on stderr,
  `cargo-heli: r18: rerun of r17`. Selectors work on lines: one stream's
  bytes up to a `\n` (or its unended last bytes), ordered by the event that
  holds their last byte, so `--tail` counts both streams together.
- **Reading.** `log` asks the daemon; when none answers, the client reads
  the record itself, and starts no daemon. Records outlive the daemon.
- **Retention.** The record holds what the tools printed in the chosen mode,
  so the dropped per-unit and per-passing-test lines are not in it (of a
  compact build, check, clippy or doc, it holds cargo's JSON messages, which
  `log` prints as they are). heli keeps the last 200 run logs or 2 GB,
  whichever limit is hit first: when a run starts, the oldest records
  (lowest run number) are deleted first. The record of a run in flight is
  never deleted.

**Reruns** (#91). `cargo heli rerun <run>` runs a recorded run again: the
command line of its start event, from its working directory, with the
environment `rerun` is given. It is a run of its own, linked to the one it
reruns: `status` (`rerun_of`, §8) and `log` say `rerun of r17`. Without
`<run>`, it takes the latest test run in this worktree whose record shows
failed tests.

- `--failed` reruns only the tests that failed, read from the record as the
  runner printed them, in any output mode (colours dropped):
  - **nextest**: its status lines on stderr, `FAIL [ 0.012s] (1/3)
    <binary-id> <test>`, also `TIMEOUT`, `ABORT`, a signal (`SIGSEGV`) or
    a retry's last word (`TRY 2 FAIL`), unless a later `PASS` or `FLAKY`
    line takes the test back. The rerun adds one filterset that names each
    test exactly: `-E '(binary_id(=b) & test(=t)) | …'`.
  - **libtest**: each test binary's list of names after `failures:` on
    stdout, which libtest prints before the binary's summary, and the line
    cargo prints on stderr once that binary has failed, `error: test
    failed, to rerun pass `-p foo --lib``, which names its target. Each
    stream is in order, but across the two only as the daemon read them,
    and cargo's line comes right as the binary exits, so it may be recorded
    before the binary's last output. So a binary's output (from libtest's
    `running N tests`) belongs to the binary at the same place among
    cargo's `Running …` and `Doc-tests …` lines when both streams count the
    same binaries, else to the last one cargo started before the output
    began; without those lines (`--quiet`), the outputs that list failures
    pair with cargo's failure lines in order. The rerun replaces the run's package and target selection (`-p`,
    `--workspace`, `--exclude`, `--lib`, `--bin`, `--test`, `--doc`, …) with
    the failed targets, and adds `--exact` and the names after `--`;
    binaries without failures do not run.
  - When that cannot be exact, heli says why in a `note:` line and runs
    more: a libtest run that already selects tests by name, or a target
    whose failures libtest did not list (`harness = false`), reruns the
    failed targets whole; failed doc tests together with other targets,
    which cargo cannot run in one command, keep the run's own selection,
    with the names; a nextest run that already filters (`-E`, a test
    filter, arguments after `--`), which a filterset of heli's would only
    add to, or failures heli cannot place in a target, rerun the run whole.
  - A narrowed rerun drops the run's `--changed` and `--affected`: it
    selects its own packages, and those would choose again from what
    changed since (and run nothing on an unchanged tree).
  - The first line says what reruns: `cargo-heli: rerunning 2 failed tests
    of r17: a::t1, b::t2`.
- `--then-all`, with `--failed`: once the failed tests pass, the whole run
  again, as another rerun.
- A run with no record (pruned, or never), with no start event (it ran
  before heli recorded them), or, with `--failed`, one that is not a test
  run or failed no tests: one line saying so, and exit 1.

The rerun is this program again with a hidden first flag, `--rerun-of
<run>`, which the client sends in its request and which is not part of the
recorded command line.

**With rtk installed.** A bare `cargo test` that rtk rewrites to
`rtk cargo test` still draws from the heli pool: rtk passes the environment
through to cargo unchanged. Checked in rtk's source at commit
[`8157d4d`](https://github.com/rtk-ai/rtk/tree/8157d4dcdc3c011f6251de2c972ff6ef8006df7b)
(2026-10-13); rtk was not installed to try it.

- `rtk cargo build|test|clippy|check` builds the command in
  `run_cargo_streamed` ([`src/cmds/rust/cargo_cmd.rs` 334](https://github.com/rtk-ai/rtk/blob/8157d4dcdc3c011f6251de2c972ff6ef8006df7b/src/cmds/rust/cargo_cmd.rs#L334)):
  `resolved_command("cargo")`, then arguments only.
- `resolved_command` ([`src/core/utils.rs` 609-633](https://github.com/rtk-ai/rtk/blob/8157d4dcdc3c011f6251de2c972ff6ef8006df7b/src/core/utils.rs#L609-L633))
  is `Command::new` on the resolved path, and touches the environment only
  for `git`, in rtk's own test builds.
- `run_streaming` ([`src/core/stream.rs` 425-471](https://github.com/rtk-ai/rtk/blob/8157d4dcdc3c011f6251de2c972ff6ef8006df7b/src/core/stream.rs#L425-L471))
  sets stdin, stdout and stderr, then spawns. Nothing on the path calls
  `env_clear`, `env_remove` or `env`, so `CARGO_MAKEFLAGS` reaches cargo.
  cargo sees pipes, not a terminal.
- The rewrite keeps variable assignments before the command: rtk's hook
  rewrites `CARGO_MAKEFLAGS='…' cargo test` to
  `CARGO_MAKEFLAGS='…' rtk cargo test` ([`src/discover/registry.rs` 66-77, 1767-1786](https://github.com/rtk-ai/rtk/blob/8157d4dcdc3c011f6251de2c972ff6ef8006df7b/src/discover/registry.rs#L1767-L1786)).
- The rule ([`src/discover/rules.rs` 94](https://github.com/rtk-ai/rtk/blob/8157d4dcdc3c011f6251de2c972ff6ef8006df7b/src/discover/rules.rs#L94))
  matches only `cargo (build|test|clippy|check|fmt|install)`, so
  `cargo heli …` and `cargo nextest …` pass through untouched.

Anyone who prefers rtk's filters to heli's can run
`cargo heli --output=full …` under `rtk test` or `rtk err`, rtk's generic
wrappers.

## 6. Build store

### 6.1 Placement

Build placement is **opt-in per repository** (Q2, decided in #27). Enabling
heli's host pool does not imply moving every project's intermediates.
`cargo heli setup` offers `build.build-dir = "<store>/{workspace-path-hash}"`
for one repository. The separate user-wide offer remains an explicit override
for a user who has validated their other projects and tools; it is not the
recommended scope. Both file writes default to no. Verified: `build-dir`
supports the templates `{workspace-root}`,
`{cargo-cache-home}` and `{workspace-path-hash}`, the last being a hash of the
canonical manifest path (cargo `src/context/mod.rs`, `custom_build_dir`; Cargo
Book, `build.build-dir`).

The intermediates (`deps/`, `incremental/`, `.fingerprint/`, `build/`) leave
the worktree. `target/` keeps the final artifacts and tool caches: doc,
trybuild, and whatever a project's own tests build there.

`<store>` must be on the same volume as the worktrees. Cargo hardlinks final
artifacts out of the build directory, and across volumes it copies them
instead. The default is `<volume>/.heli/build`, one store per volume that
holds worktrees. On Unix the root of a volume is rarely writable, so there
the default is the user's cache, `$XDG_CACHE_HOME/heli/build` (else
`~/.cache/heli/build`), when it is on the worktree's volume, and
`<volume>/.heli/build` otherwise. A volume is known by its root:
`GetVolumePathNameW` on Windows, elsewhere the highest directory above the
path on the same device.

**Where setup writes it.** Each file is offered with its own question, as
for the pool's variable (§4, "Reach"):

- **This repository**: `.cargo/config.toml` in the repository's main
  worktree (the one holding the git common directory). Cargo reads the
  `.cargo/` of every directory above the one it runs in, so this covers the
  main worktree and every worktree inside it (Claude Code puts them in
  `.claude/worktrees/`), and nothing else: not worktrees elsewhere, and not
  the nested cargos of tests that build projects in temporary directories.
  Its store is the one on that worktree's volume. If the file is tracked,
  the line is a local change, this machine's, not to be committed.
- **Every project**: the user's Cargo configuration (`config.toml` in
  `CARGO_HOME`, else in `~/.cargo`). One value serves every volume, so its
  store is the one on the volume setup runs in. Every cargo reads it,
  nested ones included. Accepting the repository offer does not authorize
  this write: it has its own question, default no.

setup adds two lines under the file's `[build]` table (or in a new one at
its end): a comment that marks them as heli's, and
`build-dir = "<store>/{workspace-path-hash}"`. It replaces lines an earlier
setup wrote, and leaves alone, with the reason, a file that is not TOML,
that sets `build-dir` itself, or whose new content would not parse to the
same configuration plus `build.build-dir`. Then, in a repository, setup
offers its git `post-checkout` hook, for recycling (§6.4).

For a run and its nested cargos, the process-scoped equivalent is
`CARGO_BUILD_BUILD_DIR="<store>/{workspace-path-hash}"`. The two `BUILD`s
are intentional: Cargo maps the `build.build-dir` configuration key to
that environment variable ([Cargo Book](https://doc.rust-lang.org/cargo/reference/config.html#buildbuild-dir)).
Unlike a repository configuration file, the inherited environment also
reaches generated workspaces in temporary directories. It does not change
the user's Cargo configuration.

**Risk: tools that look in `target/<profile>/deps`.** Once the intermediates
move, anything that searches `target/<profile>/deps` for rlibs, test binaries
or `.d` files stops finding them. In flui, the candidates are the hot-reload
fixtures, trybuild and the nested-cargo checks. Before phase 2, a real
workspace's full test suite (flui's) must pass with `build-dir` set (§12).

This is the direction of cargo#16147. heli turns it on and manages what
accumulates.

#### FLUI validation, 2026-10-15 (#27)

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

### 6.2 Accounting

Each build directory holds `.heli/owner`: the workspace path, the repository
id (its git common dir), the creation time and the last run. The daemon keeps
sizes by category (incremental, deps, build scripts, fingerprints, other), for
build directories and for registered `target/` directories. It rescans a
directory after cargo releases its lock there.

**Learning a workspace.** heli does not compute `{workspace-path-hash}`
(cargo's hash of the canonical manifest path, which changes with cargo's
hasher); cargo names the directories. A run whose program is named `cargo`
(a cargo subcommand, or an `exec` of cargo) starts, beside it in the
background, `cargo metadata --format-version 1 --no-deps` as the run would:
its program, directory and environment. No other program is started with
those arguments, not even one standing in for cargo under another name. Its
`workspace_root`,
`target_directory` and `build_directory` (cargo 1.91 and later) say where
the run builds. It takes no lock (verified: it answers while a build holds
the build directory's), and it fails no run. It delays none either, but the
first run from a directory while a store holds an orphan of its
repository, which waits for it in case its worktree gets a recycled build
directory (§6.4).

- The `target/` directory is **registered**, with its workspace, in
  `store.json` in the state directory, which outlives the daemon. When the
  build directory is elsewhere, the entry also has `build_dir` and
  `separated_since`: when heli first saw the two apart, in seconds since
  1970, kept while they stay apart (a move from one store to another
  included) and dropped once the workspace builds in its `target/` again.
  Both are optional, so an older `store.json` still loads (#92).
- A build directory at `<store>/<2 hex digits>/<hex digits>` in a store
  heli manages (configured, or its volume's default) gets its manifest,
  JSON: `{"workspace":…,"repository":…,"created_at":…,"last_run":…}`, times
  in seconds since 1970, `repository` `null` outside a repository. The
  daemon rewrites it whole as each run starts, keeping `created_at`.

The stores heli manages are the configured ones and the default store of
each registered workspace's volume, where it exists.

**Sizes.** A file's category follows from its path in a profile directory
(`<profile>/`, or `<triple>/<profile>/`, a triple always holding a `-`):
`incremental/`, `deps/`, `build/` (build scripts) and `.fingerprint/`. In a
registered `target/` directory, what a `[store] tool_caches` pattern names
counts as tool caches; nothing is a tool cache by default. The rest is
other. Sizes are file lengths; links are never followed, and a final
artifact cargo hardlinked into `target/` counts in both directories. The
daemon's accountant scans a directory it has no figures for, those a heli
run built in once the run has ended (cargo has let go of its locks), and any
whose figures are an hour old, which catches what bare cargo built. It
never takes cargo's locks; `status` shows its figures, and
`gc --dry-run` scans afresh.

**Superseded intermediates** (#92). A worktree built before `setup` set
`build-dir` keeps its old intermediates in `target/` after the switch:
cargo no longer reads them, and on flui they were most of the disk. In a
registered, non-shared `target/` directory with `separated_since`, a
profile directory's `deps/`, `incremental/`, `build/` and `.fingerprint/`
are **superseded** when nothing in them was written after that time
(`src/store/superseded.rs`). One cargo still writes to (another
configuration, an environment variable) is not. Final artifacts at a
profile's top level, `examples/`, tool caches and a build directory inside
`target/` never are. `status` and `gc` report each target's superseded
bytes as `superseded`, which `by_category` counts too, and the text form
ends with a hint: `58.3 GiB superseded by the build store: cargo heli gc
--superseded`.

A build directory is in one of three states:

- **Busy.** One of cargo's locks in a profile directory cannot be locked:
  `<build-dir>/<profile>/.cargo-build-lock`, which cargo holds for a build
  (`src/compiler/layout.rs`), or in the artifact directory (`target/`)
  `.cargo-lock` and `.cargo-artifact-lock`. Where the build directory is the
  target directory, all three are in one place (verified with cargo 1.99).
  heli only tries the locks, and lets go at once. A lock it cannot even
  open, it counts as held.
- **Live.** Its workspace path exists: its `Cargo.toml`, since a removed
  worktree can leave files behind.
- **Orphan.** Its workspace is gone, because the worktree was removed.

A build directory without a manifest, which only bare or nested cargo has
built in, is attributed when it can be (#88). Cargo names it after the hash of
its workspace's canonical manifest path (`{workspace-path-hash}`: cargo's
`short_hash`, `StableSipHasher128` over the path, little-endian hex, split
2/14). The hash cannot be inverted, but a candidate can be checked: the
compiler records its working directory, the workspace root, in the object
files of the crates it compiles incrementally. heli reads the absolute paths
in the four smallest `.o` files of the directory's incremental sessions, and
takes the ancestor whose `Cargo.toml` (canonical when it exists; else as
spelled, under its nearest existing ancestor's canonical path, and on Windows
verbatim) hashes to the directory's name: never a guess. It then writes the
owner manifest, with no repository and no last run, and the directory is live
or an orphan like any other. One it cannot attribute (only `check` or
`clippy` built there: no object file) is marked `.heli/unattributed`, tried
again only after cargo has written in it, and is **unknown**: heli cannot
tell whose it is. A user-wide `build-dir` gives every temporary workspace of
cargo run inside tests (trybuild, generated projects) a directory of its
own; `setup` says so, and a repository's `build-dir` leaves them out.
Registered `target/` directories
have the same states, from the same locks and their registered workspace.

`cargo heli gc --dry-run` lists every directory, build and target, with its
state, size, workspace and repository, and the victims an eviction pass
would take (§6.3), and deletes nothing; `gc` without `--dry-run` evicts
them. It reads the store itself and needs no daemon. `--json` prints a
versioned document (`schema`, now 1): `dry_run`; `dirs`, one entry per
directory, with `kind` (`build` or `target`), `path`, `store`, `state`,
`workspace`, `repository`, `created_at`, `last_run`, `size` and
`by_category`, and `superseded` (bytes, above); `used` (what every directory takes), `budget` and `floor`
(bytes, `null` without one); `volumes`, each with `root`, `free` (`null`
when unknown) and `pending` (bytes still in its trash); `victims`, planned
in a dry run, else evicted, in order, each with `tier`, `path`, `bytes`,
`age` (seconds, as its tier counts it) and `reason`; and `skipped`, the
victims that could not be evicted, each also with `why`. The document's
`superseded` says whether the pass was `gc --superseded`, which reclaims
every superseded directory now (tier 6, §6.3), whatever the budget and the
floor, and nothing else; `--dry-run` combines with it.

### 6.3 Budget and eviction

The budget is B for all build directories plus registered `target/`
directories (`[store] budget_mb`), with a free-space floor F on each volume
that holds a store or a registered `target/` directory (`[store]
floor_mb`). Neither has a default until open question 3 is settled:
without them, nothing is evicted. Over budget, heli evicts down to 85 % of
B. Under the floor, it evicts on that volume until F is free there,
counting what its trash still holds as free. It evicts what is cheapest to
regenerate first, oldest first within each tier (the ages are `[store]
scratch_hours`, `unit_hours`, `orphan_hours`, `idle_hours` and `dir_hours`).
A tier's number names it and does not rank it: tier 6, the newest, comes
first, so that the numbers `gc`, `status` and the event log already showed
keep their meaning.

6. Superseded intermediates in registered `target/` directories (§6.2),
   whatever their age: cargo will not read them again. Reason:
   `intermediates superseded by build-dir <dir>`; age: since the
   directory was seen apart from its build directory.
0. Scratch copies cargo tools left in the temporary directory, untouched for
   1 h. cargo-mutants copies the workspace there for its builds
   (`mutants-*` on Windows, `cargo-mutants-<dir>-*` elsewhere) and deletes
   each copy when done. A cancelled or killed run leaves them behind, each
   a whole source tree with a `target/` of its own, used by nothing. A
   directory there with such a name and a `Cargo.toml` counts
   (`src/store/scratch.rs`). A cargo lock held anywhere inside makes it
   busy. Its trash is `.heli-trash` beside it, on its volume. It counts
   toward B, and `status` and `gc` list it as `scratch`.
1. `incremental/<unit>` directories untouched for 24 h, oldest first across
   the store. Each unit ever built leaves one; in the example they make up
   114 GB of one worktree.
2. Orphans that no new worktree claimed within 24 h (§6.4), longest
   unclaimed first.
3. All of a profile's `incremental/` in directories idle for 6 h.
4. Tool caches in the `target/` of worktrees idle for 6 h: cargo-llvm-cov's
   `llvm-cov-target`, built in, and patterns in the configuration, for
   example `tests/trybuild`, and for flui `cli-template-check` and
   `facade-consumer-check`.
5. Whole build directories of live workspaces idle for 7 days.

A registered `target/` directory seen for two workspaces (a global `CARGO_TARGET_DIR` or `build.target-dir`) is `shared` from then on. It is not heli's to manage: it has no rungs, not even 1 and 3, and does not count toward B. `gc` and `status` list it as shared (#65).

What depends on a directory's kind (build, target, scratch or shared) lives in one place, `src/store/kind.rs`: where a cargo lock makes it busy, its trash, its tool caches and its own rungs (6, 0, 2, 4 and 5); rungs 1 and 3 apply to every kind.

Later, between 4 and 5: units in `deps/` and `.fingerprint/` unused for 7
days, cargo-sweep's approach (out of scope for now).

A unit is untouched since its newest file was written. A directory is idle
since its newest file was written or its last heli run started (§6.2),
whichever is later. A profile directory is `<dir>/<profile>/`, or
`<dir>/<triple>/<profile>/` where a triple holds a `-`. A victim inside one
taken before counts only what is left in it, and nothing inside a victim
taken before is one. The clock is the test seam's in debug builds that set
one (§5).

Which directories have which tiers:

- A busy directory has none.
- A live build directory has tiers 1, 3 and 5; an orphan 1, 2 and 3. An
  orphan is deleted whole by tier 2 only, so it has its 24 h to be claimed
  by a worktree of its repository (§6.4). One without a repository (a
  temporary workspace's, attributed as in §6.2) can be claimed by none and
  does not wait (#88).
- An **unknown** build directory, which only bare or nested cargo built in,
  normally has tiers 1 and 3 only. Its first heli run gives it a manifest.
  In an exclusively regenerable cache root created by heli, it also has
  tier 5 after `dir_hours`, with a recognized Cargo lock layout and no held
  locks. It remains unknown: ownership of a cache root does not establish
  workspace identity, orphan status or eligibility for recycling.
- Only an explicitly configured `store.paths` root whose final directory
  heli successfully creates receives `.heli-cache-root.json` (schema 1,
  bound to its canonical path). Existing roots, even empty ones, and default
  roots are never adopted. Missing, copied, malformed or unsupported markers
  retain the conservative policy. `gc --dry-run` creates nothing.
  Root provenance, real child directories and the Cargo lock layout are
  checked again before eviction. `gc --json` exposes `managed_cache_root`.
  Configure a fresh root for nested-Cargo reclamation; there is no implicit
  migration of an existing store.
- A registered `target/` directory has tiers 1 and 3 where cargo keeps its
  intermediates there (no `build-dir`), tier 6 where it kept them before
  `build-dir` moved them, and tier 4. It is in the user's worktree, so it
  is never deleted whole. A tier 6 victim is renamed into its trash under
  the profile locks like the others, so a held `.cargo-lock` or
  `.cargo-artifact-lock` skips it, and only if it is still untouched since
  the separation.

Safety rules:

- **Protect runtime, not just compilation.** Cargo 1.99 releases its layout
  locks before executing tests. A public nested-test reproducer, gated after
  compilation, lost its `OUT_DIR` asset to GC with `dir_hours = 0`. Every
  managed command now holds a shared runtime lease of its worktree
  (`<activity>/worktrees/<hash>/`, the worktree's path in `path`) for its
  whole lifetime, including the build/test transition. Independent commands
  take compatible shared leases; they do not queue behind each other. An
  internal command parent also holds the lease until the actual command
  exits, so killing only the daemon does not expose surviving tests, and it
  lets go before it exits. Each holder leaves a marker in the lease while it
  holds it (`run-<nanos>-<pid>-<n>`, naming the command), so eviction knows
  when the oldest run in flight started.
  GC leaves alone (#108):
  - the directories of a workspace related to a worktree a run is in flight
    in (the workspace inside the worktree, or around it). It takes those
    leases exclusively, without waiting, and holds them through the rename,
    outside the victims and independently of `HELI_STATE_DIR`;
  - any victim that changed since the oldest run in flight started:
    nested commands can build and use another workspace's cache, a
    temporary one without an owner or another known one, and what they
    wrote is that recent. Both are real times (file times and the runs'
    markers), never the test seam's clock. Such victims are left out before the ladder
    chooses, so that it chooses others, and `gc` lists them as skipped
    ("changed since a run in flight started").
  heli's own notes in a build directory (`.heli/`) do not count as a
  change. Older, idle directories go while runs are in flight: one host-wide
  guard, as before, deferred all eviction for as long as any run was in
  flight, which on a host of busy agents is always (the store grew to fill
  its volume). B is a reclamation target, not a hard disk quota. A volume
  the store is on that is under its floor is a `doctor` warning
  (`store-floor`). When the volume is too full even to rename a victim into
  the trash (`os error 112` on NTFS), the victim is deleted in place, under
  the same locks. Bare Cargo's compilation locks remain respected, but bare
  runtime and detached background descendants require explicit integration
  and are not covered by this managed-command guarantee. All processes using
  the same caches must use the same heli version and activity namespace;
  `HELI_ACTIVITY_DIR` is only for explicitly isolated test/spike environments.
  It must be absolute. `gc --json` exposes `runtime_busy`: some directories
  were left alone for runs in flight; an activity namespace that does not
  resolve leaves everything alone. Failure to
  acquire a run's guard refuses that managed command rather than executing
  it without runtime protection (the narrow G5 exception above).
  A daemon-owned runtime guard releases its file before requesting another
  accounting/GC pass on every exit path. The request is retained under the
  accountant's condition-variable mutex, so an idle transition cannot be lost
  before the accountant starts waiting. This also covers `exec`, which has no
  Cargo learning worker. Shared activity still prevents eviction while another
  managed command runs. Activity released only by another daemon, a surviving
  command guardian or an external participant is observed by the existing
  periodic survey; this local notification is not a cross-process broadcast.
- **Never wait on a lock.** heli tries to lock `.cargo-build-lock` (build
  directory) or `.cargo-lock` and `.cargo-artifact-lock` (artifact
  directory) exclusively, in every profile directory of the victim's
  directory, and anywhere inside the victim (including nested Cargo caches
  left by build scripts). If one is
  held, or cannot be opened, the directory is busy and skipped. heli checks
  as it plans, and again as it evicts.
- **Rename, then delete.** Under the locks, heli only renames the victim
  into the trash on its volume, which takes constant time, and releases the
  locks: `<store>/.trash/` for a build directory's victim,
  `<target>/.heli-trash/` for a registered `target/` directory's. Windows
  refuses to rename a directory with a file open inside, so there heli lets
  go of the locks inside the victim (a whole build directory's) just before
  it renames, and the rename fails if a cargo opened one meanwhile. The
  trash is deleted afterwards, at low I/O priority (background mode on
  Windows, the I/O throttle on macOS, the lowest nice value on Linux): by
  `gc` before it exits, and by a thread of the daemon's, which also deletes
  what a `gc` that was stopped left there. A cargo that starts meanwhile
  finds its directory without the victim and rebuilds it; it never waits
  for the deletion. If a file in the trash is temporarily undeletable (for
  example, a Windows handle denies deletion), `gc` reports the failed
  cleanup and retains it for a later pass; the daemon also retries its
  trash cleanup. This does not restore the victim or hold Cargo's locks.
  A trash counts in no directory's size.
- **Narrow scope.** heli deletes only below registered roots (the stores'
  build directories and the registered `target/` directories), only the
  subpaths listed above, and never through a symlink or a junction: no path
  to a victim crosses a link, a link is never a victim, a link inside a
  victim is removed and not followed, and a trash that is a link is never
  used.
- **Unknown layout.** Lock names change between cargo versions. A profile
  directory with cargo's intermediates (`deps/`, `incremental/`,
  `.fingerprint/`, `build/`) and none of the three lock files has a layout
  heli does not know. Then eviction treats its directory as busy when
  anything in it changed within the last hour, and `gc` lists it so.

**Passes.** `cargo heli gc` makes an eviction pass: it plans the victims,
evicts them, logs each eviction in the event log (§8), and deletes the
trash; `--dry-run` names each victim with its tier and reason and changes
nothing. With a budget or a floor configured, the daemon's accountant
(§6.2), at each look (every minute, and when a run ends), keeps the
orphans' notes (§6.4) and makes a pass when its figures are over B or a
volume they are on is under F. One pass evicts at a time (`evict.lock` in
the state directory): `gc` waits for the daemon's, and the daemon skips its
own while `gc` runs. `status` shows the daemon's last pass that evicted
anything (§8).

### 6.4 Recycling: warm starts without sharing

A worktree with no build directory can appear in three ways: the first
`cargo heli` run in it, the git `post-checkout` hook that `cargo heli setup`
installs (git runs it for `git worktree add`), or the daemon's periodic
`git worktree list`. heli gives it the most recently used orphan of the same
repository, renamed to the new worktree's `{workspace-path-hash}` path. On one
volume that is a rename: no copy and no extra disk. One build directory still
belongs to one live worktree.

- **The donor** is an orphan (§6.2: its workspace's `Cargo.toml` is gone,
  none of cargo's locks in it held) in the store that holds the new build
  directory, whose owner manifest names the same repository; of those, the
  one with the latest last run. Never taken: live, busy and unknown
  directories, orphans of other repositories or of none, and what eviction
  moved into `<store>/.trash/` (§6.3). An orphan stays claimable until
  eviction takes it, which is no earlier than 24 h unclaimed (§6.3, tier 2).
- **The new build directory** is the one cargo names for the worktree
  (`build_directory`, §6.2): heli recycles only when it is in a store heli
  manages and does not exist. An existing one, even empty, is never
  replaced. Afterwards its owner manifest names the new workspace, keeping
  its creation time.
- **The first run.** The run's own `cargo metadata` (§6.2) names the build
  directory, and recycling happens before the run starts: the first run
  from a directory waits for it while a store holds an orphan of its
  repository (§5, "Where a
  run can wait"). The run's clients get a note:
  `cargo-heli: note: recycled the build directory of <donor>, a removed
  worktree; the path scan invalidated 2 units`, or after `worktree` for
  precise recycling (below) `, precisely (412 files got their recorded
  times)`.
- **The hook.** setup offers the repository's `post-checkout` hook (the file
  in its hooks directory, as `git rev-parse --git-path hooks` names it, so
  `core.hooksPath` counts; one file serves every worktree), with its own
  question, after the Cargo configurations (§6.1). git runs it after every
  checkout; only for a checkout from the null commit, which is what
  `git worktree add` does, does it run `cargo heli recycle` in the new
  worktree, which reads the store itself and needs no daemon. It runs
  before `git worktree add` returns, says what it recycled on stderr, and
  never fails the checkout. It names heli by its full path; setup replaces
  a hook it wrote earlier, and leaves any other one alone, with the reason.
- **The look.** As the daemon starts, as runs end and every minute, for each
  repository with an orphan in a store, it lists the worktrees
  (`git worktree list`). Each linked worktree it has not looked at before
  gets a build directory recycled, if it has none. The main worktree is
  never new, and is left to its first run.
- The hook and the look ask cargo from the top of the worktree, with their
  own environment, and recycle only for a workspace inside it. While no
  store holds an orphan of the repository, they do not ask cargo at all.
- Each recycling is a `recycle` event in the event log (§8). One recycler
  at a time recycles in a store: it holds the store's recycling lock,
  `<store>/.recycle.lock`, waiting at most 25 s for it (else it recycles
  nothing), and once it has it, looks again whether the build directory
  exists, since the hook, the look and a run can race for one worktree. On
  Windows a directory that another recycler walks (to scan it) cannot be
  renamed, so two recyclers of one donor could each make the other's
  rename fail, and cargo would then make an empty build directory. The
  operating system releases the lock of a recycler that dies. Beside it
  (an older heli takes none), a rename never replaces a directory that has
  files, each scans the donor under its old name, which a rename by the
  other takes away, and before each donor a recycler checks again that the
  build directory does not exist.
- **The claim.** Before anything else, a recycler rewrites the donor's
  owner manifest to name the new workspace (keeping its creation time),
  then scans and renames it. Its workspace lives, so from then on no other
  recycler takes the donor for an orphan, under its old name or its new
  one: a directory at a worktree's place never names a removed worktree,
  even for the moment between the rename and the manifest. After the
  rename the manifest is written once more, in case another recycler
  claimed the same donor after this one. A recycler that gives the donor up
  (it cannot be moved, its worktree is older than its last build) writes
  its manifest back, unless another recycler claimed it since or it moved.
  A donor whose manifest cannot be written is not taken. A recycler that
  dies between the claim and the rename leaves the donor named after a
  live workspace, where it is: recycling never takes it, and eviction does
  (§6.3). Under the store's lock no other recycler of this heli runs
  meanwhile; the claim also keeps a recycler that takes no lock (an older
  heli's hook) from taking the directory.

**Orphans and eviction** (§6.3), one rule for both. An orphan is unclaimed
from the first eviction pass that finds its workspace gone: the daemon's,
every minute while a budget or a floor is set, or `gc`'s. That pass writes
`.heli/orphaned` in it, `{"workspace":…,"since":…}`. The note counts while
it names the owner manifest's workspace and is no older than its last run;
a directory found live loses it, and `gc --dry-run` writes none. An orphan
becomes evictable (tier 2) only after 24 h unclaimed, and no other tier
deletes it whole, so recycling has at least that long to claim it. A
recycled directory names its new workspace, so its old note no longer
counts. Recycling never takes a directory that is already in the trash:
`<store>/.trash/` is not a build directory (`<2 hex digits>/<hex digits>`),
and an eviction renames its victim there before deleting anything. Either
rename of an orphan, by eviction or by recycling, fails once the other has
moved it, and the loser goes on without it.

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
lines in `deps/*.d`, observed on the example host). So heli scans the rustc
dep-info of the donor, where it lies, and only then renames it: a cargo that
starts in the new worktree never sees it half scanned. It deletes the
fingerprint of every unit with an `env-dep` value under the donor's
workspace or the old build directory path, and cargo rebuilds those units,
and what depends on them. This applies in both modes below.

- **Values.** rustc escapes `\`, line feed and carriage return as `\\`, `\n`
  and `\r` (on Windows, `# env-dep:CARGO_MANIFEST_DIR=E:\\heli`). A value is
  under a path when its components start with the path's: on Windows
  without case, with either separator, and without `\\?\`. The old paths
  are spelled as cargo spelled them: the workspace as `cargo metadata`
  named it (the owner manifest), and the build directory as cargo's
  `build-dir` places it in a store heli manages. A value that lists paths
  (`PATH`-like) is also looked at path by path. Any variable counts, not
  only cargo's.
- **Where cargo keeps them** (cargo 1.99, checked by experiment), in each
  profile directory (`<profile>/` or `<triple>/<profile>/`): rustc's
  dep-info is `deps/<crate>-<hash>.d`, `examples/<crate>-<hash>.d`, or for a
  build script `build/<package>-<hash>/build_script_build-<hash>.d`; the
  unit's fingerprint is `.fingerprint/<package>-<hash>/`, with the same hash
  (cargo's `-C extra-filename`). A crate's name has `_` where its package's
  has `-`, so only the hash ties the two. Units whose files have no hash (on
  Windows MSVC a path package's executables, and dylibs and cdylibs of path
  packages) write `deps/<crate>.d`; their fingerprint directory still has a
  hash, and its files are named after the target (`dep-<kind>-<target>`,
  `<kind>-<target>`), so such a file stands for every fingerprint directory
  with a file whose name ends in its crate's name, `-` read as `_`.
  Invalidating a unit too many costs only its rebuild.
- The number of units it invalidated is in the `recycle` event.

There are two modes:

Both start from the donor's **last build**: the newest modification time of
a file in its build directory, leaving out heli's own `.heli/` and rustc's
`incremental/` caches (as for the record's validity, below), read before the
path scan. heli stages the new worktree first (as for the record, below); a
worktree git cannot stage gets no recycled directory.

- **Conservative**, what heli does when the donor has no record that
  counts: only for a new worktree whose every file (tracked or untracked,
  as staged) was last written after the donor's last build, as in a
  worktree just made. Every workspace member then rebuilds. Third-party
  packages stay fresh, except those invalidated by the scan above (a crate
  that compiles an `OUT_DIR` path in, including through `include!`). This
  is correct by cargo's own rules plus the scan. A worktree with an older
  file (one made before the donor's last build: the main worktree at its
  first run, or a worktree the look finds) gets no recycled directory from
  this donor: cargo would find units built from another content fresh, and
  link the donor's code. heli tries the next orphan, and else the worktree
  builds cold. The cost of the `OUT_DIR` invalidations is to be measured.
- **Precise.** The donor has a *source-state record*: the raw content's
  SHA-256 digest, Git blob id, size and mtime of every tracked or untracked file.
  - **File times and the clock.** A file's mtime can be earlier than the
    moment, by the clock heli reads, at which it was written: the kernel
    dates a write by a coarse clock, up to a tick (some milliseconds)
    behind, and a file system keeps what its format holds, whole seconds on
    HFS+ and two on FAT. So wherever heli compares a file's mtime with the
    clock, it takes the file as written as late as its mtime plus a
    *slack*: 50 ms for an mtime with a fraction of a second, which comes
    from a file system that keeps fractions (10 ms or finer), and 2.05 s
    for a whole second, which may come from one that keeps whole seconds or
    two. A file changed right after heli's staging read it (below), to
    content of the same size, may be dated before the staging all the same;
    without the slack it would get the recorded mtime, and cargo would link
    the donor's build of the old content. heli reads each mtime from the
    file itself, never from a directory listing: on Windows a listing gives
    what the file's directory entry holds, which keeps the time of the
    file's creation until its writer closes it, and may lag on a loaded
    host, so a record taken by it could be taken before the build.
  - The record counts only if it was taken after the donor's last build: its
    time is no earlier than the mtime of each file in the build directory
    plus its slack, leaving out heli's own `.heli/` and rustc's
    `incremental/` caches, which no freshness decision reads (a build that
    changes what cargo decides writes a fingerprint). Otherwise heli ignores
    it. It is read before the path scan, which changes the directory.
  - For each file in the new worktree whose raw content digest and size match the record,
    heli sets the recorded mtime, if the file was written before heli began
    to stage the worktree: its mtime plus its slack is no later. So that a
    worktree just made gets the recorded mtimes, heli stages it again, once,
    when some file may have been written after the staging began, after
    waiting until none may have been (at most 2.05 s; a file dated later
    than that is not waited for). Every other file last written at the
    donor's last build or before gets the time now, and at least its slack
    after that build, so that a time cut to whole seconds stays after it (a
    worktree made before that build has such files); any other keeps its
    time, newer than every artifact already. Git's blob id alone cannot prove
    equality: filters can normalize different files of the same size, and
    `include_str!` compiles in bytes. The digest is checked again through
    the handle used to restore the time. Should a
    file not get its newer time, heli deletes the fingerprint of every unit,
    and cargo rebuilds them all.
  - Every file with the donor's content then looks to cargo exactly as it did
    in the donor, and every changed file is newer than any artifact. Only the
    changed crates, their dependents and the units the scan invalidated
    rebuild.
  - Only the recycler whose rename succeeded sets times, with the record of
    the directory it moved, after the owner manifest names the new
    workspace. It sets a file's time through a handle that on Windows lets
    nobody write to the file meanwhile: a file an agent writes while heli
    looks keeps its time.
  - The daemon writes a record after every run it executes, before the
    run's clients hear its end, so that a worktree removed right after its
    last run leaves one; and when it sees a build lock released: held at one
    look of its accountant (§6.2), free at the next, unless a run in flight
    there will write one as it ends, or the record still counts. Never while
    one of cargo's locks in the build directory is held, and not for a build
    the daemon did not see. A run the client executes without a daemon
    writes none. Before taking its time, the daemon waits until each file of
    the build directory is a slack old (at most 2.05 s, and 50 ms on a file
    system that keeps fractions of a second), so that the record of the
    build that just ended counts.
  - Ignored inputs, such as an `include_str!` of an ignored file, are not
    recorded, and neither mode looks at them. Cargo rebuilds when one is
    missing; one that is present triggers a rebuild when it was written
    after the donor's last build, as in a worktree just made. An ignored
    input of other content, present in a worktree made before that build,
    is the case recycling does not cover yet.
  - With `-Zchecksum-freshness` (cargo#14136), cargo compares content itself,
    and precise mode becomes unnecessary. The path scan does not.

  The record is `.heli/sources` in the build directory, JSON, replaced
  whole: `{"taken_at":…,"files":{"<path>":{"blob":…,"content":…,"size":…,"mtime":…}}}`,
  each path from the top of the worktree as git writes it, times in
  nanoseconds since 1970. `taken_at` is read before anything else, so a
  build that starts meanwhile makes the record stale, its files dated at
  most a slack before `taken_at`. The blob ids come from
  the result store's snapshot (§7): `git add -A` into a scratch copy of the
  index, then `git ls-files --stage`; raw content digests, sizes and mtimes are read afterwards,
  so a file changed in between looks newer, never older. Symbolic links and
  submodules are left out. An older record without raw content digests is
  ineligible for precise recycling; conservative recycling still applies.

**Later: cloning.** On volumes with block cloning (Windows Dev Drive on ReFS,
btrfs, XFS, APFS), recycling can clone instead of renaming, preserving mtimes.
A *live* worktree can then donate, forking a warm build directory. The same
rules apply.

### 6.5 Immutable dependency epochs (opt-in integration)

Separate worktrees and artifact directories do not remove Cargo's shared
package-cache lock. The experiments linked in `docs/multi-agent.md` verified
private writable Cargo homes consuming shared immutable vendor sources. This
ownership model is now available explicitly, through ordinary managed runs:

```text
cargo heli deps import --store <absolute-new-store> --vendor <vendor-tree> --config <cargo-vendor-output.toml>
cargo heli --dependency-epoch <printed-epoch-path> --cache off check --offline
cargo heli --dependency-epoch <printed-epoch-path> test --offline
cargo heli deps gc --store <absolute-owned-store>
```

Import accepts the source-only replacement TOML emitted by `cargo vendor` and
one supplied directory source. It verifies the complete file inventory and
each Cargo file checksum before and after copying. Original archive checksums
are preserved for Cargo's lockfile verification; extracted files alone cannot
authenticate the original archive. Unsupported configuration, links within
input packages and path escapes are refused. Existing unknown directories
are never adopted as managed stores.
Informational checksum-envelope fields such as Cargo's `$comment` are
accepted, as Cargo accepts them; the raw checksum file remains part of the
object identity and the actual file inventory is still verified strictly.

The store holds package objects by content identity. Epochs contain source
configuration, a manifest and cheap views of those objects: NTFS junctions on
Windows and directory symlinks on Unix. Different epochs can reference the
same physical object. A complete staged epoch becomes visible by rename;
partially published or corrupt state is refused, rather than executed.
This is atomic visibility, not a claim of durability through arbitrary
power loss. Objects include names, bytes, empty directories and executable
bits in their identity. Opening an epoch verifies the stored bytes as well
as its manifest, configuration and link targets.

Each epoch has an external shared lifetime lease. It is acquired before
reading its manifest, including before result lookup and metadata discovery.
The daemon holds ownership across queueing, build/test phases and final
keying; asynchronous metadata learning owns a separate lease. Each selected
epoch's Cargo metadata subprocess also runs under an independent guardian:
a daemon-thread lease alone cannot protect an orphaned metadata child after
the daemon dies. Hooks and daemon maintenance queries preserve a nonempty
inherited epoch selector too; they must not lose ownership merely because
they bypass normal client preparation. Discovery capture preserves raw Cargo
JSON without a runner handshake prefix. The command's
guardian acquires and validates ownership before spawning Cargo, retaining
it after client or daemon loss. Direct fallback uses a separate guardian
too, preserves the prepared execution context and raw stdio, and never
falls back to the original shared Cargo home. Invalid snapshots are terminal
errors. Protocol 2 prevents silent execution by a daemon that does not honor
this contract. A `Preparing` response acknowledges a live daemon before
potentially expensive validation, without treating validation as a failed
five-second connection handshake.

Explicit dependency GC tries exclusive epoch leases without waiting for
builds. It may retire an unleased epoch while another epoch remains active;
all objects referenced by retained epochs stay live. Publication and GC
serialize catalog mutations. Staging and GC's object verification happen
outside that critical section; ready readers do not acquire the catalog
lock. Duplicate import currently re-verifies reused objects under the catalog
lock, so concurrent publishers can wait there; active ready consumers do not.
Selecting an epoch already being retired fails promptly rather than waiting
for its trash deletion; callers can select a ready epoch or retry publication.
Epoch lease files are never unlinked, avoiding split-lock identities after
retirement and republication. Trash deletion does not follow package links.
This source lifecycle does not weaken the conservative global runtime guard
used for arbitrary build artifacts in §6.3.

The invoking client prepares a stable private Cargo home for its worktree,
epoch and original configuration/credential inputs. It publishes rather
than rewriting an existing home. Cargo 1.99 or newer is required. Original
`config.toml` is included at home precedence with its original path origin;
workspace and CLI overrides retain their native higher precedence. The
original config remains a live file, as in ordinary Cargo. Credentials are
copied privately outside shared objects: Unix owner-only modes and Windows
protected owner-only ACLs are established before bytes are written. An
unsafe existing private home is refused. Nested Heli retains the selected
home and epoch instead of building recursive include chains. Cargo,
toolchain and wrapper selection retain the caller's PATH order; the original
home's bin directory is appended only when absent.

This immutable-source profile requires explicit preparation and selection.
The native home profile in §6.6 needs no epoch publisher or vendor prepass;
automatic epoch publication is an optional storage optimization, not a
prerequisite for independent Cargo downloads. Capacity accounting for
sources/staging/private homes/lease metadata and automatic private-home
reclamation remain unfinished. Legacy home `config` is
explicitly refused because Cargo includes require `.toml`; copying it would
change relative-path semantics. Arbitrary cargo extensions, competing
home-bin/PATH extension precedence and tools mutating the Cargo home need
the tool profiles tracked in #38. Native workspace/CLI source overrides can
select sources outside an epoch; this feature does not claim to lease those
external directories. The existing artifact budget does not yet include
this opt-in dependency store. No hard disk or RAM bound follows from enabling
the profile alone.

Public acceptance cases are in `tests/dependencies.rs`: two live Cargo
builds under a held baseline package-cache lock; collecting an unrelated
epoch during those builds; delayed source reads after client/daemon loss
and during direct fallback; original config origins/precedence and private
credentials; and refusal of corrupted source objects before execution.
They also keep a gated metadata child alive after the foreground command and
daemon end, and switch source values 7 → 8 → 7 with one unchanged lockfile and
target while retaining both source views. The production concurrency canary
in `experiments/dependency-epoch-concurrency.py` exercises shared-object GC
between exits of three consumers across two projects and two Git worktrees.
Cross-platform results must be reported for the exact tested revision.

### 6.6 Automatic worktree Cargo homes

The independence profile uses ordinary writable Cargo homes, one per
worktree, rather than requiring a vendor snapshot before a command can run.
With `[cargo] home = "isolated"` in the user configuration, or
`--cargo-home isolated` on a command, the invoking client prepares the home
before submitting the command. Default `shared` preserves the caller's
Cargo home. Explicit epochs remain an optional source-storage optimization;
an explicit shared-home request conflicts with an epoch selection.

Preparation creates configuration and private credentials, not a dependency
resolution pass. Cargo itself downloads, resolves and builds with its native
flags. Each worktree owns its mutable registry, index, extracted sources and
git cache. None are links to another worktree's or the original home's
mutable cache. Concurrent cold worktrees may download the same crate twice;
avoiding their mutual package-cache lock takes priority over storage savings.
Home preparation does not wait for another worktree to download its
dependencies. Live verification sharing still follows the complete input-key
policy in §5.

The secure publication mechanism in §6.5 also applies without an epoch:
original home configuration is included at home precedence with its relative
origins intact, credentials are copied only into protected files, and a
stable generation is published rather than rewritten under active commands.
Worktree identity is the canonical nearest checkout root, not Git's common
directory; outside Git it is the invoking directory. Config or credential
changes select a new generation for a new outer command. Already running
commands and their nested Heli invocations retain their exact parent home,
including in generated directories outside the parent's repository.

Native generations use the compact `<state>/h/<32 hexadecimal digits>`
layout. The directory name is a prefix, not the authority for ownership:
the schema-3 marker records the complete 256-bit home identity and reuse
must compare that identity. A prefix collision or an inconsistent marker
is an error; it must never attach a command to another worktree's mutable
cache. Existing epoch layouts and inherited schema-1/2 homes retain their
paths. New layout selection does not move or delete an active generation.

The short layout is a native-tool compatibility requirement. In the real
two-worktree FLUI acceptance, an existing aws-lc C source reached 268 path
characters and MSVC reported C1083, even though Rust could create and read
the file. A regression compiles a C source at that dependency's actual
registry depth with the installed MSVC compiler. Shortening only the test
checkout would conceal this defect. An arbitrarily deep custom state root
or dependency can still exceed a native tool's path limit; use a short
`HELI_STATE_DIR` on Windows when selecting a custom root.

Cold `--offline` runs can fail because the private home has no dependencies
yet. Heli must not hide that failure by retrying through the original shared
cache. Warm offline runs reuse their worktree's cache. No hidden step creates
or updates `Cargo.lock`; `--locked` retains Cargo's normal meaning. Interrupted
downloads are recovered by Cargo in that home. Selected isolation survives
daemon loss and direct fallback rather than silently becoming shared.

Home-mutating Cargo commands need an explicit destination policy. This
profile refuses `install`, `uninstall`, `login`, `logout` and `publish`, and
the tools whose profile writes the Cargo home (`writes_home` in §5:
`binstall`, `install-update`, `quickinstall`), instead of silently changing
their original destination or installing into a private `bin/` that nothing
has on `PATH`. A command that only prints is not refused and runs as bare
Cargo would, with the caller's own home: `install --list`, which reads the
home's record of installed tools (so it lists what is really installed, not
the empty private home), and `-h` or `--help` of any of those commands, and
`-V` or `--version` when it is the only argument (`install` and `binstall`
take `--version <semver>`). `install foo --help` counts as help. Inside an
already isolated parent the home stays the private one. An explicit shared request
inside an inherited isolated command is refused rather than escaping its
parent's context. Use an outer shared command for home maintenance. Arbitrary
extensions and executables may interpret Cargo home themselves; their full
compatibility remains part of #38. Existing Cargo 1.99+ and legacy home
`config` limitations apply. Private native homes are retained and are outside
artifact-store accounting; automatic retirement requires lifetime ownership
before deletion. No hard resource bound follows from this profile.

Acceptance must exercise the public CLI with two actual Git worktrees and
a localhost registry: delay A's real download and observe B's build-script
and compiler progress before releasing it; resolve a nested generated
project whose dependency is outside the parent's graph; preserve original
home/config/credential and project lockfile bytes; distinguish cold and warm
offline runs; and verify configuration origins, private credentials, unsafe
home refusal and direct fallback. `tests/cargo_homes.rs` covers the public
home policy, config/credential semantics, simultaneous worktrees and a real
fallback command surviving client loss with its GC guard. The local registry
fixture `experiments/isolated-home-concurrency.py` covers actual cold downloads,
nested resolution, offline behavior and optional authenticated access through
Cargo's token provider. Native Windows runs pass; cross-platform verification
must be reported for the exact tested revision. This is not a completion
claim for #38 or the overall multi-agent acceptance.

**Isolation and live-sharing integration, 2026-10-16.** The native-home
profile is integrated with the source-aware live policy from §5. Home
selection happens before submission and inventory uses the selected home.
The generated home's location alone does not prevent equivalent verification
from sharing; its Cargo configuration and all other keyed inputs still count.
Incomplete offline source discovery disables sharing rather than certifying
an unknown dependency graph.

Public real-Cargo tests in `tests/single_flight.rs` cover both shared and
isolated modes: two same-worktree test clients execute one test process,
then a source edit executes the new revision after the old executable exits.
Both direct tests and `exec --verify` wrappers exercise that transition.
Equivalent checks can share across isolated worktrees with result caching
off, while builds in another worktree or output directory produce their own
actual `.rlib` files. No persistent verdict marker is created by those runs.

The native-home registry experiment now uses `exec --verify`, exercising
source-key preparation as well as isolation. Local Windows integration
runs pass its ten unauthenticated and thirteen authenticated checks: B
finishes with real compiler output while both A's download and a shared-home
control remain blocked; nested resolution retains B's home; original
configuration, credentials and lockfile bytes remain unchanged. These are
mechanism checks, not a throughput comparison or a hard disk bound. They do
not remove Cargo's artifact lock for distinct commands sharing one target,
the nested-run admission bypass, or the need for separate worktrees for
independent editors.

The integrated Windows revision passes the full 419-test suite across 38
targets, strict all-target Clippy and formatting. Five acceptance-driver
safety tests also pass. Cross-platform results must refer to the integrated
revision, rather than the earlier private-home or live-sharing CI alone.

## 7. Result store

For each passing verify or test key, heli keeps a marker
`<state>/verified/<digest>` holding the run id, the worktree and the time. It
keeps at most 256 markers and drops the least recently used first; a hit
counts as a use. The daemon keeps the store: a run that the client executes
directly, without a daemon, neither looks it up nor records.

**The key.** These rules carry over the lessons of flui#1619, where each one
was found by a failing test.

- **Encoding.** Every field is hashed as its name's length, the name, the
  value's length and the value. Values are raw bytes, never lossy UTF-8:
  environment values, Cargo configuration files, tool output.
- **Tree.** The worktree with every change staged: `git add -A` into a
  scratch copy of the index (`GIT_INDEX_FILE`), then `git write-tree`. The real
  index is never written (`GIT_OPTIONAL_LOCKS=0`). The copy keeps the
  index's stat cache, so unchanged files are not hashed again, and the
  index's own time. git takes a file whose times and size match its entry
  for unchanged unless the entry is racily clean (no older than the index),
  and where git compares whole seconds (its Linux builds), a file changed
  to the same size in the second the index was written matches its entry:
  only that check finds it. A copy dated later (as `std::fs::copy` dates it
  on Linux) hid such a change, and git kept the old blob: a false hit here,
  and in the source-state record (§6.4) a stale link, which a recycling
  test showed on a Linux runner.
  The key additionally hashes the raw bytes of every listed regular file:
  Git clean filters, including `ident` and line ending normalization, must
  not equate different compiler inputs. Gitlinks include the submodule's
  recursive current inventory rather than only its recorded commit.
  Cargo metadata selects the actual workspace (`--manifest-path`,
  toolchain and configuration included). Offline resolution with all
  features inventories local packages, including external path dependencies
  and registry/git patches; their Git roots contribute raw content too.
  That resolution needs every package of the lockfile in the Cargo home. A
  private home (§6.6) lacks optional and other-platform dependencies it
  never built, and then the manifests alone (`--no-deps`) are the
  inventory. They declare every path dependency, optional and
  platform-specific ones included. The local crates that `[patch]` and
  `[replace]` put in place are read from the root manifest and from the
  Cargo configuration. Registry and git sources are already keyed through
  the lockfile, which is in the tree (#66).
  If the inventory cannot be established, or a local source has no Git
  inventory, the command runs without result caching. Existing key markers
  use a different composition and cannot match these keys.
- **Base.** For a command that diffs against a base, the merge base too. No
  verify or test subcommand does; `exec`, which can run such a command, does
  not use the store yet.
- **Machine.** Host name, OS and architecture, because a state directory in a
  shared home must not certify another machine.
- **Toolchain.** `rustc -vV` (`RUSTC` when set), and the versions of cargo
  and of the tool the run uses (clippy, rustfmt, nextest), each asked in the
  client's working directory with its environment, so that the pinned
  toolchain answers. A missing tool is keyed as absent.
- **Cargo configuration.** The content of every `config.toml` and `config` on
  cargo's discovery path: `.cargo/` in the working directory and every
  directory above it, then the Cargo home (a relative `CARGO_HOME` against
  the working directory). They are keyed in the order cargo reads them, not
  by path, so worktrees in different places can share a key. A file that is
  there but cannot be read is keyed by its error, never as absent.
  Files explicitly selected by `--config` are keyed by content too;
  inline `KEY=VALUE` settings already contribute through argv.
  Transitive `include` files contribute their raw bytes in Cargo's order,
  including `--config 'include=[...]'`: file includes resolve relative to
  their containing file, inline includes relative to the client's cwd,
  with Cargo's lexical path normalization. String entries and tables
  (`path`, `optional`) are supported; unknown table fields are ignored as
  in Cargo. Missing optional files contribute an absence marker, so their
  creation invalidates a pass and removal restores the original inputs.
  Invalid, cyclic, unreadable required includes, or a graph exceeding 64
  files disable result caching rather than certify incomplete inputs.
  Repeated include paths count as cycles, matching Cargo 1.99. The whole
  include graph is reread after the run; keys use composition 3, excluding
  markers written before transitive configuration identity was included.
  [Pinned Cargo include loading](https://github.com/rust-lang/cargo/blob/5f94df4789f005f9a352888e8355ffc645b7ed0e/src/context/mod.rs#L1386)
- **Argv**, normalized: without the program, which the toolchain stands for,
  and with the working directory relative to the worktree, which selects
  packages and configuration as an argument would. The worktree's own path is
  not keyed.
- **Environment.** Every variable except a neutral list:
  - Names are case-folded on Windows only.
  - `PATH`, `LD_LIBRARY_PATH` and the `DYLD_*` search paths are keyed without
    their entries inside the worktree, which cargo prepends for the programs
    it runs; one left without entries counts as unset. An entry is compared
    with the worktree on Windows without case and with `\` read as `/`;
    elsewhere `\` is part of a name.
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
  skips on this host. No verify or test subcommand has one yet.

**When the key is computed.** It is checked twice: before the run waits for
anything, and again once admitted and past the worktree lock, because another
run may have recorded it meanwhile. A hit before the wait is never admitted
as a run: it takes no run id; a miss goes on to single-flight with that key
(§5, step 3). The key is computed afresh after a wait, since the runs waited
for may have changed the tree. A run that waited for nothing keeps the key
of the first check, taken a moment before, so its clients from other
worktrees attached to the tree it runs on. After the steps the whole key is
computed again, every input read anew, and compared with the one taken
before them, so a change in between (to the tree, the Cargo configuration
or the toolchain) counts as one during the run. A marker is written only
when the run passed and both keys match. A run that fails drops the
marker of its key, so a flaky pass is reused only until a run of the same key
fails.

**Caveats**, printed on every hit: a flaky test that passed once stays green
for that key (`--no-cache` runs it again), and ignored files are not part of
the key. `--cache off` skips persistent result lookups, recording and
invalidation, but shares matching checks/tests already in flight using the
same complete source/configuration/toolchain/environment key. After an edit,
the next request starts a different run; after a conflicting worktree wait,
the key is recomputed. No complete key means no sharing. This policy has its
own wire value (`live`), so older daemons refuse it instead of silently
discarding freshness. The legacy `disabled` value retains its old no-sharing
semantics for older clients. Full source inventory uses Cargo's resolver lock
even offline and can delay the first compiler. `--cache on` is the default
for checks/tests; `--cache refresh` is equivalent to `--no-cache`. Compile
commands always use source-keyed live sharing, regardless of this option;
mutate and ordinary exec commands retain command-based sharing.
`exec --verify` is verify and defaults to `off`; an explicit cache policy can
opt this declared verification into persistent verdicts. Its output and tool
arguments retain exec's behavior. Source inventory currently requires a Cargo
workspace and its complete local Git source graph; a wrapper that cannot
supply that inventory runs independently. It does not snapshot-isolate files
from concurrent editor writes.
`--no-cache` skips both lookups, not the recording, and attaches to
no run in flight (§5, step 3).

Compile runs are never recorded, because their product is the artifact, not
the verdict; nor is a request for help (`-h` or `--help`, also the test
runner's after `--`) of a verify or test subcommand.

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

- `daemon`: `pid`, `protocol` and `started_at`, and where it runs, so that
  a user can see it runs in their interactive session (§3): `session` (on
  Windows the logon session, elsewhere `getsid`), and on Windows
  `window_station` and `desktop` (`WinSta0` and `Default` for the user's
  desktop) and `interactive` (the window station `WinSta0` of a session
  other than 0; `null` elsewhere, where heli cannot tell). The text form
  says the same on the daemon's line:
  `daemon  pid 4242, protocol 1, session 1, WinSta0\Default (interactive), up 52s`.
  `null` when none answers;
- `pool`: `slots`, `free`, `withheld`, `in_use` (`slots − free − withheld`,
  never below 0), or `null` when no daemon answers;
- `memory`: the governor's last sample, in bytes: `total`, `available`,
  plus `commit_available` on Windows, and the `low_watermark` and
  `high_watermark`; `null` when no daemon answers or memory cannot be read;
- `runs`: one entry per run, with `id`, `worktree` (the nearest directory
  holding `.git`, else the working directory), `argv`, `class`, `priority`
  (`interactive`, `normal` or `background`, §4; the text form shows it when
  not `normal`), `phase`
  (running for a run of one phase; building, testing, done; `unknown` for
  a newer daemon's phase the client does not know), `slots` (the
  host pool's slots it uses: lent to its run pool and not free there, plus
  its test slots), `share` (its share of the capacity, §4, a number that
  need not be whole), `waiting_since` (`null` unless the run waits),
  `waiting_for`
  (the ids of the runs it waits for at the worktree lock),
  `attached_clients` (the clients following it now), `rerun_of` (the run a
  rerun reruns, §5 "Reruns"; `null` for any other run, and the text form
  adds `rerun of r17` to the run's state), `started_at`, and
  once it has ended `ended_at` and `exit` (`code`, `signal` on Unix,
  `cancelled`), else `null`. The runs in flight are listed, and the last
  32 runs that ended while no client followed them, as done: nobody else
  was told how they ended. A run whose clients heard its end leaves the
  list; a done run holds no worktree lock and counts as no run in flight
  (for the top-up's quiescence, §4);
- `disk`: `stores`, one entry per store, with `path`, `budget` (B, which
  every store and registered `target/` directory share, in bytes; `null`
  without one), `used` and `by_category` (`incremental`, `deps`,
  `build_scripts`, `fingerprints`, `tool_caches`, `other`, in bytes); and
  `targets`, one entry per registered `target/` directory, with `path`,
  `used` and `by_category`. `null` when no daemon answers;
- `last_gc`: the daemon's last eviction pass that evicted anything (§6.3):
  `at`, and `evicted`, one entry per victim with its `path`, `tier` and
  `bytes`; `null` before the first, and when no daemon answers.

There is also a rotating log of admissions, loans, withholdings, top-ups,
drains and evictions, and `cargo heli gc --dry-run`.

**The event log** is `events.log` in the state directory
(`events-<session>.log` on Windows), one JSON object per line, each with `at`
(seconds since 1970, UTC) and `event`:

- `admit`: `run`, `worktree`, `argv`, `class` and `profile` (where the class
  came from: `cargo`, `built-in`, `user`, `repository`, `unknown` or `exec`;
  §5, "Tool profiles");
- `direct`, written by the client for a run it makes itself in the pool:
  `worktree`, `argv` and `profile`;
- `withhold`: `withheld` (after it), `available`, `commit_available` and
  `low_watermark`;
- `return`: `slots`, `available`, `commit_available` and `high_watermark`;
- `top-up` and `drain`: `slots`, `free` (after it) and `target`;
- `loans`, when a run ends: `run`, `loans` (slots lent to its run pool),
  `taken_back` (slots the broker took back while it had work pending),
  `most` (the most slots it was seen using), `wait_median_ms` and
  `wait_longest_ms` (how long it waited for a lent slot, at most; `null`
  without loans);
- `recycle`, when a worktree got an orphan's build directory (§6.4):
  `trigger` (`run`, `hook` or `look`), `workspace` and `to` (the new
  workspace, and the build directory's path now), `donor` and `from` (the
  removed workspace, and the path the directory had), `invalidated`
  (the units whose fingerprints the path scan deleted), `mode` (`precise`
  or `conservative`) and `restored` (for precise recycling, the files that
  got their recorded times, else `null`). The hook writes it itself, to the
  log of its logon session's daemon;
- `evict`, for each victim an eviction pass of the daemon's or of `gc`
  renamed into the trash (§6.3): `tier`, `path`, `bytes` and `reason`.

Past `[event_log] max_kb` (default 1024) it becomes `<name>.1`, replacing
the one before, and the next event starts a new file.

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
| Daemon not running | A bare cargo warns and uses its own pool, as today | The pool goes with its daemon once nobody holds it: a semaphore by itself, a FIFO through the daemon's warden (§4, Lifetime). `cargo heli` or `cargo heli ensure` starts the daemon; logon task or session hook |
| Daemon and its warden both gone (Unix: a kill of both, a power loss) | The FIFO stays, with no bytes: a bare cargo runs one job at a time, without a warning | The next daemon fills it again (§4); `cargo heli ensure` as a session-start hook, or the logon task, starts one early |
| Daemon unreachable, silent for 5 s, of another protocol version, or older and without a request the client sends | `cargo heli` cannot hand over the run, or the request | The command runs directly (`log` reads the record itself) after one warning line; a newer client asks an older daemon to exit once idle (§3) |
| Daemon lost while a run waits (worktree lock, recycling) | The command never started | The client runs it directly, after one warning (§3) |
| Daemon lost once a run's command started | The client cannot tell whether the command finished | The client exits 1 and does not run the command again |
| A client is killed, or stops reading, during a run | none | The run goes on and its record is complete; `status` lists it as done if no client heard its end; the same command attaches to it while it runs (§5) |
| Two daemons on one state directory (Windows logon sessions) | They could give the same run id, and write one record over another | Ids come from the state directory's counter under a lock, above every record (§5) |
| An agent harness stops the command that started the daemon | Observed (Claude Code, Windows): it kills the command's process tree, found through parent processes | The daemon is started through a short-lived intermediate, so it descends from no client (§3) |
| An agent harness terminates its whole job object | On Windows, a daemon started on demand inside a job that refuses breakaway (Claude Code's) dies with it, and every run with it | Not seen in normal operation: that job holds the whole app and has no kill-on-close limit. Once `setup --autostart` registered the logon task, the daemon starts outside any agent's job: at logon, and when a client needs one, through `schtasks /Run` (§3) |
| Daemon started outside the user's interactive session (a service, or a task that runs while the user is logged off) | Runs lose the desktop: tests that drive real windows fail | The daemon runs only in the interactive session: started by a client, or by a logon registration restricted to the logged-on user (`InteractiveToken`, a systemd user unit, a launchd agent in the Aqua session; §4, "Autostart"). `status` shows the daemon's session, and on Windows its window station and desktop and whether they are interactive (§8) |
| The logon registration is gone or stale (removed by hand, heli installed elsewhere, other locations) | A start through it would bring up no daemon, or the wrong one | A client uses it only when the record names its own program and locations; when the service manager refuses the start, the client starts the daemon itself (§3) |
| Daemon crashes mid-build | Running cargos keep their handles: a Windows semaphore lives while any handle is open, and a FIFO keeps its bytes while any process holds it open (its path persists either way) | The restarted daemon adopts a pool still held, as it is; a FIFO that nobody held has lost its bytes, and the daemon fills it again (§4) |
| Slot leak by bare cargo | The host pool shrinks | Top-up at quiescence, bounded by the target (§4) |
| Run killed while holding slots | none | The run pool's loans go back in full (§4) |
| The pool holds more than the target (a FIFO has no maximum; a semaphore's is N, above the target while slots are withheld) | Oversubscription | The daemon drains the surplus at once (§4) |
| Memory spike | New jobs wait | Slots are withheld; running jobs finish |
| GC races a build | none | Try-lock, rename to trash, skip busy directories |
| `gc` or the daemon stopped while deleting the trash | What is left there takes disk | It counts as free for the floor and in no directory's size; the daemon's deleter, at start and every minute, or the next `gc`, deletes it |
| Recycled unit with a donor path compiled in | Tests read the removed worktree | The rustc dep-info scan invalidates the unit (§6.4) |
| Recycled directory with a stale record | Could link stale code | A record counts only if it is newer than every file in the build directory; otherwise conservative mode |
| Recycling into a worktree made before the donor's last build (the main worktree's first run, the look) | Its unchanged-looking files are older than the donor's artifacts: cargo would link the donor's code | Precise mode gives every file not of the recorded content a time after that build; conservative mode does not recycle into such a worktree (§6.4) |
| A recycler taking a directory another one has just renamed into place | One worktree's build directory moved to another | The donor's manifest names the new workspace before the rename: a claimed donor is no orphan (§6.4, "The claim") |
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

**Headstart as a candidate toolchain.**
[Headstart](https://github.com/PowderworksCode/headstart/tree/ff81bfecda2116d54776a7fec538fc2f4e349d1f)
patches Cargo and rustc to start dependents on early interface metadata. It
can complement host-level scheduling, but it has not been tested with heli.
Its [readiness assessment](https://github.com/PowderworksCode/headstart/blob/ff81bfecda2116d54776a7fec538fc2f4e349d1f/docs/readiness.md)
identifies two relevant limits: a resumed compiler can run without a
jobserver token, and paused compilers retain memory without a count limit.

The useful lesson for heli is to measure active CPU work and resident memory
separately, and to validate the inherited jobserver contract of an optional
toolchain before recommending it. An integration experiment must compare
stock and patched toolchains with one and multiple concurrent worktrees,
record token accounting through wait/resume, peak memory and total wall time,
and verify dependency-error/crash exit status and absence of cached false
greens. The §6.1 FLUI run uses stock Rust and establishes none of those
Headstart properties. heli does not enable its flags or ship compiler patches.

## 12. Plan

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
until managed activity is idle (§6.3); continuous load has no hard disk cap.
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
§6.6 prepares an isolated home automatically after explicit selection. The
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
nested-Cargo compatibility scope in §6.1 or the repository opt-in Q2 decision.

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
the global runtime guard deliberately postpones reclamation (§6.3).
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
See [the acceptance report](docs/research/shared-verification-acceptance.md)
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
  suite passes with `build-dir` set (§6.1). Operational acceptance uses the
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
  Weighted shares by priority are done (§4, "Priorities").

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
2. **Resolved in #27:** build placement is opt-in per repository (§6.1).
   A user-wide setting requires a separate explicit choice. FLUI validation
   establishes compatibility for the measured toolchain and test scope;
   it cannot establish compatibility for every project, coverage tool or
   volume on the machine. Repository configuration does not reach temporary
   nested projects; a scoped `CARGO_BUILD_BUILD_DIR` can include those in an
   opted-in run without changing the user's Cargo configuration.
3. The default budget B (proposed: 150 GB on D:) and floor F (proposed: 50 GB).
   Until this is settled, neither has a default, and nothing is evicted
   unless the configuration sets one (§6.3).
4. Is a pool per logon session enough, or do agents run in other sessions
   (SSH, services)?
5. Should the default without a terminal be `compact` or `quiet`?
