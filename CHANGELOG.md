# Changelog

## Unreleased

### Added

- **Superseded intermediates.** A `target/` directory built before
  `build-dir` moved its workspace's intermediates into the build store keeps
  the old `deps/`, `incremental/`, `build/` and `.fingerprint/`, which cargo
  no longer reads. `status` and `gc` report them per target as
  `superseded`, with a hint; they are the ladder's new first rung (tier 6),
  and `cargo heli gc --superseded` reclaims them now, whatever the budget.
  Final artifacts stay, and a directory cargo still writes to is not
  superseded (#92).
- **`--changed[=REF]` and `--affected[=REF]`** run a cargo subcommand on
  the workspace members changed since the merge base with `REF` (default:
  the upstream, else `origin/HEAD`), as a sorted `-p` list; `--affected`
  adds the members depending on them. A root manifest, `Cargo.lock`,
  `.cargo/` or `rust-toolchain*` change selects `--workspace`; an unchanged
  tree runs nothing. `cargo heli changed [--affected] [--base REF] [--json]`
  prints the selection and why (#90).

### Fixed

- **`cargo heli test` through a target's `runner`** (`wasm-bindgen-test-runner`
  for wasm) and for `harness = false` tests: heli passes the test threads in
  `RUST_TEST_THREADS` instead of adding `--test-threads` after `--`, and the
  test phase no longer passes on compact output's `--quiet`; such runners
  refused both (#98).

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
each in its own Git worktree. The contracts are in [DESIGN.md](DESIGN.md).

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
