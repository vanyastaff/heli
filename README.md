# heli

Host scheduling for Cargo on machines shared by multiple agents. Each agent
works in its own Git worktree; heli shares compiler capacity while keeping
worktrees' mutable dependency caches separate when isolation is selected.

Build from source with Rust/Cargo 1.99 or newer:

```text
cargo install --path . --locked
cargo heli ensure
```

In each agent's worktree, keep Cargo's checkout-local `target/` and run:

```text
cargo heli --cargo-home isolated --cache off check --workspace --all-targets --locked
cargo heli --cargo-home isolated --cache off clippy --workspace --all-targets --locked
cargo heli --cargo-home isolated --cache off test --workspace --locked
```

To select private Cargo homes for every managed command, set this in heli's
user configuration (`%APPDATA%\heli\config.toml` on Windows,
`$XDG_CONFIG_HOME/heli/config.toml` or `~/.config/heli/config.toml` on Unix):

```toml
[cargo]
home = "isolated"
```

Each worktree gets a persistent native Cargo home automatically. Cargo
downloads dependencies normally; no vendor/import/fetch prepass is needed.
Nested commands keep that home. A cold private home needs network access;
another worktree's warm cache does not make it ready for `--offline`.

Use `cargo heli status` to see active work and `cargo heli log <run-id>` for
recorded output. Wrap project task runners with
`cargo heli exec -- <command> [args...]`; a runner's own host-wide lock still
needs integration. Sharing a single `CARGO_TARGET_DIR` across worktrees still
causes Cargo's artifact lock to serialize them.

Isolation does not promise a general speedup over correctly isolated native
Cargo. It automates the separate mutable homes that avoid a reproduced
cross-worktree package-cache blocking case. Global application resources such
as fixed ports and desktop windows remain the project's responsibility.

Artifact GC is opt-in, protects active managed commands and resumes at idle.
It is not a hard disk quota; private native Cargo homes are retained outside
the artifact budget. See [the operating guide](docs/multi-agent.md) for store
placement, budgets, credentials, fallback and supported-tool limits.

The contracts and dated acceptance evidence are in [DESIGN.md](DESIGN.md).

## Cargo tools

Start with `cargo heli doctor` (or `--json`). It lists your installed
`cargo-*` tools with what heli does with each. It also explains how your
Cargo configuration interacts with heli: `jobs`, a global `target-dir`,
your own `build-dir`, an sccache wrapper, mold or lld, rust-analyzer and a
broken advisory database. Finally it shows the tools without a profile that
you run most.

heli ships built-in profiles for popular tools, so they work without
configuration:

- **Tools that rewrite the worktree** take its lock exclusively, so they
  never run under a build: `cargo add`/`remove`/`update`, `upgrade`,
  `set-version`, `release`, `sort`, `hakari generate`, `machete --fix`,
  `shear --fix`, `insta accept`, `sqlx prepare`, `sweep`, `clean-all`.
- **Read-only checks** are cached and shared like `check` while nothing
  changed: `hack check`, `machete`, `shear`, `sort --check`,
  `hakari verify`, `udeps`, `public-api`, `miri test`. `audit` and
  `deny check` are cached too: heli fetches the advisory database first and
  keys the check by it, so a new advisory is never hidden behind a cached
  pass.
- **Interactive and long-running tools** run in your terminal while still
  drawing from the shared pool: `insta review`, `cargo watch`, `dx serve`,
  `trunk serve`, `leptos watch|serve`, `tauri dev`. At a terminal, so does
  any tool heli has no profile for.
- **A toolchain** given as `cargo heli +nightly <tool>` is honoured, and is
  part of the cache key.

To describe your own tools, add profiles in heli's user configuration, or
commit them for everyone in your repository's `.config/heli.toml`:

```toml
[[profiles.tool]]
tool = "xtask"
args = ["check-changed"]
class = "verify"     # compile, verify, mutate or exec
```

A repository's profiles can only make a tool stricter (take the lock, stop
caching), unless you set `trust_repository = true` under `[profiles]` in
your own configuration. Machine settings stay in your configuration:

```toml
[advisories]
fetch_interval_minutes = 10   # reuse audit/deny passes without the network
```

`cargo heli status` shows each run's class and the profile that gave it.

## Contributing

The full test suite requires `cargo-nextest` in addition to Rust/Cargo.
Install it once and verify Cargo can find it in your test environment:

```text
cargo install cargo-nextest --locked
cargo nextest --version
cargo test --locked --no-fail-fast
```

Tests that exercise real nextest runs fail immediately with installation
guidance and Cargo's original diagnostic if that probe fails; they never
silently skip. Argument-only tests using a stand-in Cargo need no nextest.
CI runs the complete suite and `cargo doc --no-deps --locked` with
`RUSTDOCFLAGS="-D warnings"` on Windows, Linux and macOS.
It also checks all targets with the minimum supported Rust, pinned to 1.99.0,
on each OS; `Cargo.toml` declares `rust-version = "1.99"`.

A separate workflow runs popular cargo tools through heli on the three OSes:
cargo-hack, machete, shear, deny, audit, llvm-cov, mutants, insta and
hakari, and in a nightly job udeps, public-api and Miri. It runs weekly and
on changes to classes, profiles, the client, the daemon or the store. With
the tools installed, run it locally; the nightly tests take their toolchain
from `HELI_NIGHTLY` (default `nightly`):

```text
cargo test --locked --test tool_compatibility -- --ignored --skip nightly_
cargo test --locked --test tool_compatibility -- --ignored nightly_
```

Windows native path acceptance requires installed Visual Studio C++ tools.
The test locates `vswhere.exe` through environment roots, Windows Known
Folders and PATH, then tries `cl.exe` on PATH. Omitting `ProgramFiles` in a
shell does not hide a normal Visual Studio installation. Missing tools
remain a test failure with discovery details, never a silent skip.
