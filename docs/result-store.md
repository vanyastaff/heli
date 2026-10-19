# Result store

For each passing verify or test key, heli keeps a marker
`<state>/verified/<digest>` holding the run id, the worktree and the time. It
keeps at most 256 markers and drops the least recently used first; a hit
counts as a use. The daemon keeps the store: a run that the client executes
directly, without a daemon, neither looks it up nor records.

**The key.** These rules carry over the lessons of flui#1619, where each one
was found by a failing test.

- **Encoding.** Every field is hashed as its name's length, the name, the
  value's length and the value. Values are raw bytes, never lossy UTF-8:
  environment values, Cargo configuration files, tool output.
- **Tree.** The worktree with every change staged: `git add -A` into a
  scratch copy of the index (`GIT_INDEX_FILE`), then `git write-tree`. The real
  index is never written (`GIT_OPTIONAL_LOCKS=0`). The copy keeps the
  index's stat cache, so unchanged files are not hashed again, and the
  index's own time. git takes a file whose times and size match its entry
  for unchanged unless the entry is racily clean (no older than the index),
  and where git compares whole seconds (its Linux builds), a file changed
  to the same size in the second the index was written matches its entry:
  only that check finds it. A copy dated later (as `std::fs::copy` dates it
  on Linux) hid such a change, and git kept the old blob: a false hit here,
  and in the source-state record ([recycling](build-store.md#recycling)) a stale link, which a recycling
  test showed on a Linux runner.
  The key additionally hashes the raw bytes of every listed regular file:
  Git clean filters, including `ident` and line ending normalization, must
  not equate different compiler inputs. Gitlinks include the submodule's
  recursive current inventory rather than only its recorded commit.
  Cargo metadata selects the actual workspace (`--manifest-path`,
  toolchain and configuration included). Offline resolution with all
  features inventories local packages, including external path dependencies
  and registry/git patches; their Git roots contribute raw content too.
  That resolution needs every package of the lockfile in the Cargo home. A
  private home ([worktree Cargo homes](build-store.md#worktree-cargo-homes)) lacks optional and other-platform dependencies it
  never built, and then the manifests alone (`--no-deps`) are the
  inventory. They declare every path dependency, optional and
  platform-specific ones included. The local crates that `[patch]` and
  `[replace]` put in place are read from the root manifest and from the
  Cargo configuration. Registry and git sources are already keyed through
  the lockfile, which is in the tree (#66).
  If the inventory cannot be established, or a local source has no Git
  inventory, the command runs without result caching. Existing key markers
  use a different composition and cannot match these keys.
- **Base.** For a command that diffs against a base, the merge base too. No
  verify or test subcommand does; `exec`, which can run such a command, does
  not use the store yet.
- **Machine.** Host name, OS and architecture, because a state directory in a
  shared home must not certify another machine.
- **Toolchain.** `rustc -vV` (`RUSTC` when set), and the versions of cargo
  and of the tool the run uses (clippy, rustfmt, nextest), each asked in the
  client's working directory with its environment, so that the pinned
  toolchain answers. A missing tool is keyed as absent.
- **Cargo configuration.** The content of every `config.toml` and `config` on
  cargo's discovery path: `.cargo/` in the working directory and every
  directory above it, then the Cargo home (a relative `CARGO_HOME` against
  the working directory). They are keyed in the order cargo reads them, not
  by path, so worktrees in different places can share a key. A file that is
  there but cannot be read is keyed by its error, never as absent.
  Files explicitly selected by `--config` are keyed by content too;
  inline `KEY=VALUE` settings already contribute through argv.
  Transitive `include` files contribute their raw bytes in Cargo's order,
  including `--config 'include=[...]'`: file includes resolve relative to
  their containing file, inline includes relative to the client's cwd,
  with Cargo's lexical path normalization. String entries and tables
  (`path`, `optional`) are supported; unknown table fields are ignored as
  in Cargo. Missing optional files contribute an absence marker, so their
  creation invalidates a pass and removal restores the original inputs.
  Invalid, cyclic, unreadable required includes, or a graph exceeding 64
  files disable result caching rather than certify incomplete inputs.
  Repeated include paths count as cycles, matching Cargo 1.99. The whole
  include graph is reread after the run; keys use composition 3, excluding
  markers written before transitive configuration identity was included.
  [Pinned Cargo include loading](https://github.com/rust-lang/cargo/blob/5f94df4789f005f9a352888e8355ffc645b7ed0e/src/context/mod.rs#L1386)
- **Argv**, normalized: without the program, which the toolchain stands for,
  and with the working directory relative to the worktree, which selects
  packages and configuration as an argument would. The worktree's own path is
  not keyed.
- **Environment.** Every variable except a neutral list:
  - Names are case-folded on Windows only.
  - `PATH`, `LD_LIBRARY_PATH` and the `DYLD_*` search paths are keyed without
    their entries inside the worktree, which cargo prepends for the programs
    it runs; one left without entries counts as unset. An entry is compared
    with the worktree on Windows without case and with `\` read as `/`;
    elsewhere `\` is part of a name.
  - Neutral, and so not keyed:
    - job and thread counts holding a value the tool accepts (an invalid one
      fails the run, so it is keyed);
    - `CARGO_MAKEFLAGS`;
    - heli's own variables (`HELI_*`);
    - cargo's terminal styling (`CARGO_TERM_*`);
    - what cargo sets for the program it runs (`CARGO`, `CARGO_MANIFEST_DIR`,
      `CARGO_PKG_*` and the like);
    - locations whose content is keyed elsewhere (`CARGO_HOME`,
      `CARGO_TARGET_DIR`, `RUSTUP_*`);
    - shell bookkeeping (`PWD`, `OLDPWD`, `SHLVL`, `_`);
    - per-session identifiers (terminal and harness session ids).
  - Any variable not on the list, a test control such as `PROPTEST_CASES`
    among them, is keyed, so it misses the cache instead of reusing a result.
- **The plan**, when the command has one, rendered, including the steps it
  skips on this host. No verify or test subcommand has one yet.

**When the key is computed.** It is checked twice: before the run waits for
anything, and again once admitted and past the worktree lock, because another
run may have recorded it meanwhile. A hit before the wait is never admitted
as a run: it takes no run id; a miss goes on to single-flight with that key
([runs](runs.md), step 3). The key is computed afresh after a wait, since the runs waited
for may have changed the tree. A run that waited for nothing keeps the key
of the first check, taken a moment before, so its clients from other
worktrees attached to the tree it runs on. After the steps the whole key is
computed again, every input read anew, and compared with the one taken
before them, so a change in between (to the tree, the Cargo configuration
or the toolchain) counts as one during the run. A marker is written only
when the run passed and both keys match. A run that fails drops the
marker of its key, so a flaky pass is reused only until a run of the same key
fails.

**Caveats**, printed on every hit: a flaky test that passed once stays green
for that key (`--no-cache` runs it again), and ignored files are not part of
the key. `--cache off` skips persistent result lookups, recording and
invalidation, but shares matching checks/tests already in flight using the
same complete source/configuration/toolchain/environment key. After an edit,
the next request starts a different run; after a conflicting worktree wait,
the key is recomputed. No complete key means no sharing. This policy has its
own wire value (`live`), so older daemons refuse it instead of silently
discarding freshness. The legacy `disabled` value retains its old no-sharing
semantics for older clients. Full source inventory uses Cargo's resolver lock
even offline and can delay the first compiler. `--cache on` is the default
for checks/tests; `--cache refresh` is equivalent to `--no-cache`. Compile
commands always use source-keyed live sharing, regardless of this option;
mutate and ordinary exec commands retain command-based sharing.
`exec --verify` is verify and defaults to `off`; an explicit cache policy can
opt this declared verification into persistent verdicts. Its output and tool
arguments retain exec's behavior. Source inventory currently requires a Cargo
workspace and its complete local Git source graph; a wrapper that cannot
supply that inventory runs independently. It does not snapshot-isolate files
from concurrent editor writes.
`--no-cache` skips both lookups, not the recording, and attaches to
no run in flight ([runs](runs.md), step 3).

Compile runs are never recorded, because their product is the artifact, not
the verdict; nor is a request for help (`-h` or `--help`, also the test
runner's after `--`) of a verify or test subcommand.
