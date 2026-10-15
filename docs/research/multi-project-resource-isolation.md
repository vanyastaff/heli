# Multi-project resource isolation: evidence and experiment criteria

Research date: 2026-10-15. Scope: concurrent Cargo projects and worktrees on
one host, including nested Cargo, with automatic reclamation. This document
supports the runtime experiment and records one confirmed defect reported by
its investigator; it does not claim final operational acceptance.
The authoritative behavior contract remains `DESIGN.md`, especially G1–G3,
the host pool (§4), build store (§6) and operational acceptance (§12).

## Conclusions

Keep the host pool and distinct build directories. A repository-wide queue
or a single shared target directory would reintroduce the contention the user
needs to remove. Separate three claims when evaluating heli:

1. Independent worktrees make progress concurrently.
2. Adaptive control preserves measured host memory/commit headroom for the
   tested workload.
3. Reclaimable, managed artifacts converge to the configured disk budget.

A successful sample establishes these properties for that workload, not a
universal RAM or disk bound for arbitrary programs. The current governor
withholds future tokens; it does not interrupt running allocations. The
eviction ladder protects busy, young and unknown directories. If those
protections leave insufficient victims, budget compliance is unmet.

## First priority: remove the actual serialization seams

The user's priority is concurrent useful progress across many projects, not
new hard RAM/CPU limits. Diagnose the place a run waits before changing pool
sizes. The practical order is:

1. Keep one writable checkout and Cargo build directory per agent worktree.
   Two commands using the same Cargo directory can still serialize inside
   Cargo; a host jobserver cannot remove that file lock.
