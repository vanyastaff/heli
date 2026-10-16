# Alternatives to coordinating mutable Cargo caches

Research date: 2026-10-15. Question: how can many agents work across many Rust
projects on one Windows host without interfering, while avoiding one growing
build tree per agent? CPU/RAM limits are secondary. This is research and a
proposed spike, not an implemented heli architecture or a performance result.

## Recommendation: immutable dependencies, private mutable execution

The strongest practical next experiment keeps Cargo but changes who owns its
mutable state. Publish a shared **immutable dependency epoch**, give each
agent a small private Cargo home and a private writable build arena, and share
only explicitly cacheable compiled outputs. Refresh dependencies and collect
retired storage concurrently with running agents. This tackles package-cache
locks by removing shared mutation, rather than adding another queue around it.

This recommendation is an engineering inference from the mechanisms below.
It has not been demonstrated by the existing multi-project spike. The existing
shared-store safety guard that defers GC while runs are active is a useful
correctness fallback, but a continuously busy agent host needs a finer ownership
model before it can reclaim storage without a global idle interval.

### Proposed data flow

1. A dependency publisher resolves/fetches dependencies in its own mutable
   Cargo home, stages a directory-source snapshot, verifies it, and publishes
   a new immutable epoch by atomic completion. Epoch identity includes source
   identities, versions/checksums, git revisions and generated replacement
   configuration. Missing content is never silently substituted.
2. Agents lease an already published epoch. Each gets its own Cargo home
   containing only scoped configuration and local bookkeeping/lock files.
   Cargo source replacement points at the same shared epoch. Each worktree
   keeps private targets/intermediates and temp directories. Nested cargos
   inherit the private home and the leased epoch.
3. A request requiring a new dependency asks for a new epoch. Other agents
   remain on the old epoch and keep compiling; only requests missing their
   required dependency wait. Publishing never modifies a leased epoch.
4. Start with qualified compiler-output reuse through sccache, with incremental
   compilation disabled where required. Do not replay entire Cargo target
   directories as if they were immutable action results.
5. GC leases dependency epochs, reusable objects and writable arenas separately.
   It can remove an unleased old epoch or retired arena while another arena
   builds. Publication, lease acquisition and deletion need one atomic protocol;
   a PID or recent timestamp alone is insufficient proof that data is unused.

