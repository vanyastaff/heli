# Host pool

## Mechanism (verified)

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
lever on its parallelism. That is why "Run pools" below exists.

The probe [`experiments/jobserver-probe.ps1`](../experiments/jobserver-probe.ps1)
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
its daemon is gone and nobody holds it, a bare cargo pointed at it ("Reach"
below) cannot open it, prints cargo's warning, and builds with a pool of
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

## Run pools

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
  test slots ([runs](runs.md)). When a run ends, or is cancelled or killed, everything lent
  to it goes back to the host pool at once, whatever its processes held.
  Leaks are impossible for heli runs; slots lent by a daemon that died come
  back through the top-up.
- **Latency.** For every slot it lends, the broker records how long the run
  may have waited for it: from when its pool was last seen with a free slot
  (an upper bound). The `loans` event gives each run's median and longest
  wait when it ends ([observability](observability.md)). A waiting process gets a slot within one broker
  tick while the host pool has one free. Measured on the example host
  (Windows, N = 8, `cargo heli build` of heli itself, 7.3 s): 49 loans, at
  most 8 slots in use, every wait at most 20.6 ms, one tick. Cargo's
  jobserver helper asks for one token at a time and is not yet waiting
  again when the broker counts once more, so a run gains about one slot
  per tick: 32 slots take some 0.65 s, short next to a rustc job.

Bare cargo, started without heli, still draws from the host pool directly.

## Priorities

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
share ([runs](runs.md)). The memory governor treats every priority alike.

## Saturation

The pool is **saturated** while the host pool has no free slot, two runs
or more are active, and one of them waits for a slot (#127); a spell of
it ends only after a second without (a slot given back and taken again
leaves it free for a moment). heli queues
nothing for it (G1): each run keeps its share, and a run of the whole
workspace among 17 others gets a slot or two, its tests one thread. What
heli does is say so, where the agents and the operator look:

- **`status`**: the `pool` line adds how long the pool has been saturated,
  the runs waiting for slots out of the active ones, and the median and
  90th percentile of every run's waits for a lent slot over the last 10
  minutes (the waits only while it is saturated or the 90th percentile is
  a second or more):
  `pool    32 slots: 32 in use, 0 free, 0 withheld; saturated for 4.2 min, 17 of 18 runs waiting for slots; slot wait p50 45 s, p90 5.6 min (last 10 min)`.
  The JSON has them in `saturation`: `active_runs`, `waiting_runs`,
  `saturated_s`, `wait_p50_ms`, `wait_p90_ms` and `window_s`
  ([observability](observability.md)).
- **The run's stderr**: a run that joins a pool saturated for 5 s or more
  (`SATURATED_AFTER`; every build fills the pool for moments as its units
  start and end) and that selects the whole workspace (`--workspace`,
  `--all`, or eight `-p` or more) gets one header line before its command
  starts: `cargo-heli: r2096: 18 runs share 32 slots, saturated for 340 s;
  this run gets about 2. Narrow it (--affected, -p) or run it with
  --priority background.` A run of a few packages, `cargo heli exec` and
  quiet output that succeeded do not show it.
- **The event log**: each spell that lasted 5 s or more is a `saturated`
  event as it ends, and a nextest run that failed while the pool was
  saturated logs the tests its slow-timeout killed as `timeouts`.
- **`doctor`**: `pool-saturated` when the pool was saturated for more than
  half of the last hour, with the most runs and waiting runs and the
  advice above; `nested-cargo-timeouts` when nextest timed tests out while
  it was. A test that runs cargo itself (a generated project, a facade
  check) waits for slots like any build, and under a saturated pool it
  outlasts its slow-timeout.

The fix stays on the agents' side: `--affected` (which also takes
nextest's `-E`), short `-p` lists, and `--priority background` for sweeps.

## Properties

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
    slots withheld ([observability](observability.md)).
- **Bounded top-up.** Only bare cargo can leak a slot: a process killed while
  holding one, and semaphores and FIFOs have no owner. The host pool's target
  of free slots is `N − withheld − Σ lent`, where Σ lent counts the test
  slots held for runs ([runs](runs.md)) too.
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

## Reach

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
client does ([daemon](daemon.md)): it takes no new runs (they run directly, after one
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
version. **`cargo heli self-upgrade`** first puts a newer version in this
program's place, then does the same with it (docs/daemon.md, "Upgrading").
`status` shows the daemon's version and program (`version` and
`executable` in the JSON document, `null` from a daemon older than them),
and the text form says when the daemon is older than the client, with
`restart` as the fix (#87).

## Autostart

**`cargo heli setup --autostart`** registers the daemon to start when the
user logs on, in their own session, never as a service and never while they
are logged off: a daemon there would have no desktop. Clients then start the
daemon through the registration too ([daemon](daemon.md), "Starting on demand").

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
  refuse: these are the locations [daemon](daemon.md) tells a packaged app's user to set
  (#70, #82). The refusal names the difference. The unit and the agent name
  the locations setup resolved, so that the service manager's environment,
  which lacks what a shell's profile sets, cannot move them.
- **The record**, `autostart` in the state directory, names the program and
  the locations setup registered. A client starts the daemon through the
  registration only when the record names its own ([daemon](daemon.md)). setup removes the
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
