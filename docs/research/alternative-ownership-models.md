# Alternative ownership models for multi-agent Cargo

2026-10-15. Exploratory research and isolated experiments, not an accepted
replacement for the design (`docs/architecture.md`). The priority is independent useful progress across
projects and worktrees on one host; CPU/RAM caps are secondary. Both D: and E:
on the inspected host report NTFS. The starting production revision was
`b3c68ee64de8e6a6c3ffd36834e98cfdfac852c5`. Follow-up verification found and
fixed transitive Cargo configuration keying in `4c959b2`; the proposed
isolation mechanisms still change no default run policy.

## Direction to investigate first

Keep mutable coordination private and share immutable data. Give each agent
a small Cargo home and its own artifact namespace, publish shared dependency
snapshots, and lease only the artifact namespaces a run is allowed to use.
Add transparent compression for qualified cold files. A virtual content store
is a longer-term extension if those simpler mechanisms are insufficient.

This is an engineering recommendation for experiments, not a claim that these
pieces already form a production solution. The dependency and compression
mechanisms have small executed controls below; selective runtime GC and lazy
artifact views still need implementation and adversarial validation.

```text
dependency publisher -> immutable snapshot v1 <- agent A private Cargo home
                                             <- agent B private Cargo home
                    -> immutable snapshot v2 <- new requests after publication

agent A -> private artifact namespace A -> its runtime lease
agent B -> private artifact namespace B -> its runtime lease
completed, unleased namespaces -> compression / cold archive / GC
```

One host scheduler can continue distributing compile/test slots. Per-agent
mutable storage does not require independently managing the same host pool
through several uncoordinated daemons.

## 1. Immutable dependency snapshots: remove the shared mutation boundary