**Supported Cargo building blocks:** `cargo vendor --sync` can gather multiple
workspaces into one output, emits source-replacement configuration, and Cargo
treats the resulting dependencies as read-only. Source replacement assumes
identical source content, and directory sources contain checksum metadata.
[Cargo vendor](https://doc.rust-lang.org/cargo/commands/cargo-vendor.html),
[source replacement](https://doc.rust-lang.org/cargo/reference/source-replacement.html)

**What remains a proposal:** epoch publication/leases, transparent scoped
configuration, automatic union refresh across many unrelated repositories,
and per-arena reclamation. Do not call these existing Cargo features.
The publisher can serialize dependency updates without serializing builds.
Separate homes remove contention on their own package-cache lock files; this
must be measured with actual Cargo and nested tools before claiming success.

### Problems that would invalidate a naive implementation

| Boundary | Required behavior |
|---|---|
| User Cargo configuration | Preserve effective registries, patches, credentials, target/linker/rustflags, credential providers and wrappers. Moving `CARGO_HOME` must not silently discard user semantics or copy secrets into shared epochs. |
| Toolchain selection | Preserve rustup overrides, `+toolchain`, `RUSTUP_HOME`, actual compiler binaries and required native SDKs. A dependency epoch is not a compiler/SDK snapshot. |
| Dependency changes | Existing epoch remains usable; a new dependency produces a new epoch or an explicit failure. No silent fallback to the shared mutable home. |
| Paths | Keep epoch roots stable. Absolute paths in generated Rust, link search directives, dep-info and debug info can prevent reuse across arenas. A renamed source tree does not prove artifact equivalence. |
| Nested Cargo and IDE | Both receive the intended home/epoch and their own writable output namespaces. Tools that reset environment or derive home directly need explicit integration. |
| Windows | Use short roots, avoid assuming reflinks or symlink privileges, handle open DLL/EXE deletion and junction escapes, and count pending physical bytes. No Linux namespace solution may be presented as native Windows validation. |
| Continuous activity | Reclaim unrelated retired arenas while a deliberately long build stays live. A global idle requirement fails this acceptance criterion. |
| Disk pressure | Immutable sources reduce dependency duplication; private compiler outputs still grow. Account for both, cache materialization and temporary publication overlap. When all space is leased, report an unmet budget instead of deleting live work or promising impossible convergence. |

Never implement private homes by symlinking a shared **mutable** registry into
them: independent lock files would no longer protect the shared data. Shared
immutable directory sources are a different ownership contract. Content-addressed
storage can deduplicate epoch files, but materialization and write protection
must preserve immutability; hardlinking writable compiler outputs is unsuitable.

## What changes when the build itself becomes an action graph?

### Bazel / rules_rust: real action reuse, substantial adoption cost

**Fact.** Bazel records action inputs, command lines and environment separately
from content-addressed output blobs. It supports a shared disk cache and cache
GC, including reuse across checkouts. Its output bases are distinct per
workspace by default and can be explicitly selected.
[Bazel caching](https://bazel.build/remote/caching),
[output layout](https://bazel.build/remote/output-directories)

**Fact.** rules_rust provides Cargo interoperability for build scripts, Cargo
environment variables and Cargo-defined lints. That is not a generic claim
that every arbitrary Cargo/xtask workflow executes unchanged under Bazel.
[rules_rust Cargo integration](https://bazelbuild.github.io/rules_rust/cargo.html)
Windows has documented path-length, symlink/runfiles and shell requirements.
[Bazel Windows guide](https://bazel.build/configure/windows)

**Inference.** This is a credible stronger alternative when a project accepts
an explicit build graph. It can cache independently executable units rather
than waiting for a whole Cargo command to finish. For heli's many existing
projects it is an adoption project: preserve feature resolution, build scripts,
proc macros, tests/doctests, native SDKs, rust-analyzer, nextest and project
commands. Separate output bases remain necessary for independent execution;
a shared action cache does not make every writable output directory shareable.

**Practical experiment:** migrate only one pure Rust library and one
build-script/native-library consumer. Compare resulting behavior and developer
commands on native Windows. Do not migrate FLUI wholesale to answer whether
private Cargo homes remove startup lock interference.

### BuildXL: observe dependencies instead of pretending to know them

**Fact.** BuildXL supports Windows and Linux and documents frontends for
MSBuild, JavaScript, Ninja and DScript; the README does not advertise a
drop-in Cargo frontend.
[BuildXL README](https://github.com/microsoft/BuildXL)
Its two-phase cache combines a static weak fingerprint with observed pathsets
and hashes to produce a strong fingerprint. Observations include absent-file
probes and directory enumerations, not just successfully read files. Its
Windows sandbox uses Detours-based interception to observe/enforce accesses.
[Two-phase lookup](https://github.com/microsoft/BuildXL/blob/main/Documentation/Wiki/Advanced-Features/Two-Phase-Cache-Lookup.md),
[sandbox implementation](https://github.com/microsoft/BuildXL/blob/main/Documentation/Specs/Sandboxing.md)

**Inference.** This is the most useful design lesson for a future heli action
cache involving arbitrary build scripts: cache a validated execution closure,
not just `argv + Cargo.lock`. It is also a major product: graph extraction,
observation completeness, toolchain/registry access, path mapping and replay
need native Windows proofs. Wrapping a whole Cargo invocation creates one
coarse cache action and retains Cargo's internal lock behavior; it does not
automatically produce per-crate scheduling or reuse.

### Nix: immutable dependency/build identity, not the native Windows answer

**Fact.** Nixpkgs `buildRustPackage` supports Cargo-oriented builds using
vendored dependency hashes or a Cargo lockfile; git dependencies can require
explicit output hashes. It separately documents building Rust crates without
Cargo. Nix's published supported platform list is Linux and macOS.
[Nixpkgs Rust integration](https://github.com/NixOS/nixpkgs/blob/6e9f5b900a06f1ad6cb63fda1ffc91fa386fa93a/doc/languages-frameworks/rust.section.md),
[Nix platforms](https://nixos.org/manual/nix/stable/installation/supported-platforms)

**Inference.** Borrow immutable dependency identity, publication and reachability
GC. Full adoption is plausible for controlled Linux/macOS builds but does not
prove native Windows/MSVC/desktop behavior. WSL can be an explicit separate
environment, not a substitute for the user's requested Windows evidence.

### Shake: explicit dynamic dependencies, not automatic Cargo decomposition

**Fact.** Shake rules declare dependencies with `need` and can discover further
dependencies during execution, including compiler dep-info. Its manual gives
cross-platform filename handling and command execution examples.
[Shake manual](https://shakebuild.com/manual)

**Inference.** It is useful for coordinating independent coarse tasks and
generated outputs. Running `cargo check` as one Shake rule leaves the shared
Cargo-home lock and private-output problem unchanged. Reimplementing Cargo's
crate graph inside Shake inherits the same correctness burden as a new build
engine; it is not a low-cost concurrency fix.

## Why a generic Cargo CAS cannot be keyed only by source and flags

Cargo's intermediate layout is internal, and dep-info paths are absolute by
default. Build scripts communicate ordered linker/configuration directives,
depend on environment and host/target distinctions, and may reuse persistent
`OUT_DIR` contents. These are actual supported semantics, not incidental noise.
[Cargo build cache](https://doc.rust-lang.org/cargo/reference/build-cache.html),
[build scripts](https://doc.rust-lang.org/cargo/reference/build-scripts.html)

Consequently, restoring `.fingerprint/`, dep-info and `build/` by content alone
can make Cargo accept artifacts produced for a different input closure or
path. An external file appearing where a build script previously found none
can also change behavior. An arbitrary test may depend on a server, clock,
device or filesystem outside the repository. Reusable compiler outputs and
reusable test verdicts therefore need separate contracts.

sccache already supplies a narrower reuse mechanism, with explicit caveats:
incremental compilation must be disabled, linker-invoking Rust crates are not
cached, and file-reading proc macros can be problematic. Treat it as a bounded
optional cache, not a universal correct Cargo action store.
[sccache Rust limitations](https://github.com/mozilla/sccache/blob/7bdb9bdcde435fd365e291715a6003f3ca28c2f9/docs/Rust.md)

## A focused spike that can reject the recommended architecture

1. Pin two real projects/toolchains, create two worktrees each, publish one
   union dependency epoch with no global config edits, and give each agent
   its own home and arena. Compare against the existing shared-home harness;
   do not mix this experiment with a compiler/toolchain upgrade.
2. Hold one private home's package-cache lock deliberately. The other three
   agents must reach actual compilers and finish. The blocked agent must be
   attributed correctly; tests that only inspect overlapping client lifetimes
   are insufficient.
3. While those builds run, request a new dependency and publish the next epoch.
   Existing builds remain valid. Kill the publisher during staging: no agent
   may observe a partial epoch or obtain a false cache hit.
4. Exercise registry and git dependencies, `[patch]`, path dependencies,
   environment-reading and file-probing build scripts, proc macros, generated
   nested projects, rust-analyzer and a native Windows linker consumer. Compare
   diagnostics, exit status and output behavior to ordinary Cargo.
5. Keep one long-lived arena active while repeatedly retiring other arenas.
   GC must reclaim their eligible bytes without a global pause. Crash and
   restart the daemon to check lease recovery. Never infer liveness from a PID
   without handling reuse and process-tree lifetime.
6. Record physical disk consumption of epochs, private homes, output arenas,
   reusable cache and trash. Measure publisher refresh time, first compiler,
   completed useful work, cache-hit reasons and evicted bytes. Reject the
   architecture if private-home copying or retained epochs simply move the
   growth elsewhere.

The winning first milestone is independent progress with shared immutable
dependencies and concurrent safe retirement. Fine-grained CAS execution is a
later choice justified by measured remaining duplication, not a prerequisite.

## Retrieval notes

Primary documentation/source was retrieved through Firecrawl. Keenable had
already returned an organization rate limit earlier in this investigation and
was not retried. Failed guessed documentation URLs were replaced by project
documentation indexes or verified source paths. No external build system was
installed or benchmarked, and no performance claim above comes from a heli
experiment of these alternatives.
