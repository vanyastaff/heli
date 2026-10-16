# Concurrent agents on one host

Use one writable worktree per agent. Keep its default checkout-local `target/`;
do not export a single `CARGO_TARGET_DIR` shared by several worktrees. A shared
host jobserver cannot remove Cargo's exclusive artifact-directory lock.

The current priority is independent progress across projects and worktrees.
Disk and RAM caps are secondary; preserve runtime safety without treating
disk optimization as a demonstrated prerequisite for concurrency.

To automatically isolate Cargo's mutable dependency caches, select this once
in heli's user configuration (`%APPDATA%\heli\config.toml` on Windows,
`$XDG_CONFIG_HOME/heli/config.toml` or `~/.config/heli/config.toml` on Unix):

```toml
[cargo]
home = "isolated"
```

Each worktree then gets a persistent private Cargo home when a managed
command starts. No vendor, import or explicit fetch preparation is required:
Cargo downloads and resolves dependencies itself. For a command-scoped
choice, use `cargo heli --cargo-home isolated --cache off check --locked`.
The default is `shared`; an outer `--cargo-home shared` keeps the caller's
original home. An explicit epoch selection still uses its isolated profile.

This trades duplicate downloads and storage for independence. A cold
isolated home cannot build offline just because another home has the
dependencies; run the ordinary online check first, then subsequent offline
commands can reuse that worktree's cache. Heli never retries through the
shared home after an isolated failure. Keep native `--locked`/`--offline`
flags as needed; Heli does not insert a lockfile-generation step.

Nested Cargo and nested Heli retain the parent's chosen home, including
generated projects with additional dependencies. Credentials/configuration
changes select a new home generation for a new outer invocation, while live
nested consumers retain the existing generation. Homes are protected and
retained; their bytes are outside the artifact GC budget. Original home
`config.toml` keeps its relative path origins; legacy home `config` is
refused rather than silently rebased. Cargo 1.99+ is required.

Native homes use a compact directory under `<state>/h/` so dependencies with
deep C source trees remain usable by Windows native compilers. Full home
identities are verified independently of the shortened directory names;
an ownership mismatch is an error. Keep a custom `HELI_STATE_DIR` short on
Windows: native tools can still reject sufficiently long paths. Existing
inherited homes keep their paths until their commands finish.

Perform Cargo-home maintenance such as install/login/logout from an outer
shared command. The isolated profile refuses known home-maintenance
commands and refuses an explicit switch to shared inside an already
isolated invocation. Arbitrary external tools can have their own Cargo-home
assumptions; the full tool compatibility work remains open in #38.

Give intermediate artifacts a separate path per workspace. For a project on
Windows drive D, its `.cargo/config.toml` can contain:

```toml
[build]
build-dir = 'D:\heli-cache\{workspace-path-hash}'
```

The equivalent command-scoped environment variable is
`CARGO_BUILD_BUILD_DIR`, with both `BUILD` components. Do not globally override
projects that use different placement. Keep the `{workspace-path-hash}`
template: managed build directories have the layout
`<store>/<2 hex digits>/<hex digits>`. An arbitrary `<store>/agent-a` path
is not enrolled in that store's inventory or GC.
Configure heli with an explicit store
on each relevant volume and one aggregate artifact budget, for example:

```toml
[store]
paths = ['D:\heli-cache', 'E:\heli-cache']
budget_mb = 51200
floor_mb = 51200
dir_hours = 1
```

These numbers are examples, not measured universal sizing requirements. Leave
new cache roots absent, then run `cargo heli ensure` with this configuration.
Only heli's successful creation of a new root establishes its exclusively
regenerable-cache provenance. Existing roots are not adopted. Inspect
`cargo heli gc --dry-run --json` before a real collection; each build directory
reports `managed_cache_root`.

For foreground checks and tests, use:

```text
cargo heli --cache off check --locked
cargo heli --cache off test --locked
cargo heli exec -- cargo xtask check-changed
```

`--cache off` keeps host scheduling and test phases, while skipping result
reuse, result recording and the full dependency/source inventory needed for
a safe cached verdict. It never attaches an unkeyed check to an earlier
check. Use `--cache on` when verdict reuse is useful. Existing `--no-cache`
means refresh: it still computes a key and records a successful result.

