# Build store

## Placement

Build placement is **opt-in per repository** (Q2, decided in #27). Enabling
heli's host pool does not imply moving every project's intermediates.
`cargo heli setup` offers `build.build-dir = "<store>/{workspace-path-hash}"`
for one repository. The separate user-wide offer remains an explicit override
for a user who has validated their other projects and tools; it is not the
recommended scope. Both file writes default to no. Verified: `build-dir`
supports the templates `{workspace-root}`,
`{cargo-cache-home}` and `{workspace-path-hash}`, the last being a hash of the
canonical manifest path (cargo `src/context/mod.rs`, `custom_build_dir`; Cargo
Book, `build.build-dir`).

The intermediates (`deps/`, `incremental/`, `.fingerprint/`, `build/`) leave
the worktree. `target/` keeps the final artifacts and tool caches: doc,
trybuild, and whatever a project's own tests build there.

`<store>` must be on the same volume as the worktrees. Cargo hardlinks final
artifacts out of the build directory, and across volumes it copies them
instead. The default is `<volume>/.heli/build`, one store per volume that
holds worktrees. On Unix the root of a volume is rarely writable, so there
the default is the user's cache, `$XDG_CACHE_HOME/heli/build` (else
`~/.cache/heli/build`), when it is on the worktree's volume, and
`<volume>/.heli/build` otherwise. A volume is known by its root:
`GetVolumePathNameW` on Windows, elsewhere the highest directory above the
path on the same device.

**Under the floor.** Once the volume of the store a run would build in
has less free than `[store] floor_mb`, the next build there fails for lack
of space (os error 112 on Windows), which costs more than the copies cargo
makes across volumes. So a managed run whose build directory would land
in such a store builds in the first configured `store.paths` entry whose
volume is above the floor, through `CARGO_BUILD_BUILD_DIR=<that
store>/{workspace-path-hash}`, which its nested cargos inherit; its
clients get `cargo-heli: note: the volume of <store> is under its floor
([store] floor_mb); building in <other> instead` (not with `--output
quiet`). It starts cold there, and goes back to its own volume's store
once that volume is above the floor again. heli asks cargo where the build
directory is only while some store's volume is under the floor; an
inherited `CARGO_BUILD_BUILD_DIR` and a `build-dir` outside every store
heli manages are kept (`src/store/placement.rs`, #135).

**Where setup writes it.** Each file is offered with its own question, as
for the pool's variable ([host pool](host-pool.md), "Reach"):

- **This repository**: `.cargo/config.toml` in the repository's main
  worktree (the one holding the git common directory). Cargo reads the
  `.cargo/` of every directory above the one it runs in, so this covers the
  main worktree and every worktree inside it (Claude Code puts them in
  `.claude/worktrees/`), and nothing else: not worktrees elsewhere, and not
  the nested cargos of tests that build projects in temporary directories.
  Its store is the one on that worktree's volume. If the file is tracked,
  the line is a local change, this machine's, not to be committed.
- **Every project**: the user's Cargo configuration (`config.toml` in
  `CARGO_HOME`, else in `~/.cargo`). One value serves every volume, so its
  store is the one on the volume setup runs in. Every cargo reads it,
  nested ones included. Accepting the repository offer does not authorize
  this write: it has its own question, default no.

