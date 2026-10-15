# Concurrent agents on one host

Use one writable worktree per agent. Keep its default checkout-local `target/`;
do not export a single `CARGO_TARGET_DIR` shared by several worktrees. A shared
host jobserver cannot remove Cargo's exclusive artifact-directory lock.

Give intermediate artifacts a separate path per workspace. For a project on
Windows drive D, its `.cargo/config.toml` can contain:

```toml
[build]
build-dir = 'D:\heli-cache\{workspace-path-hash}'
```

The equivalent command-scoped environment variable is
`CARGO_BUILD_BUILD_DIR`, with both `BUILD` components. Do not globally override
projects that use different placement. Configure heli with an explicit store
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

A project's own machine-wide queue also needs integration. FLUI's corrected
xtask skips that queue when heli supplies a nonempty `HELI_RUN`, and leaves
standalone commands protected by their existing lock. Each other task runner
needs the same explicit integration review. Bare Cargo inheriting the host
jobserver does not automatically supply this runner handshake. Do not export
`HELI_RUN` manually or globally disable project locks.

GC preserves active Cargo directories, including locks in deeply nested
build-script caches. It can reclaim completed unknown nested Cargo artifacts
only inside a verified managed root, after the configured idle age and with
a recognized lock protocol. A budget can be temporarily exceeded while
artifacts are busy or young, or remain unmet when insufficient data is
eligible; inspect GC's victims and skipped entries. This is cache reclamation,
not a hard filesystem quota.

Cargo's shared package-cache resolver lock still exists, even offline. The
foreground cache-off path avoids extra result-cache resolutions; it does not
promise that Cargo itself never briefly waits. Do not give independent lock
files to multiple Cargo homes that symlink the same mutable registry cache.
An immutable shared vendor snapshot is a possible separate experiment.

See [the research](research/multi-project-resource-isolation.md) for pinned
primary sources and [the reproducible harness](../experiments/multi-project-spike.py)
for actual compiler-overlap, held-runner-lock and GC acceptance checks.