Cargo supports vendoring registry/git dependencies, source replacement, and
`--sync` for multiple workspaces. It treats the resulting sources as read-only.
These are usable building blocks for one versioned dependency snapshot shared
across projects, while every agent keeps private Cargo locks and bookkeeping.
[Cargo vendor](https://doc.rust-lang.org/cargo/commands/cargo-vendor.html).

The publisher alone fetches and assembles a new snapshot. It publishes only a
complete verified version, never edits a version leased by an active build,
and retires versions after their readers finish. Missing dependencies request
a new snapshot; they do not make already covered requests wait behind refresh.
Do not substitute different source identities or silently fall back to a
shared mutable registry when an input is unavailable.

**Executed evidence:** two small projects compiled their two registry
dependencies and local crates into fresh separate targets using one read-only
238,924-byte vendor directory and private 57,521-byte Cargo homes. A laboratory
default-home package-cache lock stayed exclusively held throughout. The
default-home control blocked; both private-home checks exited zero. Windows
kernel timestamps show 0.456237 seconds of overlapping Cargo process lifetime.
This is not independently measured overlap of the rustc jobs. A mutated-vendor
negative control failed its checksum check with exit 101.
[Commands, timing, boundaries and reproduction](dependency-epoch-spike.md).

The larger [real Heli/FLUI workload](real-dependency-epochs.md) records a
736-package shared snapshot, full Heli suite, selected FLUI checks, failures
found during integration, and the corrected run's acceptance evidence.

This establishes a mechanism for bypassing the demonstrated shared-home lock,
not a full-workspace performance claim. Full Heli/FLUI integration must preserve
toolchain selection, registry credentials/configuration, patches, git revisions,
Cargo extensions, nested invocations and complete result-cache input identity.
Read-only file attributes are an operational convention, not a security boundary.

A second executed mechanism avoids copying all packages into each new epoch:
publish package objects once, then make epoch directories contain NTFS junctions
to those objects. Cargo 1.99 accepted two such views with separate homes/targets.
Matching Windows file identities confirmed shared backing files. After retiring
only the first view's junctions, a fresh-target test through the second passed;
the original sources and copied objects retained their inventories. The payload
was just 238,924 bytes across two packages, not a whole-workspace validation.
This needs neither a custom virtual filesystem nor writable hardlinks.
[Package objects, measured view retirement and reproduction](package-epoch-views.md).

An epoch lease must pin the complete referenced object set. Retiring a view
does not authorize deleting its objects while another view or reader still
references them; atomic publication and object GC remain unimplemented here.

## 2. Artifact namespaces with complete lifetime leases

Allocate a run's artifact namespace before compilation. Its build-dir template
contains all enrolled nested Cargo outputs; target directories and any other
output roots are enrolled too. The run holds the namespace until its final
consumer exits. GC can remove completed namespace B while A is running.

This replaces the requirement for a globally idle host with a requirement for
complete ownership declarations. A lease on a stable per-worktree root is the
smaller starting point; distinct generations are needed when old runtime tests
must keep their historical outputs while a new producer changes the workspace.
The Nix store's GC roots illustrate reachability-based retention, but do not
provide a drop-in native Windows Cargo implementation.
[Nix GC roots](https://nix.dev/manual/nix/latest/package-management/garbage-collector-roots.html).

**Not yet implemented here.** The decisive test is a long test A that delays
opening its OUT_DIR asset, completed B reclaimed during A, and independent C
starting meanwhile, including killing only the daemon. Publish/lease/delete
races need a single protocol with a guardian retaining the lease.

An inherited environment variable alone does not prove closure: a child can
override build-dir, select another manifest, or open another absolute cache path.
Such paths must enroll before use, be mediated, or retain the conservative
global safety guard. Scanning current open handles or dep-info misses future
runtime opens. A global junction switched between generations is also unsafe;
each consumer needs a stable path view.
[Platform mechanisms, limitations and proposed test](unconventional-artifact-lifetimes.md).

## 3. Transparent compression on the existing NTFS volumes

Cold artifacts can retain their logical names and bytes while using less disk.
Windows exposes ordinary NTFS compression and WOF executable compression through
`compact`. Treat the two separately: write behavior, in-use files and physical
allocation differ.
[Microsoft compact documentation](https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/compact).

The isolated copy-only experiment compares both mechanisms on a real Heli EXE
and PDB, checks SHA-256, modification times and executable startup, and measures
storage using the Windows API rather than logical file length.
[Compression measurements and reproduction](ntfs-artifact-compression-spike.md).

For these two files together, reported storage was 70,875,648 bytes before,
32,186,368 bytes with ordinary NTFS compression (54.59% less), and 15,044,608
bytes with WOF LZX (78.77% less). Both formats preserved hashes, modification
times and executable startup. These are file-storage API measurements, not a
measured volume-free-space delta. Opening the WOF EXE copy for a same-byte write
expanded its reported storage from 2,789,376 to 10,086,912 bytes.

This is a candidate retention tier for qualified cold artifacts, not proof
that compression is safe or beneficial on every active Cargo output. WOF files
can expand when opened for writing; reserve space for that transition. Skip
busy or unsupported files, keep compression outside compilation's critical
path, and measure latency and total physical bytes on real caches before
enabling policy. Logical-byte accounting cannot establish the physical savings.

## 4. A target directory that materializes files on demand

Store immutable outputs in a content-addressed store; keep per-agent directory
manifests and writable views. Materialize only what a local consumer opens.
Buildbarn's bb_clientd demonstrates lazy CAS-backed files and a Bazel Output
Service over FUSE on Linux/NFSv4 on macOS. Buck2's deferred materialization
provides another precedent. Neither is a drop-in native Windows Cargo frontend.
[Buildbarn bb_clientd](https://github.com/buildbarn/bb-clientd),
[Buck2 deferred materialization](https://buck2.build/docs/users/advanced/deferred_materialization/).

This could reduce repeated cold artifact materialization across many worktrees,
but needs a real filesystem/IO integration, immutable objects, copy-up before
writes, and retention for later runtime opens. Replaying mutable Cargo targets
or hardlinking writable outputs into a shared object store is not an equivalent
implementation. Compiled absolute OUT_DIR paths, PDB/debug paths, build-script
hidden reads, absent-file probes, directory enumeration, mmap and provider
crashes are compatibility tests, not details to assume away.

## An additional independent idea: builds as revision queries

An agent could ask for diagnostics/tests for an immutable source revision rather
than leave obsolete command processes queued after successive edits. Requests
with identical validated inputs share work; explicitly superseded requests can
be withdrawn only when no consumer still needs them. This may avoid generating
obsolete artifacts in the first place. It requires stable input views, verdicts
tagged with their revision, and explicit supersession semantics for mutating
commands and gates. It is a proposal, not an implemented cancellation policy.
rust-analyzer's immutable Analysis snapshots are a useful conceptual precedent,
not a ready Cargo build snapshot mechanism.
[rust-analyzer architecture](https://rust-analyzer.github.io/book/contributing/architecture.html).

## Order of experiments and rejection criteria

1. Extend the demonstrated dependency snapshots to Heli and FLUI together,
   including proc macros, build scripts, nested Cargo, patches/git dependencies
   and toolchain changes. Deliberately hold a private default-home lock and
   publish a new snapshot during an existing build. Reject the design if an
   already covered agent must wait for publication or a changed identity is
   silently accepted.
2. Implement only the namespace/lease experiment first. Keep the global guard
   for unenrolled consumers. Reject selective GC if delayed OUT_DIR reads,
   daemon death, nested overrides or publication races can lose data.
3. Benchmark compression on an owned cold cache copy, including writes,
   timestamps, failed/busy files, physical growth and compile/startup latency.
   Reject broad policy if it changes content/freshness or shifts an unacceptable
   cost onto foreground work. Do not extrapolate two files to all FLUI caches.
4. Consider virtual CAS views only after measuring residual duplication and
   materialization cost. Maintain existing Cargo/test/IDE behavior and distinguish
   a functioning cross-platform backend from an interface-only prototype.

Compression, cloning and deduplication cannot reclaim unique bytes still needed
by active consumers. A budget smaller than the pinned live working set requires
more storage, spill/remote retrieval, waiting or refusal. Report live/pinned,
cold/reclaimable and physically free bytes separately. Independent progress and
correctness should not be replaced with an unmeasured promise of arbitrary
continuous workloads fitting a fixed disk.

Additional source comparison: [Cargo-native vs larger executor alternatives](unconventional-build-execution.md).
