# heli architecture

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

## Goals and non-goals

Goals:

- **G1, no head-of-line blocking.** A run starts compiling at once, with at
  least one job, whatever else runs. The only places a run can wait at all are
  listed in [runs](runs.md), "Where a run can wait".
- **G2, one CPU and memory budget per host**, including cargo runs heli did
  not start.
- **G3, a disk budget enforced automatically**, deleting only what cargo
  regenerates.
- **G4, warm worktrees.** A new worktree starts warm, and two live worktrees
  never share a build directory.
- **G5, drop-in.** heli takes the same arguments as cargo and nextest. When
  heli is absent or broken, plain cargo runs: a failure of heli itself never
  fails a run, except when the runtime/GC safety interlock cannot be acquired
  ([eviction](build-store.md#eviction)). That case refuses the managed command with a diagnostic: silently
  starting it unprotected could let another GC delete its runtime assets.
  An invalid explicit activity namespace is a configuration error. This is
  a deliberate safety exception to unconditional fallback.
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

## Shape

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

1. The [host pool](host-pool.md) serves G1 and G2, and covers bare cargo too.
2. [Runs](runs.md), the `cargo heli <cmd>` front end, serve G1, G6 and G7.
3. The [build store](build-store.md) serves G3 and G4.
4. The [result store](result-store.md) serves G6.

The mechanisms and contracts of each part:

- [The daemon](daemon.md): its state directory, endpoint, protocol, and how clients start it and hand it runs.
- [Host pool](host-pool.md): the jobserver, run pools and the broker, priorities, the memory governor, `setup`, `ensure`, `stop` and autostart.
- [Runs](runs.md): classes, tool profiles, the lifecycle of a run, where a run can wait, the command line, output, logs and reruns.
- [Build store](build-store.md): placement, accounting, eviction, recycling, dependency epochs and worktree Cargo homes.
- [Result store](result-store.md): what a run's key holds, and when a pass is reused.
- [Observability](observability.md): `status`, `doctor` and the event log.
- [Failure modes](failure-modes.md): what happens when a part fails, and how heli handles it.
- [Working with many agents](multi-agent.md): the practical guide.

## Upstream map

| Issue | heli's part | When it lands |
|---|---|---|
| cargo#5931, per-user artifact cache | [recycling](build-store.md#recycling) recycling, one owner at a time | heli's store can defer to it |
| cargo#16147, build-dir out of the workspace | [placement](build-store.md#placement) | heli stops setting it |
| cargo#14136, checksum freshness | [recycling](build-store.md#recycling) precise mode | Precise mode becomes cargo's; the path scan stays |
| cargo#4282, #17508, finer locking | Not solved: one build directory per worktree, and the pool makes waiting cheap | Concurrent runs in one worktree get faster |
| cargo#6747, jobserver token while waiting on a file lock | Irrelevant under the pool ([host pool](host-pool.md)) | none |
| cargo#13136, target GC | [eviction](build-store.md#eviction) | heli's eviction ladder can call it |
| cargo#14774, feature unification | Out of scope (cargo-hakari) | Fewer duplicate units for [eviction](build-store.md#eviction) |
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
greens. The [placement](build-store.md#placement) FLUI run uses stock Rust and establishes none of those
Headstart properties. heli does not enable its flags or ship compiler patches.

## Open questions

1. The default budget B (proposed: 150 GB) and floor F (proposed: 50 GB).
   Until this is settled, neither has a default, and nothing is evicted
   unless the configuration sets one ([eviction](build-store.md#eviction)).
2. Is a pool per logon session enough, or do agents run in other sessions
   (SSH, services)?
3. Should the default without a terminal be `compact` or `quiet`?
