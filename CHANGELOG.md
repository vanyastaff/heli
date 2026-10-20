# Changelog

## Unreleased

### Fixed

- **`--changed` and `--affected` take nextest's `-E`.** A filterset selects
  among the tests of the packages cargo builds, so beside the selection it
  filters within it, as it does beside `-p`; heli refused it as a package
  chooser (`--affected chooses the packages itself; it cannot be combined
  with -E`). `cargo heli --affected nextest run -E 'not test(slow)'` now
  runs the tests of the changed members and their dependents that the
  filterset keeps.

## 0.0.5

After upgrading, run `cargo heli restart` so that the daemon is the new one:
the eviction, recycling and trash fixes below are the daemon's.

### Fixed

- **Tools that rewrite the sources through another tool** take the worktree
  lock exclusively, as `cargo clippy --fix` and `cargo insta accept` do.
  `cargo hack clippy --fix`, `cargo hack fix`, `cargo ndk … clippy --fix`,
  `cargo ndk … fix` and `cargo insta test --accept` (also `--accept-unseen`
  and `--force-update-snapshots`) ran as verify or exec runs, under the
  shared side of the lock, beside builds in the same worktree.
  `cargo insta test --review` runs directly, as `insta review` does. And
  `cargo heli +nightly heli …` is refused as `cargo heli heli …` is, instead
  of running heli again down to the nesting limit (#115).
- **`setup` and `autostart` replace the user's files whole.** Shell
  profiles, editor settings, service definitions and the daemon's record
  were written in place: a crash, a kill or a full volume in the middle left
  the file truncated, and a profile kept as a link into a dotfiles
  repository was replaced by a copy. The content now goes to a temporary
  file beside the target and is renamed over it, keeping the target's
  permissions and following a link (#115).
- **Eviction measures a whole-directory victim again** just before moving
  it. The in-flight check used the survey's measure, taken when the pass
  was planned, minutes earlier on a large store, so what a run's nested
  cargo wrote there since went unseen (#115).
- **Recycling checks that a donor is still an orphan** just before claiming
  it. The orphans were decided when the store was scanned, and a worktree
  made back at a donor's place meanwhile would have had its build directory
  renamed away (#115).
- **Windows: the rename into the trash is tried again** when a handle is
  open inside the victim (a search indexer's, an antivirus scan of a fresh
  file, a `status` passing through), up to ten times 50 ms apart. One such
  handle made `gc` skip the victim and exit 0 (#110).

### Changed

- **Release binaries** are built with link-time optimization in one codegen
  unit. Symbols stay, so a panic's backtrace in a bug report names its
  functions (#115).
- **Documentation.** `DESIGN.md`, the brief heli was built from, is gone.
  Its contracts are now in `docs/`, one document per part, starting at
  [docs/architecture.md](docs/architecture.md); its dated measurements and
  acceptance runs are in
  [docs/research/acceptance-2026-10.md](docs/research/acceptance-2026-10.md)
  (#116).

## 0.0.4

After upgrading, run `cargo heli restart` so that the daemon is the new one:
runs of an older heli hold the host-wide runtime guard this version no
longer reads (#108).

### Added

- **Superseded intermediates.** A `target/` directory built before
  `build-dir` moved its workspace's intermediates into the build store keeps
  the old `deps/`, `incremental/`, `build/` and `.fingerprint/`, which cargo
  no longer reads. `status` and `gc` report them per target as
  `superseded`, with a hint; they are the ladder's new first rung (tier 6),
  and `cargo heli gc --superseded` reclaims them now, whatever the budget.
  Final artifacts stay, and a directory cargo still writes to is not
  superseded (#92).
- **Priorities.** `--priority interactive|normal|background` (or
  `HELI_PRIORITY`) weighs a run's share of the pool 4 / 1 / ¼: an interactive
  run beside two background ones gets 4/4.5 of the slots. Runs default to
  interactive on a terminal without `CLAUDECODE` or `CI`, else normal;
  cargo-mutants runs in the background. A run alone still gets the whole
  pool. `status` shows each run's priority (#93).
- **`cargo heli rerun [<run>] [--failed [--then-all]]`** runs a recorded run
  again, from its directory with the current environment; without a run,
  the latest test run with failed tests in this worktree. `--failed` reruns
  only the tests that failed, for nextest with an exact filterset and for
  libtest with the failed targets and `-- --exact` their names, and says
  when it has to run more; `--then-all` then runs the whole run once they
  pass. Each record now starts with the run's command line as typed (never
  its environment), and `status` and `log` show `rerun of r17` (#91).
- **`--changed[=REF]` and `--affected[=REF]`** run a cargo subcommand on
  the workspace members changed since the merge base with `REF` (default:
  the upstream, else `origin/HEAD`), as a sorted `-p` list; `--affected`
  adds the members depending on them. A root manifest, `Cargo.lock`,
  `.cargo/` or `rust-toolchain*` change selects `--workspace`; an unchanged
  tree runs nothing. `cargo heli changed [--affected] [--base REF] [--json]`
  prints the selection and why (#90).

### Fixed

- **Eviction on a busy host.** One host-wide runtime guard deferred all
  eviction while any run was in flight, which on a host of busy agents is
  always: the build store filled its volume. Runs now hold a lease of their
  worktree. Eviction leaves alone the directories of workspaces with a run
  in flight, and anything that changed since the oldest run in flight
  started (what a test's nested cargo built). It evicts older, idle
  directories meanwhile. A volume too full for a rename into the trash gets
  its victims deleted in place, and `doctor` warns about a store under its
  floor (#108).
- **`cargo heli test` through a target's `runner`** (`wasm-bindgen-test-runner`
  for wasm) and for `harness = false` tests: heli passes the test threads in
  `RUST_TEST_THREADS` instead of adding `--test-threads` after `--`, and the
  test phase no longer passes on compact output's `--quiet`; such runners
  refused both (#98).
- **heli's tests no longer build into the user's build store.** Their
  workspaces were in the system's temporary directory, under the user's
  home on Windows, so the user's `~/.cargo/config.toml` (and the Cargo home
  `CARGO_HOME` names) applied to every cargo they ran: a user-wide
  `build-dir` gave each one a directory in the real store. They now get an
  empty Cargo home and a directory with no Cargo configuration or git
  repository above it (`HELI_TEST_TMP` overrides it), and fail naming the
  file when there is none (#109).

## 0.0.3

### Added

- **`cargo heli stop` and `cargo heli restart`.** `stop` has the daemon take
  no new runs and exit once its runs end (`--now` cancels them, after a
  confirmation); `restart` then starts the current binary's daemon.
  `status` shows the daemon's version and program, and says when it is older
  than the client (#87).
- **The Windows user environment.** `setup --autostart` registers the
  daemon for `HELI_STATE_DIR` and `HELI_CONFIG` set there (with `setx`, as
  `doctor` advises for packaged apps); one set for a process only is still
  refused, and the message says so. `setup` offers to set `CARGO_MAKEFLAGS`
  there (#82).
- **`cargo heli doctor`** warns about a `CARGO_MAKEFLAGS` that names another
  jobserver than heli's pool, such as one written before the state
  directory moved, and gives the value to set (#86).

### Fixed

- **Build directories only bare or nested cargo built in** are attributed to
  their workspace from what cargo compiled, checked against cargo's
  `{workspace-path-hash}`. `status` and `gc` show the workspace, and one
  whose workspace is gone and has no repository (a test's temporary
  workspace) is evicted on the next pass over the budget, without waiting.
  `setup` explains what a user-wide `build-dir` does to nested cargo (#88).
- **Isolated Cargo homes:** `cargo install --list` lists the caller's
  installed tools, not the empty private home's, and `--help` or a lone
  `--version` of a refused command is no longer refused (#80).
- **`cargo heli doctor`** no longer lists as unknown a tool that has a
  profile now, and its `build-dir` hint is the line `setup --print` shows
  (#81).

## 0.0.2

### Changed

- **`cargo heli log`** prints a run as a person would have seen it. Cargo's
  JSON messages, which heli asks for in compact and quiet mode, become the
  diagnostics they carry, and `--tail` and `--grep` work on those lines.
  The new `--raw` prints the record byte for byte (#69).
- **Isolated Cargo homes** refuse `binstall`, `install-update` and
  `quickinstall`, which would install into the private home, as they refuse
  `cargo install`. `install --list` is allowed (#67).

### Fixed

- **Runs with an isolated Cargo home are keyed again** on workspaces with
  optional or other-platform dependencies, so the result cache and live
  sharing work with the configuration the README recommends. When a full
  offline resolution needs packages the home lacks, the manifests are the
  inventory (#66).
- **A `target/` directory several workspaces share** (a global
  `CARGO_TARGET_DIR`) is left alone by the build store: no eviction, and
  not counted toward the budget (#65).
- **`cargo heli doctor`** reports what heli does with nextest, llvm-cov and
  mutants, leaves out cargo-ndk's helper binaries, follows the configured
  Cargo home, and reports an unused build store and a fixed
  `RUST_TEST_THREADS` (#68).

### Added

- **Windows packaged apps.** `doctor` and `ensure` report when package
  virtualization (the Claude desktop app, packaged terminals) redirects
  heli's state and configuration, and how to fix it (#70).
- **Profiles for cross builds:** `cargo ndk` and `cargo zigbuild`, keyed by
  the NDK and zig versions (#63).
- **The compatibility matrix** checks that the pool bounds a real tool, and
  `insta review` on a real terminal (#64).

## 0.0.1

The first release. heli schedules cargo on a machine shared by many agents,
each in its own Git worktree. The contracts are in [the architecture docs](docs/architecture.md).

- **One host pool.** A per-user daemon owns a jobserver that every run draws
  from, so N agents never run N × cores jobs. Each run has its own run pool,
  and test runs get test slots. A memory governor withholds slots as memory
  runs low. `cargo heli setup` offers `CARGO_MAKEFLAGS` so that a bare cargo
  draws from the same pool, and `--autostart` registers the daemon at logon.
- **Run classes and the worktree lock.** Compile, verify, test, mutate and
  exec runs. Tools that rewrite sources take the worktree lock exclusively.
- **Result store and live sharing.** A verification or test that already
  passed on the same sources, toolchain, configuration and environment is
  not run again. Identical runs in flight are attached to, not repeated.
- **Build store.** Intermediates live in a store per volume (`build-dir`),
  with accounting, a budget and floor, an eviction ladder (`cargo heli gc`),
  and recycling of a removed worktree's build directory for a new one.
- **Cargo homes.** An optional private Cargo home per worktree, and opt-in
  immutable dependency epochs.
- **Output for agents.** Full, compact and quiet modes, run logs
  (`cargo heli log`), `status` and `cancel`.
- **Tool profiles.** Built-in profiles for popular cargo tools, with user
  profiles and a repository's `.config/heli.toml`. Interactive tools run
  directly in the pool, an sccache server outlives the run that started it,
  and `audit`/`deny` are cached against the advisory database heli fetches
  first. Scratch copies left by cargo-mutants are evicted first. cargo-sweep
  and cargo-clean-all never delete alongside heli's eviction.
- **`cargo heli doctor`** explains installed tools and Cargo configuration,
  and lists the tools without a profile that you run most.
- CI on Linux, macOS and Windows, a minimum supported Rust of 1.99, and a
  weekly compatibility matrix of popular cargo tools run through heli.
