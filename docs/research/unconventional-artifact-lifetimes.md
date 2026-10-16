# Artifact generations, virtual filesystems and live garbage collection

Research date: 2026-10-15. Exploratory alternatives, not an accepted replacement
for `DESIGN.md`. Scope: keep arbitrary runtime reads of a run's `OUT_DIR` valid
while reclaiming **other completed runs**, including nested Cargo, on the
existing Windows/NTFS host. No production changes or clone benchmarks were made.

## Finding and recommendation

The useful next seam is **a lease on an artifact generation or declared bundle**,
not a faster deletion primitive. A running command owns a complete namespace
of potentially needed outputs until its relevant process lifetime ends. GC can
reclaim unrelated unleased generations concurrently. Copy-on-write cloning can
reduce generation creation cost; it does not establish liveness or make pinned
blocks reclaimable.

Start with a small NTFS experiment using ordinary directories and per-bundle
leases. Compare it with the current global activity guard. Only add clones if
the measured copy/rebuild cost warrants them. A CAS/virtual filesystem is a
larger possible second experiment, with explicit mediation and path contracts.

The boundary matters: arbitrary programs can open an arbitrary absolute path
later, without announcing that dependency. A run-scoped root protects arbitrary
`OUT_DIR` content **inside that enrolled root**; it cannot prove that an opaque
`exec`, escaped child or external tool will not read some other completed root.
Such consumers need declared bundles, complete filesystem mediation, or the
conservative global guard. Discovering only the files already opened is unsafe:
the regression deliberately opens its asset after GC.

## Existing evidence and landscape

The public regression
`tests/multi_project_gc.rs::gc_preserves_out_dir_assets_during_nested_cargo_test_runtime`
gates a real nested test after compilation, runs GC, then reads a 64 MiB
`OUT_DIR` file. The recorded RED run deleted that file. On Windows the test
uses Cargo's supported target runner to execute a copy outside the cache,
removing accidental protection from a mapped executable. Evidence is in the
local `target/audit-evidence/runtime-outdir-red.log`; it is disposable local
evidence, not a portable artifact promised by this note.

