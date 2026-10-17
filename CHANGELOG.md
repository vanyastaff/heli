# Changelog

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
