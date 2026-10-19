# Package objects with junction-backed Cargo epoch views

Experiment date: 2026-10-15. Result: **PASS on native Windows/NTFS with Cargo
1.99.0**, for two tiny registry packages. Two Cargo directory-source views can
reference the same package objects through directory junctions. A controlled
follow-up retired the first view while a real fresh Cargo consumer of the
second view remained alive at a build-script gate; after release it read the
packages through that second view and completed its compilation and test.

This establishes a useful implementation option for immutable dependency
epochs without copying every package into every epoch or implementing a new
virtual filesystem. It does not establish production epoch/lease/GC correctness
or whole-project compatibility.

## Reproduce

From the heli repository, with Python 3.12+ and an installed rustup toolchain:

```powershell
python experiments/package-epoch-views.py `
  --vendor E:/heli/target/dependency-epoch-spike-20261015-2216/vendor-epoch-1 `
  --toolchain stable `
  --output E:/heli/target/package-epoch-view-NEW-ID
```

The vendor input is the completed earlier tiny dependency-epoch lab, containing
`itoa` 1.0.18 and `ryu` 1.0.23. Substitute another equivalent verified vendor
directory when reproducing elsewhere. The output must not already exist.
The harness creates two private Cargo homes, generated tiny projects, private
targets/build directories, its own object store and two junction views. Cargo
uses jobs=2, incremental=0 and offline/frozen commands. No original vendor
files, user configuration, user caches or active real-project epoch are modified.

For an independent regression without a preexisting vendor directory:

```powershell
python experiments/package-epoch-views.py --prepare-vendor --toolchain stable `
  --output target/package-epoch-view-NEW-ID
```

This explicit mode permits network access only for preparing the two pinned
dependencies. It generates a tiny publisher project and resolves/vendors
`itoa=1.0.18` and `ryu=1.0.23` in a new private publisher `CARGO_HOME` under the
new lab. Its Cargo commands and output are saved separately as
`publisher-lock.json` and `publisher-vendor.json`. The publisher is never
compiled. The consumers then use frozen/offline source views.

`--toolchain` defaults to `stable`, resolves installed Cargo/rustc executables
through rustup, and records their actual versions. Use `--toolchain 1.99.0` to
reproduce the original pinned-host experiment rather than assuming stable
will always mean that version.

Junction creation requires an execution context permitting reparse points.
The first ordinary sandbox invocation failed at `New-Item` with `Access is
denied`; a single authorized execution outside that filesystem restriction
completed successfully. The initial denial was not a Cargo failure or evidence
that Windows junctions are unavailable. Its evidence remains separately under
`target/package-epoch-view-1015-2230/`.

## Exact successful evidence

Evidence root: `target/package-epoch-view-1015-2232/`. Machine-readable commands,
outputs, source inventories and file identities are in `results.json`.

| Property | Observed result |
|---|---|
| Host | Windows 11 x86_64; E: reports NTFS through `GetVolumeInformationW` |
| Cargo | 1.99.0, commit `5f94df4789f005f9a352888e8355ffc645b7ed0e` |
| rustc | 1.99.0, commit `b940084d7eb6a299eb4bfeb8e34901bc051e7ac4` |
| Copied package payload | itoa: 52,409 bytes; ryu: 186,515 bytes; total 238,924 bytes |
| `generate-lockfile --offline` | Exit 0 separately in each private home |
| Two launched `check --frozen --jobs 2` clients | Both exit 0; both compiled the two dependencies and tiny consumer |
| Junction targets and file identities | Both views resolve to the same object paths; matching volume/file IDs through each view and object |
| Retire first view | Only its two verified junction entries removed; empty view root removed nonrecursively |
| Fresh-target test through second view after retirement | Exit 0; 1 test passed, no failed tests; fresh dependencies compiled |
| Original vendor and copied object inventories afterward | SHA-256/file-length inventories unchanged |
| Backing object deletion | Never attempted |

For `itoa/Cargo.toml`, epoch 1, epoch 2 and the object directory all returned
volume/device ID `11743212508721537441`, file ID `562949953590465`. For
`ryu/Cargo.toml`, the same three paths returned that volume/device ID and file
ID `562949953590491`. These identities prove shared backing files; summing
logical view lengths would double-count them. The experiment did not measure
allocation-unit overhead, all disk metadata or whole-host physical savings.

The successful layout before view retirement was:

```text
objects/
  b20a0ffe.../              # one copied itoa package
  e6b9a012.../              # one copied ryu package
epoch-1/
  itoa -> objects/b20a0ffe...  (junction)
  ryu  -> objects/e6b9a012...  (junction)
epoch-2/
  itoa -> objects/b20a0ffe...  (junction)
  ryu  -> objects/e6b9a012...  (junction)
home-1/ config.toml -> source replacement directory epoch-1
home-2/ config.toml -> source replacement directory epoch-2
```

