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