A project's own machine-wide queue also needs integration. As checked on
2026-10-16, FLUI main still has a common per-user host lock in
`tools/xtask/src/host_lock.rs`.
[FLUI PR #1683](https://github.com/vanyastaff/flui/pull/1683) adds a bypass
when heli supplies a nonempty `HELI_RUN`, retaining standalone protection;
the PR was open and unmerged. Check the consumer revision before relying
on that handshake. Bare Cargo inheriting the host jobserver does not supply
it. Do not export `HELI_RUN` manually or globally disable project locks.

GC protects the whole lifetime of managed commands, including runtime tests
reading `OUT_DIR` after Cargo releases its build locks. Compatible shared
activity guards let agents progress together; GC skips reclamation while any
managed command is active. A command parent keeps this protection alive even
if its daemon dies. This deliberately favors correct independent work over
evicting caches during a continuously busy workload. Reclamation resumes at
idle, so continuous managed activity can defer the disk target indefinitely.
When a daemon's own command releases its runtime guard, it requests another
GC pass without waiting for the periodic survey, including after `exec`.
Release by another daemon or a surviving command parent is still detected by
the periodic survey. Neither path relaxes the shared runtime safety guard.

GC also respects locks in deeply nested build-script caches. It can reclaim completed unknown nested Cargo artifacts
only inside a verified managed root, after the configured idle age and with
a recognized lock protocol. A budget can be temporarily exceeded while
artifacts are busy or young, or remain unmet when insufficient data is
eligible; inspect GC's victims and skipped entries. This is cache reclamation,
not a hard filesystem quota.

Use `heli exec` around task runners that start nested Cargo commands. Bare
Cargo's compile locks protect compilation; they cannot protect runtime assets
after Cargo releases those locks. The runtime guarantee covers managed
commands, not unintegrated bare tests or detached background descendants.
Keep all participants on the same heli version. Do not override
`HELI_ACTIVITY_DIR` for processes sharing caches: it selects a separate GC
activity namespace for isolated tests and experiments, independent of daemon
state directories and pools.

Cargo's shared package-cache resolver lock still exists, even offline. The
foreground cache-off path avoids extra result-cache resolutions; it does not
promise that Cargo itself never briefly waits. Do not give independent lock
files to multiple Cargo homes that symlink the same mutable registry cache.
Immutable shared vendor snapshots with private Cargo homes are available
through the explicit `deps import` / `--dependency-epoch` profile described in
[DESIGN §6.5](../DESIGN.md#65-immutable-dependency-epochs-opt-in-integration).
They are not selected automatically. The dependency store currently has
explicit GC rather than automatic capacity enforcement, and private homes
are retained. Its bytes are outside `[store] budget_mb`; enabling the profile
does not establish a total disk bound. Cargo 1.99+ and home `config.toml` are
required; legacy `config` and unsafe private homes are refused. Review the
documented tool/source-override limits before opting in. See the
[real Heli/FLUI workload and its verification limits](research/real-dependency-epochs.md)
and [package objects with cheap NTFS epoch views](research/package-epoch-views.md).
The [production dependency canary](../experiments/dependency-epoch-concurrency.py)
reproduces three concurrent consumers across two projects and two Git worktrees,
with a held baseline package-cache lock and source GC between consumer exits.

The 2026-10-16 FLUI control also established actual parallel compilation
with ordinary Cargo, including a shared lab Cargo home. Manually separating
artifacts and homes is not a benefit attributable to heli. The small matched
checks do not establish a general speedup or a full-suite comparison.
Both full workspace test attempts stopped on a consumer fixture that also
failed natively with a relocated target; the suite is not claimed green.
The dated [DESIGN acceptance record](../DESIGN.md#12-plan)
contains the scopes, timings and invalid GC setup finding.

There is also a real contention case: a Cargo download waiting on an HTTP
response can block another project's fully cached offline check through the
shared package-cache lock. The [local registry fixture](../experiments/real-download-hol.py)
compares that baseline with ordinary manually isolated Cargo and Heli's
dependency epoch. Both isolated cases finish while the download is waiting.
This proves the value of isolation for that case; automatic epoch setup is
still required to remove the manual preparation from the agent workflow.

See [the research](research/multi-project-resource-isolation.md) for pinned
primary sources and [the reproducible harness](../experiments/multi-project-spike.py)
for actual compiler-overlap, held-runner-lock and GC acceptance checks.