2. Integrate project task runners with heli. The pinned FLUI host-lock module
   deliberately takes one per-user machine-wide lock across checkouts, and
   only documents its own `FLUI_XTASK_NO_LOCK` escape hatch and nested-lock
   marker. The inspected module has no `HELI_RUN` check. heli's adoption
   instructions in DESIGN §9 therefore describe a consumer integration that
   must be verified, not assumed to exist. This finding describes the earlier
   compatibility baseline; the follow-up consumer fix and held-lock experiment
   are recorded below.
   [FLUI host lock at the compatibility baseline](https://github.com/vanyastaff/flui/blob/b653a7ad8ddfb17c2f43b9cec8c3b9c56e5284a0/tools/xtask/src/host_lock.rs)
3. Define a runner handshake: a verified heli-managed invocation may skip
   that runner's redundant host lock and inherit the host jobserver; ordinary
   standalone invocations keep their existing behavior. `HELI_RUN` is a
   coordination marker, not proof that the daemon or jobserver is reachable.
   Regression cases must cover managed, standalone, nested and stale-marker
   invocations. Do not bypass every project's locks globally.
4. Add a lightweight late-arriving check while the longer task runner is
   compiling. A passing direct `cargo check` does not prove a wrapped
   `cargo xtask` avoids the project's host lock. Exercise the real entry point.
5. Include IDE Cargo as another consumer. rust-analyzer supports a separate
   target directory and check-command overrides; overrides must emit Cargo
   JSON, and build-script commands may require separate configuration.
   [rust-analyzer configuration](https://rust-analyzer.github.io/book/configuration)
   With an explicitly inherited build-dir, a different target-dir alone may
   still share intermediates: confirm both paths through Cargo metadata.
   Separate IDE placement trades contention for more artifacts, which must
   enter the same inventory and GC policy.

Disk accounting for nested projects is the next practical priority. Optional
compiler-cache reuse follows ownership correctness. The containment discussion
below records resource caveats and future alternatives; it is not a proposal
to delay the concurrency work for a hard-limit redesign.

## What the primary sources establish

### Jobserver slots and fairness

The GNU jobserver protocol gives each started command an implicit slot.
Additional parallel work borrows tokens, and only borrowed tokens must be
returned. This explains heli's documented `N + k` allowance for independently
started Cargo processes; a token count is not an accounting of every live
compiler. [GNU make: job slots](https://www.gnu.org/software/make/manual/html_node/Job-Slots.html)

Windows semaphore waiters have no FIFO ordering guarantee. Consequently,
semaphore sharing alone does not prove equal service. heli's run broker needs
its own observable fairness criteria; bare Cargo's implicit progress does not
prove fairness for additional slots. [Microsoft: semaphore objects](https://learn.microsoft.com/en-us/windows/win32/sync/semaphore-objects)

**Inference:** arbitrary numbers of independent runs cannot receive immediate
unrestricted first jobs while also satisfying an exact aggregate CPU/RAM cap.
Retain no head-of-line blocking as an operational latency target with declared
capacity assumptions. If strict containment is needed, expose a separate mode
whose overload policy may delay or fail work; do not silently change G1.

### Memory: adaptive control versus containment

Linux cgroup v2 provides hierarchical accounting and controls. `memory.high`
applies reclaim/throttling and can be exceeded; `memory.max` can trigger OOM
inside the cgroup and can briefly overshoot. `memory.events` and `memory.peak`
provide evidence; `memory.oom.group` controls group killing. Membership and
controller delegation must exist before relying on these controls.
[Linux kernel: cgroup v2](https://www.kernel.org/doc/html/latest/admin-guide/cgroup-v2.html)
systemd exposes these as `MemoryHigh` and `MemoryMax`, recommending the
former for normal control and the latter as a final defense.
[systemd resource control](https://www.freedesktop.org/software/systemd/man/latest/systemd.resource-control.html)

Windows Job Objects collect process trees and normally include descendants.
Nested jobs are supported, with breakaway rules to consider.
[Microsoft: Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects)
`JOB_OBJECT_LIMIT_JOB_MEMORY` limits aggregate committed virtual memory through
`JobMemoryLimit`; it is not a physical RAM or RSS cap. Per-process and job-wide
limits are independent.
[Microsoft: extended job limits](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-jobobject_extended_limit_information)

**Proposal requiring a separate spike:** keep cooperative throttling for normal
runs; optionally contain all managed runs in a host parent resource group, with
per-run children for attribution. Report physical headroom and commit
separately. Define behavior when a limit is hit: visible failed run, preserved
logs, no result-cache success, and unaffected sibling progress. Windows and
Linux metrics must retain their actual meanings rather than sharing an
incorrectly named RAM limit. This proposal is not implemented by this report.

### Build placement and cache scope

Cargo explicitly names `CARGO_BUILD_BUILD_DIR` as the environment variable
for `build.build-dir`. The doubled `BUILD` is correct. Templates include
`{workspace-path-hash}`, derived from the manifest path.
[Cargo configuration](https://doc.rust-lang.org/cargo/reference/config.html#buildbuild-dir)
Final artifacts and intermediates can be placed separately; intermediate
layout remains Cargo-internal and may change.
[Cargo build cache](https://doc.rust-lang.org/cargo/reference/build-cache.html)

Cargo's global automatic cleanup tracks registry/git downloads in Cargo home,
not build artifacts. It therefore does not replace heli's build-store GC.
[Cargo cache cleanup](https://doc.rust-lang.org/cargo/reference/config.html#cache)

**Nested ownership gap, reproduced through the public path:** the runtime
experiment ran `heli test -p nested` followed by GC with a 16 MiB budget and
`CARGO_INCREMENTAL=0`. A child Cargo wrote a 64 MiB `OUT_DIR` artifact. The outer
directory was evicted, but the child remained UNKNOWN at 70,169,873 bytes,
above the 16,777,216-byte budget. The child inherited the build-dir template,
while heli probed only the outer workspace; its non-incremental contents had
no eligible unknown-directory tier. These numbers are the reproducer result
reported by the runtime investigator, not throughput or memory measurements.

Safe design alternatives:

| Alternative | Benefit | Required proof / cost |
|---|---|---|
| Explicit child registration through a Cargo wrapper or integration API | Retains existing path layout and per-child identity | Preserve rustup/toolchain selection, user wrappers/config, arguments and exit status; durable registration before exit; nested recursion must terminate |
| Opt-in namespace owned by a managed run or worktree, with an active-run lease | Attributes nested hashes without reversing Cargo's hash | Reuse across runs must be designed; preserve explicit user placement; recursive lock checks and crash recovery before collection |
| Process observation and later metadata probes | Low integration burden | Best effort only: short-lived cargos and deleted temporary manifests can be missed; cannot prove complete ownership |
| Explicitly configured, newly created exclusive cache root | Makes nested hashes reclaimable without knowing individual workspace identities | Creation must prove namespace ownership; existing/default/unmarked roots retain conservative treatment; whole-unknown eviction requires recognized unlocked layout |

Do not claim a new hashed directory merely because it appeared during a run:
another agent may have created it. Never solve the gap by deleting unrelated
unknown directories. A scoped namespace is a candidate, not permission to
rewrite global Cargo configuration.

The exclusive-root alternative has now been implemented in
`src/store/managed.rs` for the reproduced case: heli creates the final root
using `create_dir`, and only a successful
new creation authorizes writing a versioned ownership marker before Cargo or
metadata probes start. `AlreadyExists`, even for an empty root, is not proof.
Failed marker creation leaves the root conservative; defaults are not adopted.
Marked unknown directories remain UNKNOWN and never become recycle donors or
orphans. They can become whole-directory tier-5 victims after the configured
idle age, only with a recognized, openable, unlocked Cargo layout. The existing
unknown-layout age fallback alone is insufficient for this new authority.

Before eviction, recheck marker/version, link-free containment and relevant
locks. Dry runs must not create or mark roots. Tests must cover preexisting
empty/nonempty unmarked roots, invalid/future markers, busy nested builds,
new-lock races, link escapes and restart persistence. Document new explicitly
configured roots as exclusively regenerable cache namespaces. This policy
still cannot promise immediate budget compliance while all eligible space is
busy or younger than the configured age; it removes the permanent unknown
directory gap rather than removing safety rules. The implementation has eight
public GC regression cases. A further reproducer found a lock deeper inside
the build directory: recursive lock discovery/retention was fixed, including
keeping the lock guards across Unix rename. The investigator reported the
focused cases green; final-suite and final-binary acceptance are tracked
separately below.

### Reuse without sharing a locked target directory

sccache can share cached compiler outputs across workspaces while worktrees
keep separate Cargo directories. Its Rust support requires incremental
compilation to be disabled; linker-invoking crate types are not cacheable, and
its documentation warns about proc macros reading files.
[sccache Rust caveats](https://github.com/mozilla/sccache/blob/main/docs/Rust.md)
Its path normalization setting `SCCACHE_BASEDIRS` is relevant to multiple
checkout roots. It also has jobserver integration that must be checked when
its persistent server starts under a different environment.
[sccache README](https://github.com/mozilla/sccache/blob/main/README.md)

**Proposal:** evaluate it as an optional reuse layer after correctness and
accounting pass. Count its own cache in the total host disk inventory. Measure
hits, misses, uncached reasons and total disk, rather than assuming it removes
per-worktree artifacts or enforces heli budgets. Do not turn it on silently.

### Headstart is complementary and currently adds resource risks

At pinned commit `ff81bfecda2116d54776a7fec538fc2f4e349d1f`, Headstart's own
readiness assessment identifies resumed compilations without tokens, resident
paused compilers without a count cap, extra CPU under saturation and untested
Windows behavior. These are upstream authors' findings, not measurements of
heli. Early metadata can improve dependency-chain scheduling but does not
provide host resource isolation.
[Headstart readiness](https://github.com/PowderworksCode/headstart/blob/ff81bfecda2116d54776a7fec538fc2f4e349d1f/docs/readiness.md)

**Decision for this investigation:** use its lesson—measure resident memory
separately from active jobs—but keep stock Cargo/rustc in heli acceptance.
A later optional toolchain experiment must cover wait/resume token accounting,
paused RSS, dependency failure/crash propagation and cache correctness before
recommending it for multi-agent workloads.

## Acceptance evidence to collect

- At least four overlapping runs spanning two repositories and two worktrees
  of one repository, mixing short checks, a longer gate and nested Cargo.
  Record submission, first compiler, completion, selected worktree/build path
  and run result. Prove actual overlap, not only overlapping client lifetimes.
- Sample host available RAM and Windows commit headroom; record process-tree
  memory, pool loans, withheld slots and wait times. Include a short late
  arrival while a long build is active and a pressure/recovery episode.
  A forced governor input tests policy, not a physical memory guarantee.
- Kill a borrowing run and verify token recovery, child cleanup and surviving
  sibling completion. Run failures must remain failures through result caching.
- Use one explicitly configured budget B and volume floor F. Inventory managed
  intermediates, registered targets, nested outputs, unknown directories and
  pending trash. Measure physical free space separately from logical size.
- Repeat generated-project/worktree creation and retirement. Check GC during
  builds and after idleness. Busy artifacts and unrelated unknown sentinels
  survive; eligible artifacts disappear; usage converges when eligible space
  suffices. An unmet budget must be reported as such, including blocked bytes.
- Counterfactual for a failure: same pinned input without heli or without the
  placement change, as appropriate. Preserve a minimal public-path reproducer
  and use it as the regression test before claiming a heli defect fixed.

No throughput or memory reduction number is asserted by this research. Such
numbers belong to the reproducible experiment and its recorded configuration.

## Retrieval coverage

Keenable successfully retrieved GNU, Microsoft, kernel and pinned Headstart
sources, then returned an organization request-rate error; no Keenable retries
were made. Firecrawl retrieved Headstart, Cargo, Microsoft and sccache sources
and its developer search corroborated Cargo's build-artifact GC gap. Its
targeted kernel extraction failed; official kernel documentation was read
through the browser fallback. A systemd manual fetch timed out in the browser,
but Firecrawl's targeted retrieval returned its memory-limit documentation.
No live systemd configuration or delegation capability was tested.

## Measured Windows experiment: progress and reclamation

Evidence reviewed: `target/mps-1015-2027/result.json` and
`target/mps-1015-2032/result.json`, together with their manifests. Both runs
used four detached worktrees: two for heli and two for FLUI. The workload was
heli check/test and `flui-foundation` check/lib-test, followed by a real xtask
host-lock control and three cycles of two generated nested Cargo tests. Each
child wrote a 512 MiB build-script artifact. Pool capacity was eight borrowed
slots, store budget 2 GiB, floor 50 GiB, and all GC ages were explicitly zero
to accelerate the reclamation experiment. Stock Cargo/rustc 1.99.0 were used.

| Observation | Earlier Refresh run | Cache-off run |
|---|---:|---:|
| Harness verdict | FAIL | PASS |
| All requested commands exit successfully | Yes | Yes |
| First actual compiler, heli check | 3.302 s | 0.822 s |
| First actual compiler, heli test | 3.287 s | 0.771 s |
| First actual compiler, FLUI test | 35.021 s | 1.020 s |
| First actual compiler, late FLUI check | 31.620 s | 1.096 s |
| Peak distinct real agents in compiler intervals | 2 | 4 |
| Cross-repository compiler overlap | No | Yes |
| Late check arrived during another compilation | Yes | Yes |
| Final accounted bytes after fresh post-GC survey | 1,668,322,106 | 1,668,322,106 |
| Configured budget bytes | 2,147,483,648 | 2,147,483,648 |

The cache-off run measured minimum available physical memory of
31,473,807,360 bytes and minimum available commit of 19,556,368,384 bytes.
Its free-space-floor and unmarked-unknown sentinel checks passed. These are
sampled headroom observations, not hard memory containment guarantees.
Compiler intervals are instrumented subprocess lifetimes, not CPU-utilization
samples. Both runs still logged package-cache waits; neither establishes that
all Cargo locks disappeared.

The cache-off manifest pins heli `971bdeddf1815827d44013da5abf953b38cae523`
and FLUI `1800e914fe1ac99a2d8d6e5bece824c9b18a1b1c`. Its measured heli binary
SHA-256 is
`59553a3e96f0bc35cab50f8d1a16a7f02a3770dc30691d7f339e7d130d32ffbd`.
The earlier run pins heli `929d318898d84f68cb89e95d12fbd481d0023554` and the
same FLUI commit. Distinct experiment roots, warmed OS caches and uncontrolled
background work by other agents prevent a causal throughput or speedup claim
from this pair. The observations justify the separate cache-off mode and
further startup investigation; they are not a controlled performance benchmark.

**Real consumer lock control:** the harness built FLUI xtask through heli,
then held an isolated FLUI host lock using the standalone Rust helper. Bare
`xtask clean-nested` remained blocked. Through `heli exec`, the same prebuilt
xtask completed successfully in 0.212 s while that lock remained held. After
release, the bare command completed successfully. Both used an empty owned
target path. This demonstrates the patched FLUI consumer handshake for that
entry point, without bypassing another user's lock or deleting existing
caches. It does not validate every project-specific task runner.

**Post-measurement repair:** reviewing
the new cache-off behavior found that identical verification/test requests
could incorrectly single-flight by command despite changed source inputs.
The public source-change reproducer failed before a registry guard was added;
the investigator then reported ten single-flight and 27 result-cache cases
green. Those later registry changes are absent from the cache-off binary
identified above. At the time this section was written, the complete suite
was running and a new frozen-binary experiment was planned. The earlier PASS
does not validate those later edits; the final repeat below does.

### Final frozen implementation

The repeat in `target/mps-1015-final` passed all harness checks on heli
`e04452be9a3c121ad4e498279484af84d997f8a5` and the same FLUI commit. Binary
SHA-256: `4a99f55370a8bf5050c9e1af9670f01313a2015a826b15a2cee24f2ce9b33576`.
This includes the distinct `Cache::Disabled` wire contract: an older daemon
must refuse the unsupported semantics before any command starts. A public
fallback test verifies exactly one direct execution and a retirement request.

| Agent | First compiler | Exit |
|---|---:|---:|
| heli check | 0.49 s | 0 |
| heli test | 0.48 s | 0 |
| FLUI test | 1.13 s | 0 |
| late FLUI check | 0.74 s | 0 |

Four distinct real agents had overlapping compiler subprocess intervals.
One independently checkable witness, in Unix epoch nanoseconds:

| Agent / run | Compiler starts | Compiler exits |
|---|---:|---:|
| heli-check / r1 | 1792097343118180500 | 1792097343712593100 |
| heli-test / r2 | 1792097343112002400 | 1792097343680238900 |
| flui-test / r3 | 1792097342989212300 | 1792097343651413000 |
| flui-check / r4 | 1792097343124547500 | 1792097343686117000 |

All four intervals include `[1792097343124547500, 1792097343651413000]`.
The real xtask held-lock probe completed in 0.153 s, with its bare negative
control still blocked. All six nested workloads exited zero. Final fresh
post-GC survey: 1,668,322,816 bytes against 2,147,483,648 bytes; the unrelated
unmarked sentinel survived. Minimum sampled available physical memory was
36,550,414,336 bytes, and commit headroom 28,031,430,656 bytes. These remain
observations rather than hard caps. The same zero-age and workload limitations
apply. The complete local heli suite passed 346 tests on this implementation,
with `cargo clippy --all-targets --locked -- -D warnings` also green.

The reproducible harness is `experiments/multi-project-spike.py`, with
`experiments/spike-rustc-wrapper.rs` recording compiler start/end events while
excluding compiler-information probes. It uses private daemon/config/pool
locations, preserves per-run logs and samples, verifies an unchanged binary,
and removes only its owned clean worktrees through Git. The experiment
establishes bounded convergence for this generated workload at zero retention
ages; normal retention defaults, other toolchains and Linux/macOS runtime
remain outside this evidence.

## Follow-up: shared Cargo-home startup contention

The runtime experiment observed repeated `Blocking waiting for file lock on
package cache` lines even with `--offline`. Other unrelated Cargo processes
were active on the host, so the log line alone does not identify the holder
or prove that heli introduced the wait.

Cargo 1.99.0's exact source commit from the experiment is
`5f94df4789f005f9a352888e8355ffc645b7ed0e`. Its lock implementation assigns
different descriptions to three locks: `package cache` is DownloadExclusive,
`shared package cache` is Shared, and `package cache mutation` is
MutateExclusive. DownloadExclusive serializes resolution/downloads while
permitting existing builds to read through Shared; it does not serialize the
whole compilation stage.
[Pinned cache-lock implementation](https://github.com/rust-lang/cargo/blob/5f94df4789f005f9a352888e8355ffc645b7ed0e/src/util/cache_lock.rs)

`resolve_with_previous` unconditionally obtains DownloadExclusive before
resolving, without an offline exception. Thus offline mode does not eliminate
this lock, and separate build directories do not eliminate a shared
Cargo-home resolver bottleneck.
[Pinned resolver](https://github.com/rust-lang/cargo/blob/5f94df4789f005f9a352888e8355ffc645b7ed0e/src/ops/resolve.rs)
The metadata command's `--no-deps` branch directly exports workspace members;
full metadata instead builds a resolve graph through `resolve_ws_with_opts`.
[Pinned metadata implementation](https://github.com/rust-lang/cargo/blob/5f94df4789f005f9a352888e8355ffc645b7ed0e/src/ops/cargo_metadata.rs)

**heli-specific hypothesis:** result-cache key computation adds resolver work.
`src/store/probe.rs::sources_at` requests full offline all-features metadata.
`src/daemon/results.rs::admitted` still computes it for `--no-cache`
(Cache::Refresh), and successful keyed runs repeat it in `after`. The ordinary
background workspace-learning probe uses `--no-deps` and takes the cheaper
metadata branch. This static call trace does not measure the contribution of
either probe to the observed waits.

Before changing production behavior, compare the same pinned command through
`cargo heli --no-cache check` and `cargo heli exec -- cargo check` (the latter
uses Cache::Off), plus direct Cargo, on the same dependency-cache state.
Record probe start/end and actual compiler start separately. A scoped Cargo
cache-lock trace or an isolated held `.package-cache` control can distinguish
resolver waits from artifact locks without touching the user's cache lock.
Do not remove full dependency inventories or replace them with `--no-deps`
when computing a successful verdict key. A bounded best-effort cache-key
attempt that explicitly declines caching when busy is a possible design,
requiring child-process cancellation and false-green regression tests.

Practical choices, in priority order:

1. Remove proven redundant metadata requests or keep optional cache work off
   the compilation startup path, while retaining complete validated inputs
   for every cached success. Preserve the distinction between bypassing a
   cache lookup and disabling cache recording.
2. Prefetch dependencies once and use locked/offline invocations when their
   semantics fit. This avoids network work during the exclusive phase; it
   does not remove the exclusive resolver phase or solve every toolchain.
3. If contention remains material, spike one shared immutable dependency
   snapshot plus small isolated Cargo homes for agents. Cargo's supported
   `vendor --sync` option can combine multiple workspaces into one dependency
   directory, and Cargo treats vendored sources as read-only.
   [Cargo vendor](https://doc.rust-lang.org/cargo/commands/cargo-vendor.html)
   This can separate lock files without copying all dependency sources per
   agent, but requires explicit source replacement and compatibility work for
   git dependencies, registry identities, configuration, toolchain selection,
   diagnostics paths, snapshot refresh and snapshot GC. It is an opt-in
   experiment, not an established heli fix.

Do not symlink a mutable registry/source cache into multiple Cargo homes with
independent lock files: that removes the coordination protecting the shared
contents. Do not delete a live lock file. Multi-toolchain behavior also needs
version-specific checking; the cited source notes older Cargo versions use a
different package-cache locking contract. There is no demonstrated supported
flag in this investigation that removes resolver serialization while retaining
one shared mutable Cargo home.
