# Runs: `cargo heli <subcommand> [args...]`

A run is one cargo or nextest command that the daemon executes on behalf of a
client.

## Classes

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

## Tool profiles

Beyond cargo's own subcommands, heli ships built-in profiles for popular
cargo tools (spec #38). They apply to everyone who installs heli, with no
configuration. Each is one declarative record in `TOOLS` (`src/class.rs`): its names, a summary for `doctor`, its options that take a value, and rules from a condition on the arguments (the first word, any word, or an argument) to a class or to a direct run. Adding a tool is adding a record. A tool that
is missing gets a profile in heli itself, so every user gets it, not only
one project. Users and repositories add profiles of their own (below).
Tools' caches are the build store's: cargo-mutants' scratch copies are the
ladder's tier 0, and cargo-llvm-cov's `target/llvm-cov-target` a built-in
tool cache ([eviction](build-store.md#eviction)).

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

## Lifecycle

1. **Key** (compile, verify and test runs). The key covers the worktree's tree,
   the normalized argv, the toolchain, the Cargo configuration and the
   environment; [result store](result-store.md) lists exactly what goes in and how.
2. **Result store.** On a hit, heli prints "already passed: <run>, <when>"
   and exits 0 ([result store](result-store.md)).
3. **Single-flight.** If a run with the same key is in flight anywhere on the
   machine, the client attaches to it (step 4, "Attach"): it streams that
   run's output and returns its exit code. The key leaves the worktree's
   path out, so the run may be another worktree's; the notice names its
   worktree, and says that `--no-cache` runs it anew.
   - The key is the one the result store's first lookup computed, so a
     miss costs no snapshot of its own.
   - A run takes the clients of its key from other worktrees only once it
     has the key it runs on. A run that waits at its worktree lock is keyed
     again after the wait ([result store](result-store.md)), since the runs it waits for (a `fmt`) may
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
     that runs. The runtime guardian ([eviction](build-store.md#eviction)) is the tree's first process;
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
5. **Compile phase.** The environment gets the run pool ([host pool](host-pool.md)) and
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
     pool count first; then it waits like any waiting run ([host pool](host-pool.md)), and the
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
   unchanged at the end ([result store](result-store.md)), and writes a source-state record for its build
   directory ([recycling](build-store.md#recycling)).
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
  `status` shows the wait ([observability](observability.md)).
- **Nested runs.** A run asked for from inside another heli run (its
  `HELI_DEPTH` is above 1) does not wait: the run around it may hold the
  lock, and `cargo heli exec -- cargo heli fmt` would then wait for itself
  forever. It still takes its turn, so later runs wait for it. Since only
  runs that are not nested wait, and only for runs admitted before them, no
  wait closes a cycle.

## Where a run can wait

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
   not be ([recycling](build-store.md#recycling)): for its `cargo metadata`, and with a donor for staging the
   worktree, a scan and a rename, and with a donor's source-state record
   for setting times in the worktree. At most 30 s; later runs from there
   never wait for it, and an orphan of another repository keeps no run
   waiting. While a store holds any orphan, the first run asks git for its
   repository before it starts.
5. **The source-state record:** as a run ends, its clients hear the end
   once the record of its build directory is written ([recycling](build-store.md#recycling)), at most 10 s
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

## Command line and contracts

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
cargo heli self-upgrade [--now [--yes]] [--to VERSION | --path DIR]
cargo heli daemon [--detach | --warden]            (internal)
cargo heli recycle                                 (internal: the post-checkout hook, [recycling](build-store.md#recycling))
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
    ([result store](result-store.md)) is stable, before any `--`. One line on stderr says what it chose:
    `cargo-heli: --affected since origin/main (3f2a1c9): -p a -p b (+2
    dependents)`. When nothing is selected, heli prints `nothing changed
    since <base>` and exits 0 without running anything.
  - It works with build, check, clippy, test, bench, doc, `nextest run`,
    `nextest list` and llvm-cov; heli refuses (exit 2) any other subcommand,
    and `-p`, `--package`, `--workspace`, `--all` or `--exclude` beside it.
    nextest's `-E` is not a chooser: a filterset selects among the tests of
    the packages cargo builds, so beside the selection it filters within it,
    as it does beside `-p` (`--affected nextest run -E 'not test(slow)'`).
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
    is not one (`--priority`, `HELI_PRIORITY`, [host pool](host-pool.md)). For `log`, also a
    run id that is not `r<number>` (or a bare number), a bad regular
    expression, or two selectors at once. A `--changed` or `--affected`
    heli refuses or cannot resolve (no git repository, no base).
  - 1 when the daemon is lost once a run's command has started ([daemon](daemon.md)), and
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
  - pool slots (`[pool] slots`, [host pool](host-pool.md));
  - memory watermarks (`[memory] low_watermark_mb`, `high_watermark_mb`,
    [host pool](host-pool.md));
  - the event log's size (`[event_log] max_kb`, [observability](observability.md));
  - disk budget and free-space floor (`[store] budget_mb` and `floor_mb`,
    in megabytes of 2^20 bytes; no default, [eviction](build-store.md#eviction));
  - the store per volume (`[store] paths`: absolute paths; on each volume
    the first one there serves it, [placement](build-store.md#placement));
  - tool-cache patterns (`[store] tool_caches`: paths below `target/`, `/`
    between components, `*` and `?` within one; [accounting](build-store.md#accounting), [eviction](build-store.md#eviction));
  - eviction ages, in hours (`[store] unit_hours`, default 24;
    `orphan_hours`, 24; `idle_hours`, 6; `dir_hours`, 168; [eviction](build-store.md#eviction));
  - the default output mode (`[output] mode`, "Output" below; the client
    reads it too, and ignores a file it cannot use);
  - log retention (`[log] runs`, the records kept, default 200, and
    `[log] bytes`, their total size in bytes, default 2 GiB; "Output"
    below).

  Debug builds also read a `[test]` table, the one seam tests have into the
  daemon: memory figures (`[test.memory]`) and a clock (`clock_ms`,
  milliseconds since 1970), read again at each look of the memory governor,
  which counts only the time between its readings, and of the eviction
  ladder, for which it is the time now ([eviction](build-store.md#eviction); `gc` reads it too). Release
  builds ignore it.

## Output (G7)

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
store keys it so ([result store](result-store.md)): a compact and a full run of one command have keys of
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

The run's class ("Classes" above) decides how warnings show: counted for
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

**Test runs.** Both phases of a test run (lifecycle step 6) write to one
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
for the result store ([result store](result-store.md)). Rendered compact output has no colours.

**Nothing is lost.** The daemon records each run's complete output, for
`cargo heli log` and so that clients can attach to it (lifecycle step 4).
The output modes only change what the client prints, and the footer names
the run. `cargo heli log <run>` prints the record by run id, without
re-running: stdout on stdout and stderr on stderr, as a person would have
seen the run. Every line is as recorded, except cargo's JSON messages, which
heli asks for in compact and quiet mode. Each diagnostic becomes its
rendering, on stderr where cargo prints it, once however many targets
reported it. Artifact, build-script and build-finished records are left
out (#69), and one line on stderr ends the view with what they said:
`cargo-heli: r17: no diagnostics; 15 artifacts (1 rebuilt); --raw for
cargo's messages`, the errors and warnings counted once each, the
artifacts with how many cargo rebuilt (`all fresh` when none), then `build
failed` when cargo reported a failure without a diagnostic, or `build not
finished` when it reported no end (a run in flight, or cancelled). So a run
that passed with nothing to say reads as what it did, not as nothing
(#123). At most one selector picks a part of it:

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
reruns: `status` (`rerun_of`, [observability](observability.md)) and `log` say `rerun of r17`. Without
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
