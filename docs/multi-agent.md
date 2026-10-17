# Concurrent agents on one host

Use one worktree per group making independent source changes. Read-only roles
planning from the same checkout can share it and one test run. Keep the
default checkout-local `target/`;
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
assumptions. `cargo heli doctor` lists what heli does with each installed
tool; the tool profiles and the compatibility matrix are in DESIGN.md §5.

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
cargo heli build --locked
cargo heli exec --verify -- cargo xtask check-changed
```

`--cache off` keeps host scheduling, test phases and source-keyed live sharing,
while skipping saved-result lookup, recording and invalidation. Two identical
requests on unchanged inputs use one run; both clients receive its full output
and exit code. A source or configuration change starts a new run. A completed
run is not reused. Complete dependency/source inventory is required even with
the result cache off; a missing inventory disables sharing, not the command.
Use `--cache on` when saved verdict reuse is useful. Existing `--no-cache`
means refresh: it still computes a key and records a successful result.
It forces a new run rather than attaching. `exec --verify` is the explicit
read-only wrapper interface and defaults to live-only sharing. Ordinary exec
still attaches by command identity, which is unsuitable for asking whether
changed sources pass. Neither interface isolates files from concurrent edits.
Within one worktree, different keyed source states wait for earlier runs to
finish before rebuilding, then refresh their input key. This protects live
Windows test executables from linker overwrite errors. Different checks of
the same source state retain shared access; separate worktrees stay independent.

Builds use live sharing too, including with the default cache setting. Their
key includes the worktree and artifact locations: another worktree or target
directory gets its own build outputs. Changing sources starts a new build,
and no saved build verdict can stand in for missing artifacts. A live-only
client follows a cache-enabled run only once real execution has started,
so it cannot inherit a queued run's saved-result hit.

The [2026-10-16 acceptance report](research/shared-verification-acceptance.md)
records real Cargo same-worktree counters and the matched FLUI comparison
against both stock xtask and its lock-disabled control, including limits.

A project's own machine-wide queue also needs integration.
[FLUI PR #1683](https://github.com/vanyastaff/flui/pull/1683), merged on
2026-10-16, bypasses its host lock when heli supplies a nonempty `HELI_RUN`,
retaining standalone protection.
[FLUI PR #1684](https://github.com/vanyastaff/flui/pull/1684) also preserves
xtask's verified-result cache across managed run IDs. Check the consumer
revision before relying on that handshake. Bare Cargo inheriting the host
jobserver does not supply it. Do not export `HELI_RUN` manually or globally
disable project locks.

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

Cargo's shared package-cache resolver lock still exists, even offline. Source
inventory for live sharing can also wait there; cache-off does not
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
Earlier full workspace test attempts stopped on a consumer fixture that also
failed natively with a relocated target. After that fixture repair, the native
private-home repeat at Heli `c86d153` and FLUI `8c48ac7a3` passed seven of eight
commands: both fmt/clippy/check runs and A's full workspace test. B's test had
five failures across four consumer targets; independent native reproductions
identified test-fixture problems, rather than establishing a Heli cause.
[FLUI PR #1683](https://github.com/vanyastaff/flui/pull/1683) at `d85ab1e73`
contains repairs with targeted RED/GREEN evidence and a passing scoped gate.
The repaired repeat at that commit passes all eight commands, including both
full tests. Actual completed compiler invocations overlap across worktrees;
both worktrees remain clean and the recorded inputs stay unchanged. Idle
accounting is 4,771,735,920 bytes under an explicit 8-GiB artifact budget,
with no pending trash. Native source homes remain outside that budget. This
does not establish a full-suite speedup or universal absence of waits.
The dated [DESIGN acceptance record](../DESIGN.md#12-plan)
contains the scopes, timings and invalid GC setup finding.

There is also a real contention case: a Cargo download waiting on an HTTP
response can block another project's fully cached offline check through the
shared package-cache lock. The [local registry fixture](../experiments/real-download-hol.py)
compares that baseline with ordinary manually isolated Cargo and Heli's
dependency epoch. Both isolated cases finish while the download is waiting.
This proves the value of isolation for that case. Explicit epochs still need
preparation; the native private-home profile described above provides automatic
mutable-cache isolation without requiring an epoch or vendor snapshot.

See [the research](research/multi-project-resource-isolation.md) for pinned
primary sources and [the scoped spike](../experiments/multi-project-spike.py)
for actual compiler-overlap, held-runner-lock and GC acceptance checks.

To repeat the full eight-command FLUI workload, use
[the worktree acceptance driver](../experiments/flui-worktree-acceptance.py).
It requires Python 3.12+, Cargo/Rust 1.99, Git, a built Heli binary and an
explicit full FLUI commit already present in a local clone. The following
PowerShell example only prints the plan; choose a new short output path:

```powershell
python experiments/flui-worktree-acceptance.py `
  --heli C:/tools/cargo-heli.exe --flui D:/flui `
  --flui-ref d85ab1e73575388ba8112c1d86dbc7dd95491717 `
  --budget-mb 8192 --output E:/heli-lab/new-full-run --instrument
```

Add `--run` to create two detached worktrees and an isolated daemon, state,
native homes, activity namespace and managed artifact store. The driver
submits fmt, clippy, check and the full workspace test command concurrently
in both worktrees. It records compiler start/end intervals, command exits,
source/configuration digests, GC decisions and final accounting, and checks
that both worktrees remain clean at the pinned commit. It removes only its
still-clean worktrees at that commit after measurement; changed or
unreadable HEADs and dirty worktrees are retained. Logs and results remain.
A failed test or unmet artifact budget is reported as failure; a small budget is
not permission to remove protected runtime outputs. This public driver
accepts only a fresh lab. The recorded repeat that reused private native
homes has different cache warmth and does not establish a matched timing
comparison with a fresh replay. A cold lab downloads dependencies normally.

On Windows, while both full tests remain active, a second terminal can join
two small independent projects through
[the mixed-project sidecar](../experiments/flui-mixed-project-sidecar.py):

```powershell
python experiments/flui-mixed-project-sidecar.py `
  --live E:/heli-lab/new-full-run --output E:/heli-lab/new-sidecar
```

Again add `--run` to execute. This bounded std-only workload discovers and
checks the owned live daemon and pool, selects two more native homes, and
requires four active worktrees across three repositories. It invokes GC
while its tests hold build-script assets, then requires both projects to
finish successfully while the FLUI tests are still active. It preserves
its project sources and evidence, never stops the main daemon and cancels
only its own matching runs if necessary. The sidecar currently requires
Windows; it is a progress and runtime-safety check, not an authenticated
registry or throughput comparison. Start it after the full driver's setup
has completed and while both full test runs are still active.

The experiment CI checks syntax, five small driver safety regressions and
the full driver's prepare-only path on all three OSes. It does not launch
the heavy FLUI acceptance workload automatically.