Each package object was copied once into this lab, not reused through a
mutable user-registry cache. Copied source files had the Windows read-only
attribute set; this is an accidental-write guard, not an ACL or a security
boundary against another process owned by the user.

### Follow-up with a controlled active consumer

The original successful lab above remains intact as separate evidence.
The strengthened run is
`target/package-epoch-view-active-1015-2240/results.json`; process/gate states
are additionally preserved in `active-consumer-gate.json`. It also passed all
checks, with the same 238,924-byte package payload and unchanged original and
copied object inventories.

After both initial view checks ended, the harness generated a build script in
its own tiny project 2 and started `cargo test --frozen --jobs 2` with fresh
target/build directories. The build script signalled readiness and waited for
a release file. The harness observed Cargo PID `38300` still alive, readiness
from build-script PID `9336`, and no release file. It then removed only the two
verified junctions in the now-unused epoch 1 view and the empty view root.
Cargo 2 remained alive and unreleased after each junction removal and after
view retirement. Only then did the harness release the gate.

| Event | Recorded Unix epoch nanoseconds |
|---|---:|
| Readiness observed; Cargo alive, release absent | 1792104037853754300 |
| Epoch 1 retirement complete; Cargo still alive, release absent | 1792104037854417600 |
| Gate release initiated | 1792104037854429100 |
| Build script read both epoch 2 package manifests after release | 1792104037858030300 |
| Cargo completed, exit 0 | 1792104038352870000 |

The post-release read obtained 1,669 bytes from `epoch-2/itoa/Cargo.toml` and
2,412 bytes from `epoch-2/ryu/Cargo.toml`. The fresh Cargo invocation compiled
both dependencies and the consumer, then ran one passing test and no failing
tests. The gate controls the consumer build script; it does not assert that
each dependency compiler was waiting at that same instant or measure exact
CPU execution overlap. The demonstrated property is continued valid access
through epoch 2 across retirement of a separate unused view.

## Why Cargo accepts this, and what the source does not guarantee