setup adds two lines under the file's `[build]` table (or in a new one at
its end): a comment that marks them as heli's, and
`build-dir = "<store>/{workspace-path-hash}"`. It replaces lines an earlier
setup wrote, and leaves alone, with the reason, a file that is not TOML,
that sets `build-dir` itself, or whose new content would not parse to the
same configuration plus `build.build-dir`. Then, in a repository, setup
offers its git `post-checkout` hook, for recycling ([recycling](#recycling)).

For a run and its nested cargos, the process-scoped equivalent is
`CARGO_BUILD_BUILD_DIR="<store>/{workspace-path-hash}"`. The two `BUILD`s
are intentional: Cargo maps the `build.build-dir` configuration key to
that environment variable ([Cargo Book](https://doc.rust-lang.org/cargo/reference/config.html#buildbuild-dir)).
Unlike a repository configuration file, the inherited environment also
reaches generated workspaces in temporary directories. It does not change
the user's Cargo configuration.

**Risk: tools that look in `target/<profile>/deps`.** Once the intermediates
move, anything that searches `target/<profile>/deps` for rlibs, test binaries
or `.d` files stops finding them. In flui, the candidates are the hot-reload
fixtures, trybuild and the nested-cargo checks. Before phase 2, a real
workspace's full test suite (flui's) must pass with `build-dir` set ([acceptance runs](research/acceptance-2026-10.md#plan-and-acceptance-runs)).

This is the direction of cargo#16147. heli turns it on and manages what
accumulates.

## Accounting

Each build directory holds `.heli/owner`: the workspace path, the repository
id (its git common dir), the creation time and the last run. The daemon keeps
sizes by category (incremental, deps, build scripts, fingerprints, other), for
build directories and for registered `target/` directories. It rescans a
directory after cargo releases its lock there.

**Learning a workspace.** heli does not compute `{workspace-path-hash}`
(cargo's hash of the canonical manifest path, which changes with cargo's
hasher); cargo names the directories. A run whose program is named `cargo`
(a cargo subcommand, or an `exec` of cargo) starts, beside it in the
background, `cargo metadata --format-version 1 --no-deps` as the run would:
its program, directory and environment. No other program is started with
those arguments, not even one standing in for cargo under another name. Its
`workspace_root`,
`target_directory` and `build_directory` (cargo 1.91 and later) say where
the run builds. It takes no lock (verified: it answers while a build holds
the build directory's), and it fails no run. It delays none either, but the
first run from a directory while a store holds an orphan of its
repository, which waits for it in case its worktree gets a recycled build
directory ([recycling](#recycling)).

- The `target/` directory is **registered**, with its workspace, in
  `store.json` in the state directory, which outlives the daemon. When the
  build directory is elsewhere, the entry also has `build_dir` and
  `separated_since`: when heli first saw the two apart, in seconds since
  1970, kept while they stay apart (a move from one store to another
  included) and dropped once the workspace builds in its `target/` again.
  Both are optional, so an older `store.json` still loads (#92).
- A build directory at `<store>/<2 hex digits>/<hex digits>` in a store
  heli manages (configured, or its volume's default) gets its manifest,
  JSON: `{"workspace":…,"repository":…,"created_at":…,"last_run":…}`, times
  in seconds since 1970, `repository` `null` outside a repository. The
  daemon rewrites it whole as each run starts, keeping `created_at`.

The stores heli manages are the configured ones and the default store of
each registered workspace's volume, where it exists.

**Sizes.** A file's category follows from its path in a profile directory
(`<profile>/`, or `<triple>/<profile>/`, a triple always holding a `-`):
`incremental/`, `deps/`, `build/` (build scripts) and `.fingerprint/`. In a
registered `target/` directory, what a `[store] tool_caches` pattern names
counts as tool caches; nothing is a tool cache by default. The rest is
other. Sizes are file lengths; links are never followed, and a final
artifact cargo hardlinked into `target/` counts in both directories. The
daemon's accountant scans a directory it has no figures for, those a heli
run built in once the run has ended (cargo has let go of its locks), and any
whose figures are an hour old, which catches what bare cargo built. It
never takes cargo's locks; `status` shows its figures, and
`gc --dry-run` scans afresh.

**Superseded intermediates** (#92). A worktree built before `setup` set
`build-dir` keeps its old intermediates in `target/` after the switch:
cargo no longer reads them, and on flui they were most of the disk. In a
registered, non-shared `target/` directory with `separated_since`, a
profile directory's `deps/`, `incremental/`, `build/` and `.fingerprint/`
are **superseded** when nothing in them was written after that time
(`src/store/superseded.rs`). One cargo still writes to (another
configuration, an environment variable) is not. Final artifacts at a
profile's top level, `examples/`, tool caches and a build directory inside
`target/` never are. `status` and `gc` report each target's superseded
bytes as `superseded`, which `by_category` counts too, and the text form
ends with a hint: `58.3 GiB superseded by the build store: cargo heli gc
--superseded`.

A build directory is in one of three states:

- **Busy.** One of cargo's locks in a profile directory cannot be locked:
  `<build-dir>/<profile>/.cargo-build-lock`, which cargo holds for a build
  (`src/compiler/layout.rs`), or in the artifact directory (`target/`)
  `.cargo-lock` and `.cargo-artifact-lock`. Where the build directory is the
  target directory, all three are in one place (verified with cargo 1.99).
  heli only tries the locks, and lets go at once. A lock it cannot even
  open, it counts as held.
- **Live.** Its workspace path exists: its `Cargo.toml`, since a removed
  worktree can leave files behind.
- **Orphan.** Its workspace is gone, because the worktree was removed.

A build directory without a manifest, which only bare or nested cargo has
built in, is attributed when it can be (#88). Cargo names it after the hash of
its workspace's canonical manifest path (`{workspace-path-hash}`: cargo's
`short_hash`, `StableSipHasher128` over the path, little-endian hex, split
2/14). The hash cannot be inverted, but a candidate can be checked: the
compiler records its working directory, the workspace root, in the object
files of the crates it compiles incrementally. heli reads the absolute paths
in the four smallest `.o` files of the directory's incremental sessions, and
takes the ancestor whose `Cargo.toml` (canonical when it exists; else as
spelled, under its nearest existing ancestor's canonical path, and on Windows
verbatim) hashes to the directory's name: never a guess. It then writes the
owner manifest, with no repository and no last run, and the directory is live
or an orphan like any other. One it cannot attribute (only `check` or
`clippy` built there: no object file) is marked `.heli/unattributed`, tried
again only after cargo has written in it, and is **unknown**: heli cannot
tell whose it is. A user-wide `build-dir` gives every temporary workspace of
cargo run inside tests (trybuild, generated projects) a directory of its
own; `setup` says so, and a repository's `build-dir` leaves them out.
Registered `target/` directories
have the same states, from the same locks and their registered workspace.

`cargo heli gc --dry-run` lists every directory, build and target, with its
state, size, workspace and repository, and the victims an eviction pass
would take ([eviction](#eviction)), and deletes nothing; `gc` without `--dry-run` evicts
them. It reads the store itself and needs no daemon. `--json` prints a
versioned document (`schema`, now 1): `dry_run`; `dirs`, one entry per
directory, with `kind` (`build` or `target`), `path`, `store`, `state`,
`workspace`, `repository`, `created_at`, `last_run` (`null` for a directory
attributed without a run, which the text lists as `last run: none`), `size` and
`by_category`, and `superseded` (bytes, above); `used` (what every directory takes), `budget` and `floor`
(bytes, `null` without one); `volumes`, each with `root`, `free` (`null`
when unknown) and `pending` (bytes still in its trash); `victims`, planned
in a dry run, else evicted, in order, each with `tier`, `path`, `bytes`,
`age` (seconds, as its tier counts it) and `reason`; and `skipped`, the
victims that could not be evicted, each also with `why`. The document's
`superseded` says whether the pass was `gc --superseded`, which reclaims
every superseded directory now (tier 6, [eviction](#eviction)), whatever the budget and the
floor, and nothing else; `--dry-run` combines with it.

## Eviction

The budget is B for all build directories plus registered `target/`
directories (`[store] budget_mb`), with a free-space floor F on each volume
that holds a store or a registered `target/` directory (`[store]
floor_mb`). Neither has a default until that is settled
([open questions](architecture.md#open-questions)):
without them, nothing is evicted. Over budget, heli evicts down to 85 % of
B. Under the floor, it evicts on that volume until F is free there,
counting what its trash still holds as free. It evicts what is cheapest to
regenerate first, oldest first within each tier (the ages are `[store]
scratch_hours`, `unknown_hours`, `crate_hours`, `unit_hours`, `orphan_hours`,
`idle_hours` and `dir_hours`).
A tier's number names it and does not rank it: tiers 6, 7 and 8, the newest,
come where they belong (6 first, 7 after 0, 8 after 7), so that the numbers `gc`,
`status` and the event log already showed keep their meaning.

6. Superseded intermediates in registered `target/` directories ([accounting](#accounting)),
   whatever their age: cargo will not read them again. Reason:
   `intermediates superseded by build-dir <dir>`; age: since the
   directory was seen apart from its build directory.
0. Scratch copies cargo tools left in the temporary directory, untouched for
   1 h. cargo-mutants copies the workspace there for its builds
   (`mutants-*` on Windows, `cargo-mutants-<dir>-*` elsewhere) and deletes
   each copy when done. A cancelled or killed run leaves them behind, each
   a whole source tree with a `target/` of its own, used by nothing. A
   directory there with such a name and a `Cargo.toml` counts
   (`src/store/scratch.rs`). A cargo lock held anywhere inside makes it
   busy. Its trash is `.heli-trash` beside it, on its volume. It counts
   toward B, and `status` and `gc` list it as `scratch`.
7. Build directories of no known workspace (`unknown`, below) untouched
   for 1 h (`unknown_hours`), whole. Tests that run cargo in temporary
   workspaces (trybuild, generated projects) leave one per workspace under
   a user-wide `build-dir`, and on a host of agents they pile up by the
   hundred in a day (#134). Reason: `build directory of no known
   workspace, untouched for N h`.
8. Shared crates ([shared crates](#shared-crates)) that no build directory
   links, unused for 24 h (`crate_hours`), whole. A build directory that
   links an entry keeps it, however old; the entry goes once the build
   directories that linked it are gone. Reason: `shared crate no build
   directory links, unused for N h`.
1. `incremental/<unit>` directories untouched for 24 h, oldest first across
   the store. Each unit ever built leaves one; in the example they make up
   114 GB of one worktree.
2. Orphans that no new worktree claimed within 24 h ([recycling](#recycling)), longest
   unclaimed first.
3. All of a profile's `incremental/` in directories idle for 6 h.
4. Tool caches in the `target/` of worktrees idle for 6 h: cargo-llvm-cov's
   `llvm-cov-target`, built in, and patterns in the configuration, for
   example `tests/trybuild`, and for flui `cli-template-check` and
   `facade-consumer-check`.
5. Whole build directories of live workspaces idle for 7 days.

A registered `target/` directory seen for two workspaces (a global `CARGO_TARGET_DIR` or `build.target-dir`) is `shared` from then on. It is not heli's to manage: it has no rungs, not even 1 and 3, and does not count toward B. `gc` and `status` list it as shared (#65).

What depends on a directory's kind (build, target, scratch, shared or crate) lives in one place, `src/store/kind.rs`: where a cargo lock makes it busy, its trash, its tool caches, its own rungs (6, 0, 7, 8, 2, 4 and 5) and the rule that keeps it whole; rungs 1 and 3 apply to every kind.

Later, between 4 and 5: units in `deps/` and `.fingerprint/` unused for 7
days, cargo-sweep's approach (out of scope for now).

A unit is untouched since its newest file was written. A directory is idle
since its newest file was written or its last heli run started ([accounting](#accounting)),
whichever is later. A profile directory is `<dir>/<profile>/`, or
`<dir>/<triple>/<profile>/` where a triple holds a `-`. A victim inside one
taken before counts only what is left in it, and nothing inside a victim
taken before is one. The clock is the test seam's in debug builds that set
one ([runs](runs.md)).

**Under the floor** a volume really is full: the next build on it fails
for lack of space, which costs more than any rebuild. So there the ages
do not hold. Once the ladder's own victims above are spent, heli takes on
that volume, whatever their age: first what no live workspace will miss
(scratch copies, build directories of no known workspace, orphans whatever
`orphan_hours`), longest unused first; then the live directories' units,
`incremental/` and tool caches; last the live directories whole, longest
idle first, until F is free. Such a victim serves the floor only, never
the budget; `gc` marks it `floor` in its JSON, its reason ends with
`<volume> under its floor`, and so does the event log's `evict` event
(#132, #134).

**What is kept, and why.** When a pass cannot free all it has to, `gc`
says what is left (`left` in its JSON: bytes over the budget's target and
under each floor), each directory the rule that keeps it whole (`kept`:
busy, within its age, not of cargo's layout, in its worktree, or kept
whole over a budget that cannot be met), and the kept directories by
group, most bytes first: `kept     23 live within dir_hours (300.0 GiB);
4 busy (60.0 GiB)`. The daemon's log gets `could not free enough: …`.

**A budget that cannot be met.** The ladder cannot touch a busy
directory, so when the busy directories alone hold more than the target
(85 % of B), no pass can bring the store under the budget: evicting the
live workspaces' idle directories whole only has each rebuilt as soon as
its workspace is used again, on a host that is already saturated, and the
store is over budget all the same (a rebuild storm: 188 directories, 663
GiB, in four hours on one host, #122). So while that lasts, rung 5 of a
live directory does not serve the budget: the live directories are kept
whole, and everything else on the ladder goes as ever, their old units
(1), `incremental/` (3) and tool caches (4) among it, with the scratch
copies (0), orphans (2), build directories of no known workspace (5) and
superseded intermediates (6). The floor is served from every rung as
ever: a volume under it really is full. `gc --dry-run` says so on its
budget line (`budget_unmet` in its JSON), `status` on a `budget` line
with how long it has lasted (`unmet_budget`), and `doctor` as
`store-budget`. The fix is the configuration's: a budget above what the
workspaces that build at once take, and a floor to protect the volume
meanwhile.

Which directories have which tiers:

- A busy directory has none.
- A live build directory has tiers 1, 3 and 5; an orphan 1, 2 and 3. An
  orphan is deleted whole by tier 2 only, so it has its 24 h to be claimed
  by a worktree of its repository ([recycling](#recycling)). One without a repository (a
  temporary workspace's, attributed as in [accounting](#accounting)) can be claimed by none and
  does not wait (#88).
- An **unknown** build directory, which only bare or nested cargo built in,
  has tiers 1, 3 and 7. Its first heli run gives it a manifest. Tier 7
  needs cargo's layout: a `<2 hex>/<14 hex>` directory of its own in a
  store heli surveys (configured or default), whose profile directories
  hold cargo's lock files, none of them held; this is checked again
  before eviction. It remains unknown: being reclaimable establishes no
  workspace identity, orphan status or eligibility for recycling.
- A configured `store.paths` root whose final directory heli creates
  receives `.heli-cache-root.json` (schema 1, bound to its canonical
  path), and `gc --json` reports it as `managed_cache_root`. It records
  where a store came from and no longer decides what is reclaimed: until
  0.0.5 an unknown directory went whole only in such a root, so stores
  that existed before heli kept every nested build forever (#134).
- A shared crate entry has tier 8 only, and only while no build directory
  links it; it is checked again before eviction.
- A registered `target/` directory has tiers 1 and 3 where cargo keeps its
  intermediates there (no `build-dir`), tier 6 where it kept them before
  `build-dir` moved them, and tier 4. It is in the user's worktree, so it
  is never deleted whole. A tier 6 victim is renamed into its trash under
  the profile locks like the others, so a held `.cargo-lock` or
  `.cargo-artifact-lock` skips it, and only if it is still untouched since
  the separation.

Safety rules:

- **Protect runtime, not just compilation.** Cargo 1.99 releases its layout
  locks before executing tests. A public nested-test reproducer, gated after
  compilation, lost its `OUT_DIR` asset to GC with `dir_hours = 0`. Every
  managed command now holds a shared runtime lease of its worktree
  (`<activity>/worktrees/<hash>/`, the worktree's path in `path`) for its
  whole lifetime, including the build/test transition. Independent commands
  take compatible shared leases; they do not queue behind each other. An
  internal command parent also holds the lease until the actual command
  exits, so killing only the daemon does not expose surviving tests, and it
  lets go before it exits. Each holder leaves a marker in the lease while it
  holds it (`run-<nanos>-<pid>-<n>`, naming the command), so eviction knows
  when the oldest run in flight started.
  GC leaves alone (#108):
  - the directories of a workspace related to a worktree a run is in flight
    in (the workspace inside the worktree, or around it). It takes those
    leases exclusively, without waiting, and holds them through the rename,
    outside the victims and independently of `HELI_STATE_DIR`;
  - any victim that changed since the oldest run in flight started:
    nested commands can build and use another workspace's cache, a
    temporary one without an owner or another known one, and what they
    wrote is that recent. Both are real times (file times and the runs'
    markers), never the test seam's clock. Such victims are left out before the ladder
    chooses, so that it chooses others, and `gc` lists them as skipped
    ("changed since a run in flight started").
  heli's own notes in a build directory (`.heli/`) do not count as a
  change. Older, idle directories go while runs are in flight: one host-wide
  guard, as before, deferred all eviction for as long as any run was in
  flight, which on a host of busy agents is always (the store grew to fill
  its volume). B is a reclamation target, not a hard disk quota. A volume
  the store is on that is under its floor is a `doctor` warning
  (`store-floor`). When the volume is too full even to rename a victim into
  the trash (`os error 112` on NTFS), the victim is deleted in place, under
  the same locks. Bare Cargo's compilation locks remain respected, but bare
  runtime and detached background descendants require explicit integration
  and are not covered by this managed-command guarantee. All processes using
  the same caches must use the same heli version and activity namespace;
  `HELI_ACTIVITY_DIR` is only for explicitly isolated test/spike environments.
  It must be absolute. `gc --json` exposes `runtime_busy`: some directories
  were left alone for runs in flight; an activity namespace that does not
  resolve leaves everything alone. Failure to
  acquire a run's guard refuses that managed command rather than executing
  it without runtime protection (the narrow G5 exception above).
  A daemon-owned runtime guard releases its file before requesting another
  accounting/GC pass on every exit path. The request is retained under the
  accountant's condition-variable mutex, so an idle transition cannot be lost
  before the accountant starts waiting. This also covers `exec`, which has no
  Cargo learning worker. Shared activity still prevents eviction while another
  managed command runs. Activity released only by another daemon, a surviving
  command guardian or an external participant is observed by the existing
  periodic survey; this local notification is not a cross-process broadcast.
- **Never wait on a lock.** heli tries to lock `.cargo-build-lock` (build
  directory) or `.cargo-lock` and `.cargo-artifact-lock` (artifact
  directory) exclusively, in every profile directory of the victim's
  directory, and anywhere inside the victim (including nested Cargo caches
  left by build scripts). If one is
  held, or cannot be opened, the directory is busy and skipped. heli checks
  as it plans, and again as it evicts.
- **Rename, then delete.** Under the locks, heli only renames the victim
  into the trash on its volume, which takes constant time, and releases the
  locks: `<store>/.trash/` for a build directory's victim,
  `<target>/.heli-trash/` for a registered `target/` directory's. Windows
  refuses to rename a directory with a file open inside, so there heli lets
  go of the locks inside the victim (a whole build directory's) just before
  it renames, and the rename fails if a cargo opened one meanwhile. The
  trash is deleted afterwards, at low I/O priority (background mode on
  Windows, the I/O throttle on macOS, the lowest nice value on Linux): by
  `gc` before it exits, and by a thread of the daemon's, which also deletes
  what a `gc` that was stopped left there. A cargo that starts meanwhile
  finds its directory without the victim and rebuilds it; it never waits
  for the deletion. If a file in the trash is temporarily undeletable (for
  example, a Windows handle denies deletion), `gc` reports the failed
  cleanup and retains it for a later pass; the daemon also retries its
  trash cleanup. This does not restore the victim or hold Cargo's locks.
  A trash counts in no directory's size.
- **Narrow scope.** heli deletes only below registered roots (the stores'
  build directories and the registered `target/` directories), only the
  subpaths listed above, and never through a symlink or a junction: no path
  to a victim crosses a link, a link is never a victim, a link inside a
  victim is removed and not followed, and a trash that is a link is never
  used.
- **Unknown layout.** Lock names change between cargo versions. A profile
  directory with cargo's intermediates (`deps/`, `incremental/`,
  `.fingerprint/`, `build/`) and none of the three lock files has a layout
  heli does not know. Then eviction treats its directory as busy when
  anything in it changed within the last hour, and `gc` lists it so.

**Passes.** `cargo heli gc` makes an eviction pass: it plans the victims,
evicts them, logs each eviction in the event log ([observability](observability.md)), and deletes the
trash; `--dry-run` names each victim with its tier and reason and changes
nothing. With a budget or a floor configured, the daemon's accountant
([accounting](#accounting)), at each look (every minute, and when a run ends), keeps the
orphans' notes ([recycling](#recycling)) and makes a pass when its figures are over B or a
volume they are on is under F. One pass evicts at a time (`evict.lock` in
the state directory): `gc` waits for the daemon's, and the daemon skips its
own while `gc` runs. `status` shows the daemon's last pass that evicted
anything ([observability](observability.md)). The daemon's log gets what a
pass found: the directories left alone for runs in flight, each victim
skipped and why, the budget it cannot meet, and what it evicted. A pass
that evicts nothing and finds the same as the pass before writes nothing
(on a host where runs are always in flight, one every look wrote the same
ten lines, 16.7k a day, #122); once an hour, one line says how many passes
found the same and what that is now.

## Recycling

A worktree with no build directory can appear in three ways: the first
`cargo heli` run in it, the git `post-checkout` hook that `cargo heli setup`
installs (git runs it for `git worktree add`), or the daemon's periodic
`git worktree list`. heli gives it the most recently used orphan of the same
repository, renamed to the new worktree's `{workspace-path-hash}` path. On one
volume that is a rename: no copy and no extra disk. One build directory still
belongs to one live worktree.

- **The donor** is an orphan ([accounting](#accounting): its workspace's `Cargo.toml` is gone,
  none of cargo's locks in it held) in the store that holds the new build
  directory, whose owner manifest names the same repository; of those, the
  one with the latest last run. Never taken: live, busy and unknown
  directories, orphans of other repositories or of none, and what eviction
  moved into `<store>/.trash/` ([eviction](#eviction)). An orphan stays claimable until
  eviction takes it, which is no earlier than 24 h unclaimed ([eviction](#eviction), tier 2).
- **The new build directory** is the one cargo names for the worktree
  (`build_directory`, [accounting](#accounting)): heli recycles only when it is in a store heli
  manages and does not exist. An existing one, even empty, is never
  replaced. Afterwards its owner manifest names the new workspace, keeping
  its creation time.
- **The first run.** The run's own `cargo metadata` ([accounting](#accounting)) names the build
  directory, and recycling happens before the run starts: the first run
  from a directory waits for it while a store holds an orphan of its
  repository ([runs](runs.md), "Where a
  run can wait"). The run's clients get a note:
  `cargo-heli: note: recycled the build directory of <donor>, a removed
  worktree; the path scan invalidated 2 units`, or after `worktree` for
  precise recycling (below) `, precisely (412 files got their recorded
  times)`.
- **The hook.** setup offers the repository's `post-checkout` hook (the file
  in its hooks directory, as `git rev-parse --git-path hooks` names it, so
  `core.hooksPath` counts; one file serves every worktree), with its own
  question, after the Cargo configurations ([placement](#placement)). git runs it after every
  checkout; only for a checkout from the null commit, which is what
  `git worktree add` does, does it run `cargo heli recycle` in the new
  worktree, which reads the store itself and needs no daemon. It runs
  before `git worktree add` returns, says what it recycled on stderr, and
  never fails the checkout. It names heli by its full path; setup replaces
  a hook it wrote earlier, and leaves any other one alone, with the reason.
- **The look.** As the daemon starts, as runs end and every minute, for each
  repository with an orphan in a store, it lists the worktrees
  (`git worktree list`). Each linked worktree it has not looked at before
  gets a build directory recycled, if it has none. The main worktree is
  never new, and is left to its first run.
- The hook and the look ask cargo from the top of the worktree, with their
  own environment, and recycle only for a workspace inside it. While no
  store holds an orphan of the repository, they do not ask cargo at all.
- Each recycling is a `recycle` event in the event log ([observability](observability.md)). One recycler
  at a time recycles in a store: it holds the store's recycling lock,
  `<store>/.recycle.lock`, waiting at most 25 s for it (else it recycles
  nothing), and once it has it, looks again whether the build directory
  exists, since the hook, the look and a run can race for one worktree. On
  Windows a directory that another recycler walks (to scan it) cannot be
  renamed, so two recyclers of one donor could each make the other's
  rename fail, and cargo would then make an empty build directory. The
  operating system releases the lock of a recycler that dies. Beside it
  (an older heli takes none), a rename never replaces a directory that has
  files, each scans the donor under its old name, which a rename by the
  other takes away, and before each donor a recycler checks again that the
  build directory does not exist.
- **The claim.** Before anything else, a recycler rewrites the donor's
  owner manifest to name the new workspace (keeping its creation time),
  then scans and renames it. Its workspace lives, so from then on no other
  recycler takes the donor for an orphan, under its old name or its new
  one: a directory at a worktree's place never names a removed worktree,
  even for the moment between the rename and the manifest. After the
  rename the manifest is written once more, in case another recycler
  claimed the same donor after this one. A recycler that gives the donor up
  (it cannot be moved, its worktree is older than its last build) writes
  its manifest back, unless another recycler claimed it since or it moved.
  A donor whose manifest cannot be written is not taken. A recycler that
  dies between the claim and the rename leaves the donor named after a
  live workspace, where it is: recycling never takes it, and eviction does
  ([eviction](#eviction)). Under the store's lock no other recycler of this heli runs
  meanwhile; the claim also keeps a recycler that takes no lock (an older
  heli's hook) from taking the directory.

**Orphans and eviction** ([eviction](#eviction)), one rule for both. An orphan is unclaimed
from the first eviction pass that finds its workspace gone: the daemon's,
every minute while a budget or a floor is set, or `gc`'s. That pass writes
`.heli/orphaned` in it, `{"workspace":…,"since":…}`. The note counts while
it names the owner manifest's workspace and is no older than its last run;
a directory found live loses it, and `gc --dry-run` writes none. An orphan
becomes evictable (tier 2) only after 24 h unclaimed, and no other tier
deletes it whole, so recycling has at least that long to claim it. A
recycled directory names its new workspace, so its old note no longer
counts. Recycling never takes a directory that is already in the trash:
`<store>/.trash/` is not a build directory (`<2 hex digits>/<hex digits>`),
and an eviction renames its victim there before deleting anything. Either
rename of an orphan, by eviction or by recycling, fails once the other has
moved it, and the loser goes on without it.

What makes this sound, verified in cargo's source:

- **Paths cargo tracks.** Cargo's own dep-info encodes each input path
  relative to the package root, the workspace root or the build root. It keeps
  absolute paths only for files outside the workspace, such as registry
  sources and the sysroot, and those are the same for every worktree on the
  host (`src/compiler/fingerprint/dep_info.rs`, `DepInfoPathType`).
- **Build scripts.** Moves of a build script's `OUT_DIR` are handled through
  the `root-output` file (`src/compiler/custom_build.rs`).
- **Unit hashes.** They do not depend on the workspace's absolute path: flui
  saw identical artifact names across worktrees.
- **Freshness** is decided by mtime, including the rule that a unit is stale
  when a dependency is newer than it (`src/compiler/fingerprint/mod.rs`,
  `StaleDependency`).

**What cargo does not track: paths compiled into binaries.** Cargo drops the
environment variables it sets itself from its own freshness tracking:
`OUT_DIR`, `CARGO_MANIFEST_DIR`, `CARGO_BIN_EXE_*`, `CARGO_TARGET_TMPDIR`. Its
source says so: "technically buggy … we won't recompile that if you move the
target directory" (`dep_info.rs` 335). After a rename, a unit that stays fresh
keeps the donor's paths compiled in:

- `env!("CARGO_MANIFEST_DIR")` points into the removed worktree. flui has
  about 20 files that do this, mostly tests locating their fixtures.
- `env!("OUT_DIR")` points under the old build directory path.

rustc still records these values in its own dep-info (`# env-dep:NAME=value`
lines in `deps/*.d`, observed on the example host). So heli scans the rustc
dep-info of the donor, where it lies, and only then renames it: a cargo that
starts in the new worktree never sees it half scanned. It deletes the
fingerprint of every unit with an `env-dep` value under the donor's
workspace or the old build directory path, and cargo rebuilds those units,
and what depends on them. This applies in both modes below.

- **Values.** rustc escapes `\`, line feed and carriage return as `\\`, `\n`
  and `\r` (on Windows, `# env-dep:CARGO_MANIFEST_DIR=E:\\heli`). A value is
  under a path when its components start with the path's: on Windows
  without case, with either separator, and without `\\?\`. The old paths
  are spelled as cargo spelled them: the workspace as `cargo metadata`
  named it (the owner manifest), and the build directory as cargo's
  `build-dir` places it in a store heli manages. A value that lists paths
  (`PATH`-like) is also looked at path by path. Any variable counts, not
  only cargo's.
- **Where cargo keeps them** (cargo 1.99, checked by experiment), in each
  profile directory (`<profile>/` or `<triple>/<profile>/`): rustc's
  dep-info is `deps/<crate>-<hash>.d`, `examples/<crate>-<hash>.d`, or for a
  build script `build/<package>-<hash>/build_script_build-<hash>.d`; the
  unit's fingerprint is `.fingerprint/<package>-<hash>/`, with the same hash
  (cargo's `-C extra-filename`). A crate's name has `_` where its package's
  has `-`, so only the hash ties the two. Units whose files have no hash (on
  Windows MSVC a path package's executables, and dylibs and cdylibs of path
  packages) write `deps/<crate>.d`; their fingerprint directory still has a
  hash, and its files are named after the target (`dep-<kind>-<target>`,
  `<kind>-<target>`), so such a file stands for every fingerprint directory
  with a file whose name ends in its crate's name, `-` read as `_`.
  Invalidating a unit too many costs only its rebuild.
- The number of units it invalidated is in the `recycle` event.

There are two modes:

Both start from the donor's **last build**: the newest modification time of
a file in its build directory, leaving out heli's own `.heli/` and rustc's
`incremental/` caches (as for the record's validity, below), read before the
path scan. heli stages the new worktree first (as for the record, below); a
worktree git cannot stage gets no recycled directory.

- **Conservative**, what heli does when the donor has no record that
  counts: only for a new worktree whose every file (tracked or untracked,
  as staged) was last written after the donor's last build, as in a
  worktree just made. Every workspace member then rebuilds. Third-party
  packages stay fresh, except those invalidated by the scan above (a crate
  that compiles an `OUT_DIR` path in, including through `include!`). This
  is correct by cargo's own rules plus the scan. A worktree with an older
  file (one made before the donor's last build: the main worktree at its
  first run, or a worktree the look finds) gets no recycled directory from
  this donor: cargo would find units built from another content fresh, and
  link the donor's code. heli tries the next orphan, and else the worktree
  builds cold. The cost of the `OUT_DIR` invalidations is to be measured.
- **Precise.** The donor has a *source-state record*: the raw content's
  SHA-256 digest, Git blob id, size and mtime of every tracked or untracked file.
  - **File times and the clock.** A file's mtime can be earlier than the
    moment, by the clock heli reads, at which it was written: the kernel
    dates a write by a coarse clock, up to a tick (some milliseconds)
    behind, and a file system keeps what its format holds, whole seconds on
    HFS+ and two on FAT. So wherever heli compares a file's mtime with the
    clock, it takes the file as written as late as its mtime plus a
    *slack*: 50 ms for an mtime with a fraction of a second, which comes
    from a file system that keeps fractions (10 ms or finer), and 2.05 s
    for a whole second, which may come from one that keeps whole seconds or
    two. A file changed right after heli's staging read it (below), to
    content of the same size, may be dated before the staging all the same;
    without the slack it would get the recorded mtime, and cargo would link
    the donor's build of the old content. heli reads each mtime from the
    file itself, never from a directory listing: on Windows a listing gives
    what the file's directory entry holds, which keeps the time of the
    file's creation until its writer closes it, and may lag on a loaded
    host, so a record taken by it could be taken before the build.
  - The record counts only if it was taken after the donor's last build: its
    time is no earlier than the mtime of each file in the build directory
    plus its slack, leaving out heli's own `.heli/` and rustc's
    `incremental/` caches, which no freshness decision reads (a build that
    changes what cargo decides writes a fingerprint). Otherwise heli ignores
    it. It is read before the path scan, which changes the directory.
  - For each file in the new worktree whose raw content digest and size match the record,
    heli sets the recorded mtime, if the file was written before heli began
    to stage the worktree: its mtime plus its slack is no later. So that a
    worktree just made gets the recorded mtimes, heli stages it again, once,
    when some file may have been written after the staging began, after
    waiting until none may have been (at most 2.05 s; a file dated later
    than that is not waited for). Every other file last written at the
    donor's last build or before gets the time now, and at least its slack
    after that build, so that a time cut to whole seconds stays after it (a
    worktree made before that build has such files); any other keeps its
    time, newer than every artifact already. Git's blob id alone cannot prove
    equality: filters can normalize different files of the same size, and
    `include_str!` compiles in bytes. The digest is checked again through
    the handle used to restore the time. Should a
    file not get its newer time, heli deletes the fingerprint of every unit,
    and cargo rebuilds them all.
  - Every file with the donor's content then looks to cargo exactly as it did
    in the donor, and every changed file is newer than any artifact. Only the
    changed crates, their dependents and the units the scan invalidated
    rebuild.
  - Only the recycler whose rename succeeded sets times, with the record of
    the directory it moved, after the owner manifest names the new
    workspace. It sets a file's time through a handle that on Windows lets
    nobody write to the file meanwhile: a file an agent writes while heli
    looks keeps its time.
  - The daemon writes a record after every run it executes, before the
    run's clients hear its end, so that a worktree removed right after its
    last run leaves one; and when it sees a build lock released: held at one
    look of its accountant ([accounting](#accounting)), free at the next, unless a run in flight
    there will write one as it ends, or the record still counts. Never while
    one of cargo's locks in the build directory is held, and not for a build
    the daemon did not see. A run the client executes without a daemon
    writes none. Before taking its time, the daemon waits until each file of
    the build directory is a slack old (at most 2.05 s, and 50 ms on a file
    system that keeps fractions of a second), so that the record of the
    build that just ended counts.
  - Ignored inputs, such as an `include_str!` of an ignored file, are not
    recorded, and neither mode looks at them. Cargo rebuilds when one is
    missing; one that is present triggers a rebuild when it was written
    after the donor's last build, as in a worktree just made. An ignored
    input of other content, present in a worktree made before that build,
    is the case recycling does not cover yet.
  - With `-Zchecksum-freshness` (cargo#14136), cargo compares content itself,
    and precise mode becomes unnecessary. The path scan does not.

  The record is `.heli/sources` in the build directory, JSON, replaced
  whole: `{"taken_at":…,"files":{"<path>":{"blob":…,"content":…,"size":…,"mtime":…}}}`,
  each path from the top of the worktree as git writes it, times in
  nanoseconds since 1970. `taken_at` is read before anything else, so a
  build that starts meanwhile makes the record stale, its files dated at
  most a slack before `taken_at`. The blob ids come from
  the result store's snapshot ([result store](result-store.md)): `git add -A` into a scratch copy of the
  index, then `git ls-files --stage`; raw content digests, sizes and mtimes are read afterwards,
  so a file changed in between looks newer, never older. Symbolic links and
  submodules are left out. An older record without raw content digests is
  ineligible for precise recycling; conservative recycling still applies.

**Later: cloning.** On volumes with block cloning (Windows Dev Drive on ReFS,
btrfs, XFS, APFS), recycling can clone instead of renaming, preserving mtimes.
A *live* worktree can then donate, forking a warm build directory. The same
rules apply.

## Shared crates

Every worktree of a host builds the same registry crates: tokio, serde,
syn, each once per build directory, and a host of agents keeps hundreds of
build directories (#139). With `[store] shared_crates = true` heli compiles
each of them once per store and links the result into every build
directory that needs it.

heli does this as cargo's `RUSTC_WRAPPER`: the client sets the variable to
its own binary (with `HELI_SHARED_CRATES=1`) unless a wrapper is already
configured, in the environment or in `build.rustc-wrapper`; it then stays
out of the way. The wrapper (`src/crates.rs`) shares a compilation only
when all of these hold, and otherwise runs rustc unchanged:

- cargo capped its lints (`--cap-lints allow`), which it does for every
  package that is not a local one: registry, git and vendored crates;
- it is not incremental, and has `--crate-name`, `-C extra-filename`,
  `--out-dir` and a root source file inside `CARGO_MANIFEST_DIR`;
- its build directory is one of a store's, `<store>/<2 hex>/<hex>`
  (`build-dir = "<store>/{workspace-path-hash}"`, as [placement](#placement)
  sets it).

Its key is a SHA-256 of what decides the output: `rustc -vV`, the
arguments with the build directory replaced by a placeholder, the digest
of every `--extern` file, cargo's `CARGO_PKG_*`, `CARGO_MANIFEST_*` and
`CARGO_CRATE_NAME` variables, the variables the package's build script set
for the compiler, and the digests of the files in its `OUT_DIR`. The
metadata hash cargo puts in each file name is the same in every worktree
for the same crate, version, features and profile, so the names match.

The store keeps an entry per key in `<store>/.crates/<2 hex>/<key>`: the
output files, `meta.json` (the files, the dep-info, rustc's JSON messages,
and the environment variables and files the compilation read, from the
dep-info's `env-dep` lines) and `used`, touched on each use.

- **Hit**: the wrapper checks the recorded variables and files, hardlinks
  the files into `--out-dir`, writes the dep-info with the build directory
  filled in, and replays rustc's messages, so cargo sees what it would
  have seen. Linked files keep the entry's mtime; when a dependency in this
  build directory is newer than the entry (built after it), the files are
  copied with the current time instead, so that cargo's freshness check,
  which compares mtimes, finds them fresh.
- **Miss**: one compilation per key at a time (a lock in
  `.crates/.locks`); the others wait, then hit. rustc writes to a staging
  directory, and each artifact is linked into `--out-dir` the moment rustc
  announces it, so pipelined dependents start on the `.rmeta` as they
  would. Then the staging directory becomes the entry.

Anything the wrapper cannot do (no hardlinks across volumes, an entry gone
mid-use) falls back to copying or to rustc itself: sharing can make a
build faster and never fail it.

Accounting counts an entry's files once, in `.crates/`, by link count:
`status` shows what no build directory links (`crates` in the `disk`
line, `disk.crates` in its JSON), and `gc` lists each entry as `crate`,
`live` while a build directory links it. A crate entry counts toward B,
and [eviction](#eviction) takes it by tier 8.

## Dependency epochs

Separate worktrees and artifact directories do not remove Cargo's shared
package-cache lock. The experiments linked in `docs/multi-agent.md` verified
private writable Cargo homes consuming shared immutable vendor sources. This
ownership model is now available explicitly, through ordinary managed runs:

```text
cargo heli deps import --store <absolute-new-store> --vendor <vendor-tree> --config <cargo-vendor-output.toml>
cargo heli --dependency-epoch <printed-epoch-path> --cache off check --offline
cargo heli --dependency-epoch <printed-epoch-path> test --offline
cargo heli deps gc --store <absolute-owned-store>
```

Import accepts the source-only replacement TOML emitted by `cargo vendor` and
one supplied directory source. It verifies the complete file inventory and
each Cargo file checksum before and after copying. Original archive checksums
are preserved for Cargo's lockfile verification; extracted files alone cannot
authenticate the original archive. Unsupported configuration, links within
input packages and path escapes are refused. Existing unknown directories
are never adopted as managed stores.
Informational checksum-envelope fields such as Cargo's `$comment` are
accepted, as Cargo accepts them; the raw checksum file remains part of the
object identity and the actual file inventory is still verified strictly.

The store holds package objects by content identity. Epochs contain source
configuration, a manifest and cheap views of those objects: NTFS junctions on
Windows and directory symlinks on Unix. Different epochs can reference the
same physical object. A complete staged epoch becomes visible by rename;
partially published or corrupt state is refused, rather than executed.
This is atomic visibility, not a claim of durability through arbitrary
power loss. Objects include names, bytes, empty directories and executable
bits in their identity. Opening an epoch verifies the stored bytes as well
as its manifest, configuration and link targets.

Each epoch has an external shared lifetime lease. It is acquired before
reading its manifest, including before result lookup and metadata discovery.
The daemon holds ownership across queueing, build/test phases and final
keying; asynchronous metadata learning owns a separate lease. Each selected
epoch's Cargo metadata subprocess also runs under an independent guardian:
a daemon-thread lease alone cannot protect an orphaned metadata child after
the daemon dies. Hooks and daemon maintenance queries preserve a nonempty
inherited epoch selector too; they must not lose ownership merely because
they bypass normal client preparation. Discovery capture preserves raw Cargo
JSON without a runner handshake prefix. The command's
guardian acquires and validates ownership before spawning Cargo, retaining
it after client or daemon loss. Direct fallback uses a separate guardian
too, preserves the prepared execution context and raw stdio, and never
falls back to the original shared Cargo home. Invalid snapshots are terminal
errors. Protocol 2 prevents silent execution by a daemon that does not honor
this contract. A `Preparing` response acknowledges a live daemon before
potentially expensive validation, without treating validation as a failed
five-second connection handshake.

Explicit dependency GC tries exclusive epoch leases without waiting for
builds. It may retire an unleased epoch while another epoch remains active;
all objects referenced by retained epochs stay live. Publication and GC
serialize catalog mutations. Staging and GC's object verification happen
outside that critical section; ready readers do not acquire the catalog
lock. Duplicate import currently re-verifies reused objects under the catalog
lock, so concurrent publishers can wait there; active ready consumers do not.
Selecting an epoch already being retired fails promptly rather than waiting
for its trash deletion; callers can select a ready epoch or retry publication.
Epoch lease files are never unlinked, avoiding split-lock identities after
retirement and republication. Trash deletion does not follow package links.
This source lifecycle does not weaken the conservative global runtime guard
used for arbitrary build artifacts in [eviction](#eviction).

The invoking client prepares a stable private Cargo home for its worktree,
epoch and original configuration/credential inputs. It publishes rather
than rewriting an existing home. Cargo 1.99 or newer is required. Original
`config.toml` is included at home precedence with its original path origin;
workspace and CLI overrides retain their native higher precedence. The
original config remains a live file, as in ordinary Cargo. Credentials are
copied privately outside shared objects: Unix owner-only modes and Windows
protected owner-only ACLs are established before bytes are written. An
unsafe existing private home is refused. Nested Heli retains the selected
home and epoch instead of building recursive include chains. Cargo,
toolchain and wrapper selection retain the caller's PATH order; the original
home's bin directory is appended only when absent.

This immutable-source profile requires explicit preparation and selection.
The native home profile in [worktree Cargo homes](#worktree-cargo-homes) needs no epoch publisher or vendor prepass;
automatic epoch publication is an optional storage optimization, not a
prerequisite for independent Cargo downloads. Capacity accounting for
sources/staging/private homes/lease metadata and automatic private-home
reclamation remain unfinished. Legacy home `config` is
explicitly refused because Cargo includes require `.toml`; copying it would
change relative-path semantics. Arbitrary cargo extensions, competing
home-bin/PATH extension precedence and tools mutating the Cargo home need
the tool profiles tracked in #38. Native workspace/CLI source overrides can
select sources outside an epoch; this feature does not claim to lease those
external directories. The existing artifact budget does not yet include
this opt-in dependency store. No hard disk or RAM bound follows from enabling
the profile alone.

Public acceptance cases are in `tests/dependencies.rs`: two live Cargo
builds under a held baseline package-cache lock; collecting an unrelated
epoch during those builds; delayed source reads after client/daemon loss
and during direct fallback; original config origins/precedence and private
credentials; and refusal of corrupted source objects before execution.
They also keep a gated metadata child alive after the foreground command and
daemon end, and switch source values 7 → 8 → 7 with one unchanged lockfile and
target while retaining both source views. The production concurrency canary
in `experiments/dependency-epoch-concurrency.py` exercises shared-object GC
between exits of three consumers across two projects and two Git worktrees.
Cross-platform results must be reported for the exact tested revision.

## Worktree Cargo homes

The independence profile uses ordinary writable Cargo homes, one per
worktree, rather than requiring a vendor snapshot before a command can run.
With `[cargo] home = "isolated"` in the user configuration, or
`--cargo-home isolated` on a command, the invoking client prepares the home
before submitting the command. Default `shared` preserves the caller's
Cargo home. Explicit epochs remain an optional source-storage optimization;
an explicit shared-home request conflicts with an epoch selection.

Preparation creates configuration and private credentials, not a dependency
resolution pass. Cargo itself downloads, resolves and builds with its native
flags. Each worktree owns its mutable registry, index, extracted sources and
git cache. None are links to another worktree's or the original home's
mutable cache. Concurrent cold worktrees may download the same crate twice;
avoiding their mutual package-cache lock takes priority over storage savings.
Home preparation does not wait for another worktree to download its
dependencies. Live verification sharing still follows the complete input-key
policy in [runs](runs.md).

The secure publication mechanism in [dependency epochs](#dependency-epochs) also applies without an epoch:
original home configuration is included at home precedence with its relative
origins intact, credentials are copied only into protected files, and a
stable generation is published rather than rewritten under active commands.
Worktree identity is the canonical nearest checkout root, not Git's common
directory; outside Git it is the invoking directory. Config or credential
changes select a new generation for a new outer command. Already running
commands and their nested Heli invocations retain their exact parent home,
including in generated directories outside the parent's repository.

Native generations use the compact `<state>/h/<32 hexadecimal digits>`
layout. The directory name is a prefix, not the authority for ownership:
the schema-3 marker records the complete 256-bit home identity and reuse
must compare that identity. A prefix collision or an inconsistent marker
is an error; it must never attach a command to another worktree's mutable
cache. Existing epoch layouts and inherited schema-1/2 homes retain their
paths. New layout selection does not move or delete an active generation.

The short layout is a native-tool compatibility requirement. In the real
two-worktree FLUI acceptance, an existing aws-lc C source reached 268 path
characters and MSVC reported C1083, even though Rust could create and read
the file. A regression compiles a C source at that dependency's actual
registry depth with the installed MSVC compiler. Shortening only the test
checkout would conceal this defect. An arbitrarily deep custom state root
or dependency can still exceed a native tool's path limit; use a short
`HELI_STATE_DIR` on Windows when selecting a custom root.

Cold `--offline` runs can fail because the private home has no dependencies
yet. Heli must not hide that failure by retrying through the original shared
cache. Warm offline runs reuse their worktree's cache. No hidden step creates
or updates `Cargo.lock`; `--locked` retains Cargo's normal meaning. Interrupted
downloads are recovered by Cargo in that home. Selected isolation survives
daemon loss and direct fallback rather than silently becoming shared.

Home-mutating Cargo commands need an explicit destination policy. This
profile refuses `install`, `uninstall`, `login`, `logout` and `publish`, and
the tools whose profile writes the Cargo home (`writes_home` in [runs](runs.md):
`binstall`, `install-update`, `quickinstall`), instead of silently changing
their original destination or installing into a private `bin/` that nothing
has on `PATH`. A command that only prints is not refused and runs as bare
Cargo would, with the caller's own home: `install --list`, which reads the
home's record of installed tools (so it lists what is really installed, not
the empty private home), and `-h` or `--help` of any of those commands, and
`-V` or `--version` when it is the only argument (`install` and `binstall`
take `--version <semver>`). `install foo --help` counts as help. Inside an
already isolated parent the home stays the private one. An explicit shared request
inside an inherited isolated command is refused rather than escaping its
parent's context. Use an outer shared command for home maintenance. Arbitrary
extensions and executables may interpret Cargo home themselves; their full
compatibility remains part of #38. Existing Cargo 1.99+ and legacy home
`config` limitations apply. Private native homes are retained and are outside
artifact-store accounting; automatic retirement requires lifetime ownership
before deletion. No hard resource bound follows from this profile.

Acceptance must exercise the public CLI with two actual Git worktrees and
a localhost registry: delay A's real download and observe B's build-script
and compiler progress before releasing it; resolve a nested generated
project whose dependency is outside the parent's graph; preserve original
home/config/credential and project lockfile bytes; distinguish cold and warm
offline runs; and verify configuration origins, private credentials, unsafe
home refusal and direct fallback. `tests/cargo_homes.rs` covers the public
home policy, config/credential semantics, simultaneous worktrees and a real
fallback command surviving client loss with its GC guard. The local registry
fixture `experiments/isolated-home-concurrency.py` covers actual cold downloads,
nested resolution, offline behavior and optional authenticated access through
Cargo's token provider. Native Windows runs pass; cross-platform verification
must be reported for the exact tested revision. This is not a completion
claim for #38 or the overall multi-agent acceptance.

**Isolation and live-sharing integration, 2026-10-16.** The native-home
profile is integrated with the source-aware live policy from [runs](runs.md). Home
selection happens before submission and inventory uses the selected home.
The generated home's location alone does not prevent equivalent verification
from sharing; its Cargo configuration and all other keyed inputs still count.
Incomplete offline source discovery disables sharing rather than certifying
an unknown dependency graph.

Public real-Cargo tests in `tests/single_flight.rs` cover both shared and
isolated modes: two same-worktree test clients execute one test process,
then a source edit executes the new revision after the old executable exits.
Both direct tests and `exec --verify` wrappers exercise that transition.
Equivalent checks can share across isolated worktrees with result caching
off, while builds in another worktree or output directory produce their own
actual `.rlib` files. No persistent verdict marker is created by those runs.

The native-home registry experiment now uses `exec --verify`, exercising
source-key preparation as well as isolation. Local Windows integration
runs pass its ten unauthenticated and thirteen authenticated checks: B
finishes with real compiler output while both A's download and a shared-home
control remain blocked; nested resolution retains B's home; original
configuration, credentials and lockfile bytes remain unchanged. These are
mechanism checks, not a throughput comparison or a hard disk bound. They do
not remove Cargo's artifact lock for distinct commands sharing one target,
the nested-run admission bypass, or the need for separate worktrees for
independent editors.

The integrated Windows revision passes the full 419-test suite across 38
targets, strict all-target Clippy and formatting. Five acceptance-driver
safety tests also pass. Cross-platform results must refer to the integrated
revision, rather than the earlier private-home or live-sharing CI alone.