Cargo 1.99.0 at commit `5f94df4789f005f9a352888e8355ffc645b7ed0e`
consumes `BuildRunner`, returning its `Compilation`; its separate `files`
field owns the layouts and their locks. Tests execute after compilation returns.
Therefore build-directory lock lifetime is shorter than test-output lifetime.
[BuildRunner ownership and return](https://github.com/rust-lang/cargo/blob/5f94df4789f005f9a352888e8355ffc645b7ed0e/src/compiler/build_runner/mod.rs#L35),
[layout ownership](https://github.com/rust-lang/cargo/blob/5f94df4789f005f9a352888e8355ffc645b7ed0e/src/compiler/build_runner/compilation_files.rs#L116),
[test execution](https://github.com/rust-lang/cargo/blob/5f94df4789f005f9a352888e8355ffc645b7ed0e/src/ops/cargo_test.rs#L62).

At inspected heli commit `b3c68ee64de8e6a6c3ffd36834e98cfdfac852c5`,
`src/store/lease.rs` uses one per-user activity interlock. Commands share it;
GC needs its exclusive side for rename. `src/store/runner.rs` and
`src/daemon/execute.rs` extend lifetime beyond the daemon's own handle.
This is a conservative safety baseline, but continuously active unrelated
commands can prevent reclamation of completed directories.

The actual workspace volume was checked read-only using
`[System.IO.DriveInfo]::new('E:\').DriveFormat`: **NTFS**. ReFS/Dev Drive
capabilities must not be assumed for E:. No filesystem was reformatted or mounted.

## Candidate mechanisms and what they actually buy

| Mechanism | Useful property | Remaining obligation |
|---|---|---|
| Per-generation directory and lease | Separates active namespaces from completed victims | Enroll every relevant output/dependency root and preserve paths until final consumer exit |
| Reflink/clone generation | Independent writes with initially shared blocks | Clone a consistent tree, preserve Cargo semantics, count retained shared extents |
| Immutable CAS with writable views | Deduplicates immutable content and explicitly pins bundles | Capture all dependencies; break sharing before writes; retain runtime path view |
| Overlay/private mount namespace | Presents an isolated writable view over immutable lower content | Platform setup, stable path mapping, provider/mount lifetime and lower-layer pinning |
| ProjFS/BuildXL-style mediation | Can observe/project accesses and make content available on demand | Full API/process coverage, backing retention, crash behavior and substantial compatibility work |

### Per-run generations and bundle leases

An experimental layout could be
`<store>/generations/<generation-id>/<workspace-path-hash>/...`. The generation
exists **before compilation**, and nested Cargo normally inherits a build-dir
template inside the same generation. Its lease covers the entire root, not a
guessed list of `OUT_DIR` filenames. Multiple consumers may hold shared leases
on the same immutable generation; producing a new generation gets a writable
view of its own.

This directly handles a runtime that waits an hour before opening a generated
file. It also lets GC take a completed generation B while generation A is live.
It does not justify deleting unused-looking files inside A. Do not infer a full
bundle from dep-info: runtime directory enumeration, generated filenames,
plugins, external processes and arbitrary filesystem reads exceed that inventory.

For an existing mutable build directory, a **per-root** lease is a smaller
first step than per-run cloning. It can collect B during A without introducing
a new path on every run. It cannot provide two immutable historical runtime
views of one root while a new producer mutates that root. That is a separate,
observable reason to introduce generations rather than calling every lease a
snapshot.

An explicit `--build-dir`, `--target-dir`, wrapper or nested tool can escape the
inherited template. A design must either enroll that path before use, mediate
the operation, or fall back conservatively. Merely setting an environment
variable does not make the namespace exhaustive.

### Reflinks on supported filesystems

**Windows.** Microsoft's feature comparison marks block cloning available on
ReFS and unavailable on NTFS. Dev Drive is the relevant Windows developer
option, rather than an assumed property of an ordinary NTFS directory.
[ReFS/NTFS comparison](https://learn.microsoft.com/en-us/windows-server/storage/refs/refs-overview#feature-comparison),
[Dev Drive block cloning](https://learn.microsoft.com/en-us/windows/dev-drive#block-cloning-support).

ReFS shares clusters and separates later writes. Its clone operation requires
same-volume files, cluster-aligned ranges, compatible integrity/sparse settings
and an already-sized destination. Each cloned range must be smaller than 4 GiB;
this is not a maximum file size, because larger files can use multiple calls.
Capability detection can use `FILE_SUPPORTS_BLOCK_REFCOUNTING`, followed by
actual operation error handling. A metadata-only clone still needs space for
later copy-on-write allocation.
[ReFS mechanics and restrictions](https://learn.microsoft.com/en-us/windows-server/storage/refs/block-cloning),
[volume capability flag](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getvolumeinformationw).

**macOS.** APFS `clonefile` produces independent files sharing unmodified
storage, on the same filesystem. The destination must not already exist;
unsupported/cross-filesystem operations can fail. Later writes can fail with
`ENOSPC`: the clone did not reserve their future blocks. Apple's manpage
discourages using `clonefile` directly for directory trees and points to
`copyfile`. Tree-copy metadata, symlinks and error recovery still need testing.
[Apple-owned clonefile manual](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/clonefile.2).

**Linux.** `FICLONE` clones a whole file; `FICLONERANGE` shares selected ranges
on a reflink-capable filesystem, including suitable Btrfs/XFS configurations.
The files must be on the same mounted filesystem. Writes remain private and
the individual cloning operation is atomic against concurrent writes. A loop
cloning a directory is not thereby an atomic snapshot of that tree. Future
COW writes need free space.
[Linux FICLONE interface](https://man7.org/linux/man-pages/man2/FICLONE.2const.html).

Btrfs subvolume snapshots can supply a tree-level generation boundary, but
nested subvolumes are excluded from a parent snapshot. This matters if generated
projects or tools create their own nested subvolumes. Deletion removes the name
before background block cleanup completes.
[Btrfs snapshot and deletion semantics](https://btrfs.readthedocs.io/en/latest/btrfs-subvolume.html).

**Assessment:** use platform clone acceleration behind one semantics-preserving
interface with an ordinary-copy fallback. On this NTFS host that fallback is
the baseline cost. ReFS migration is an operational choice requiring separate
evidence, not a transparent library optimization.

### Immutable content plus writable overlays

An immutable CAS is attractive for shared third-party objects and repeated
completed generations. BuildXL already separates content storage from memoized
results and exposes session pins that keep needed blobs out of GC. Its local
implementation uses hard links where possible and Windows ACLs to prevent
content clobbering. This is evidence that content sharing and lifetime pins
are complementary mechanisms.
[BuildXL cache architecture and Pin contract](https://github.com/microsoft/BuildXL/blob/main/Public/Src/Cache/README.md).

A hard-linked writable Cargo tree is **not** a COW overlay. On NTFS writes and
attribute changes through any hard link affect the same file. Fingerprints,
incremental caches and tool-generated files cannot be treated as independent
mutable copies just because their paths differ. Immutable files need enforced
immutability, and a separate writable file before modifications.
[Microsoft hard-link semantics](https://learn.microsoft.com/en-us/windows/win32/fileio/hard-links-and-junctions).

Linux OverlayFS supplies copy-up for writes but changes inode/device behavior
depending on configuration. Modifying a mounted overlay's underlying trees is
unsupported. Consequently a CAS lower layer needs its own pin; deleting it
behind an active overlay is not safe GC. Whiteouts, rename, mmap, xattrs,
locking and native-library lookup belong in the Cargo compatibility experiment.
[Kernel OverlayFS semantics](https://www.kernel.org/doc/html/latest/filesystems/overlayfs.html).

### ProjFS and BuildXL-style filesystem mediation

ProjFS lets a provider project data into a filesystem namespace. Files can be
virtual, placeholders, hydrated, dirty or locally modified full files; deletion
can create a tombstone. This provides lazy materialization, not automatically
an immutable backing store, snapshot, or runtime reference tracker. A provider
that drops backing content before a delayed read has simply moved the same
bug behind a hydration callback.
[ProjFS overview](https://learn.microsoft.com/en-us/windows/win32/projfs/projected-file-system),
[cache states](https://learn.microsoft.com/en-us/windows/win32/projfs/cache-state).

BuildXL's sandbox demonstrates a more substantial alternative: Windows API
interception observes opens, reads, writes, probes and enumeration, including
process architecture transitions. The cited Linux implementation interposes
libc and explicitly cannot cover statically linked programs. Neither a partial
trace nor an observed successful past run proves that a future arbitrary runtime
will not read another file. Adopting mediation means owning its process-tree,
API and escape coverage, not importing a generic cache directory.
[BuildXL sandbox specification](https://github.com/microsoft/BuildXL/blob/main/Documentation/Specs/Sandboxing.md).

**Assessment:** a possible specialized backend after basic lifetime isolation
works. It adds a live I/O service and compatibility surface; unavailable provider
or missing pinned backing data becomes an application read failure. This is
an architectural experiment, not the first fix for heli's current GC seam.

## Paths and Cargo freshness constrain every alternative

Snapshots taken after compiling do not change an absolute `env!("OUT_DIR")`
already embedded in a binary. Moving that snapshot and setting a new runtime
environment variable cannot rewrite the compiled constant. A generation must
therefore be built at its final runtime-visible path, or use a proven per-process
virtual namespace preserving that path. A global junction/alias switched to a
new generation would redirect earlier processes too; it is not process isolation.

Warm cloning to a new path has the same issue as heli's current recycling:
path-sensitive units require invalidation. Preserve source bytes, mtimes,
fingerprints and lock semantics conservatively; re-run the existing stale-source,
filtered-source, moved-`OUT_DIR`, old-recipient and clean-build-equivalence tests.
An inode changing is not itself proof of Cargo staleness or freshness. The
runtime namespace and Cargo's actual freshness inputs must both agree.
The existing path scanner is in `src/store/scan.rs`; the contract and regression
fixtures are in `DESIGN.md` section 6.4 and `tests/recycle.rs`.

## Logical deletion is not physical reclamation

Removing a Unix name does not reclaim an open file until its last descriptor
closes. It also does not preserve later opens by the deleted pathname.
Windows deletion has handle-sharing and deletion-disposition rules; ordinary
DeleteFile can remain pending, while POSIX-style deletion removes the name
with handles still open. An executable mapping's incidental protection is not
a portable runtime-asset lifetime contract.
[Linux unlink](https://man7.org/linux/man-pages/man2/unlink.2.html),
[Windows DeleteFile](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-deletefilew).

The same distinction applies to clones and snapshots: deleting cache name B
does not release blocks that a live generation A still references. Btrfs exposes
total, exclusive and set-shared accounting for this reason. Summing file lengths
overstates physical usage with sharing, while treating every removed pathname's
length as freed overstates reclamation.
[Btrfs filesystem accounting](https://btrfs.readthedocs.io/en/latest/btrfs-filesystem.html#subcommand).

An implementation needs separate observations for logical namespace bytes,
live pinned content, estimated reclaimable exclusive blocks, pending deletion
and actual volume free space. On a filesystem without cheap accurate shared-block
accounting, report uncertainty and use measured free-space deltas for the floor;
do not claim a physical disk bound from logical counters.

**Capacity derivation:** if distinct live data requiring future reads, private
writable scratch and other applications' data already exceed available capacity,
no GC algorithm can both preserve all bytes and immediately restore a stricter
local disk bound. COW changes sharing and amplification, not this inequality.
Admission delay, explicit failure/cancellation, a larger device or verified
external backing is then necessary. Infinite retention plus unconditional
immediate admission plus a fixed disk ceiling cannot all be guaranteed.

## Race and crash contract for a bundle experiment

1. Create a generation under an owned managed root, bind its manifest to that
   root and identity, acquire the runtime lease, then spawn the first producer.
   No process may publish an output path before its bundle is protected.
2. A consumer attaches under the same per-bundle interlock used by GC's
   retire/rename transition. A manifest lookup followed by an unrelated lock
   acquisition leaves a deletion race. Use one stable lock identity outside
   the renamed tree and deterministic ordering for multiple bundle leases.
3. Keep a guardian/lease owner alive independently of daemon death. Closing a
   client connection, PID reuse or an expired wall-clock TTL does not prove
   that consumers ended. Define descendants and deliberate background processes
   explicitly; unknown surviving consumers keep the bundle conservatively live.
4. Seal a completed generation only after relevant writers stop. If cloning
   files individually, prevent source mutation across the whole traversal or
   validate/retry; per-file clone atomicity is insufficient for tree consistency.
5. GC excludes pinned bundles, atomically retires one unpinned identity, and
   deletes it afterwards. Partial clone, incomplete manifest, interrupted deletion
   or restart reconstruction must fail closed. A power-loss journal must distinguish
   unpublished scratch from a published possibly live generation.
6. Avoid a global admission/GC lock during recursive scans or copies. Any global
   coordination should protect short registry publication only. A run waiting
   for one bundle must not hold a lock that prevents unrelated bundle progress.

These are proposed acceptance conditions, not assertions that current heli
already implements per-generation isolation.

## Small falsifiable experiment

Use three tiny independent Cargo projects and 64 MiB **written, non-sparse**
pseudorandom runtime assets. First run on existing NTFS with ordinary copies;
later repeat on a separately available ReFS/APFS/reflink volume without changing
the NTFS result. Do not reformat user storage for this experiment.

1. A compiles into generation A, waits inside an actual runtime test, then opens
   its compiled-in `OUT_DIR` asset. B completes in a different generation.
   While A stays gated, GC must delete B and preserve every delayed read by A.
2. Start a late small check C. Its completion must not depend on opening A's
   gate. Measure admission latency separately from cloning/rebuild time.
3. Repeat with nested Cargo, explicit directory overrides, an opaque `exec`,
   and two consumers sharing one declared bundle. Unknown escapes must trigger
   an explicit conservative path, not silent unprotected execution.
4. Kill only the daemon, then repeat GC before releasing A. A must still read
   successfully; after its last relevant consumer ends, A becomes reclaimable.
   Also interrupt clone publication and GC rename/delete, then restart.
5. On clone-capable filesystems, modify one private output in A and verify B's
   bytes remain unchanged. Remove B while A shares its blocks, record actual free
   space, then remove A and record again. Failure to free B's logical size is an
   expected shared-block result, not a successful physical eviction of that size.
6. Preserve clean-build equivalence and compiled path assertions. Report number
   of rebuilt units, clone/copy bytes, peak physical allocation, pinned bytes,
   volume free-space deltas and all unsupported/fallback operations.

Reject the candidate if completed B cannot be reclaimed during A, A's delayed
read fails, C queues behind A, private writes leak between generations, a crash
permits premature deletion, or correctness depends on pretending logical unlink
immediately freed physical storage. Proceed to a larger workspace only if this
experiment passes and the measured NTFS copy/path-invalidation cost is acceptable.