**Verified source facts:** Cargo's pinned `DirectorySource` enumerates one
level of entries, skips dot names and entries without `Cargo.toml`, loads each
package through `PathSource`, and reads `.cargo-checksum.json`. Directory names
are otherwise unrestricted. Its verification method hashes listed package
files; its source cache invalidation method has no local source cache to clear.
[Cargo 1.99 directory-source implementation](https://github.com/rust-lang/cargo/blob/5f94df4789f005f9a352888e8355ffc645b7ed0e/src/sources/directory.rs)

**Empirical addition:** the tested Windows junction entries pass that loading
and checksum flow. This is stronger evidence than assuming every directory
enumeration automatically follows reparse points. It remains version/platform
evidence, not a published promise that all future Cargo versions must preserve
this particular materialization strategy.

Cargo documents directory-source checksum metadata and requires replacement
sources to preserve the original content. Checksums are not a security defense
against deliberate modification.
[Cargo source replacement](https://doc.rust-lang.org/cargo/reference/source-replacement.html)

## Protocol proposed by the original experiment

The following proposal describes the original experiment's next steps. The
explicit production integration below now implements its core ownership
protocol; the remaining compatibility and capacity work is still open.

1. Build a package object under a private staging name, validate its complete
   inventory and Cargo checksum metadata, then publish the immutable object.
   Content identity must not be confused with the original registry/archive
   checksum or source identity; retain all of them.
2. Stage a complete epoch manifest mapping source identity and package/version
   to objects. Materialize junctions and scoped Cargo source configuration,
   then publish the view atomically. Respect differing registry and git source
   identities rather than merging equal name/version pairs into one source.
3. Acquire an epoch lease before handing its path to Cargo. Nested Cargo and
   tools must inherit that lease's lifetime. An epoch lease pins its manifest
   and every referenced package object.
4. Retire an epoch only after its leases are gone. Delete the verified view
   entries themselves, without following junction targets. This lab used
   `os.rmdir` on individually verified junctions, followed by nonrecursive
   removal of the empty view directory; it never recursively deleted a view.
5. Reclaim an object only when no published epoch, staging operation or active
   lease references it. Publication, acquisition and reclamation must share an
   atomic protocol and survive daemon/publisher crashes. A directory's age or
   a single observed process PID cannot replace that protocol.

**Remaining acceptance work:** refresh epochs during active builds; fault
publication between every step; recover leases after crashes and PID reuse;
validate recursive child lifetime; reject junction escapes; test real native
dependency build scripts, proc macros, git sources, patches, IDEs and multiple
Cargo toolchains; account unique allocated bytes and materialization overhead.
The controlled consumer gate is not coverage of arbitrary build-script
behavior. Retiring one view while another Cargo remains active does not prove
safe deletion of their shared backing objects. No compiled artifact sharing
or Cargo freshness portability was tested.

## Automated experimental regression

The harness now creates Windows junctions or POSIX directory symlinks, verifies
the exact target before retirement, and removes only each link itself
(`rmdir` for a Windows junction, `unlink` for a POSIX symlink). It retains the
file-ID comparison, private homes/outputs, active Cargo build-script gate,
post-release package reads, fresh test and unchanged inventories.

`.github/workflows/dependency-epochs.yml` runs this small experiment separately
from the production suite on Windows, Linux and macOS, using stable Rust and
Python 3.13. It has read-only repository permissions, a ten-minute job timeout,
no shared cache/deployment changes, and always uploads JSON/stdout/stderr
evidence. It runs on explicit dispatch and relevant pull-request/main changes.
This automates the tested ownership mechanism; it does not enable dependency
epochs in heli or replace its production suite.

Native Windows retesting of the portable harness passed in both modes:

| Mode | Evidence root | Result |
|---|---|---|
| Existing vendor, offline consumers | `target/package-epoch-portable-1015-2248/` | PASS |
| New private publisher, fetch/vendor plus offline consumers | `target/package-epoch-fetch-1015-2248/` | PASS |
| Final harness, private publisher and strict post-release read chronology | `target/package-epoch-final-1015-2250/` | PASS |

These runs resolved `stable` to Cargo/rustc 1.99.0 on this host and passed all seven
acceptance checks. The fetch log shows only the pinned itoa/ryu packages
downloaded into its private publisher home. Local validation here covers native
Windows. The authoritative POSIX evidence is successful Linux/macOS CI jobs for
the reviewed commit; portable source code and a declared matrix alone are not
cross-platform evidence.

The narrow conclusion of the original experiment was that package-granular
shared immutable sources with cheap NTFS junction views are technically viable
on this host. The next section tests their production ownership and GC path.

## Explicit production integration

Heli now exposes `deps import`, `--dependency-epoch` on ordinary managed
commands, and `deps gc`; see [dependency epochs](../build-store.md#dependency-epochs).
The separate [production canary](../../experiments/dependency-epoch-concurrency.py)
uses those commands rather than constructing or retiring views itself:

```powershell
python experiments/dependency-epoch-concurrency.py `
  --tool E:/heli/target/debug/cargo-heli.exe `
  --vendor E:/prepared-vendor `
  --lab E:/heli/target/dependency-concurrency-NEW-ID
```

Supply an already built binary and a vendor directory containing exactly
`itoa` 1.0.18 and `ryu` 1.0.23, for example the `prepared-vendor` output of the
earlier harness's `--prepare-vendor` mode. The lab must be a fresh absolute
path. The canary records tool SHA-256, Cargo version/commit, Git version and
generated project commits, JSON results and command logs. It creates two clean
Git worktrees of one project and a separate second project, each with its own
writable home, target and intermediate directory, under one isolated daemon.

Native Windows runs at Heli `8fa21cd5e789f91881d3f23e7a8be5b06a952a9d`
passed both the initial and portable harness. Portable evidence root:
`target/dependency-integration/shared-worktrees-portable-8fa21cd/`.

| Observation | Result |
|---|---|
| Held original-home package-cache lock | Ordinary Cargo logs a lock wait and remains blocked throughout the managed workload |
| Three managed Cargo consumers | All enter their build-script gates before any gate is released; distinct private homes |
| Shared package object | Both epoch views expose the same itoa volume/file identity |
| GC with all consumers gated | Both epochs busy; no object removed |
| Release only consumer A | Its epoch is collected while B/C remain gated; their two package objects survive |
| Release B/C afterward | Delayed source reads succeed, both test commands exit zero |
| Final idle source GC | Unleased epochs and package objects are reclaimed |
| Original input and cleanup | Vendor byte inventory unchanged; clean worktrees retired through Git; only the isolated daemon stopped |

The unique package payload is 238,924 logical bytes. Each private home in the
initial run occupies 58,246 logical bytes. Summing view paths double-counts
shared packages; these are not physical-allocation or host-total measurements.
Private homes remain after source GC. This is a small production-path canary,
not a new full FLUI gate, arbitrary tool compatibility proof or disk/RAM bound.
Automatic publication/selection and capacity policy remain required work.
