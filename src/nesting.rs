//! A backstop against heli running itself in a loop.
//!
//! Every command heli runs carries [`VAR`], one more than heli's own depth,
//! and heli refuses to start nested deeper than [`MAX`]. Legitimate nesting
//! (`cargo heli exec -- cargo xtask …` whose task runs `cargo heli nextest`)
//! stays a few levels deep; a loop, such as a subcommand that resolves back to
//! heli, reaches the limit after [`MAX`] processes instead of exhausting the
//! machine.

/// The variable that carries the depth.
pub const VAR: &str = "HELI_DEPTH";

/// The deepest heli that still runs its command.
pub const MAX: u32 = 16;

/// This process's depth: [`VAR`] as heli's parent set it, 0 when absent or
/// unreadable.
pub fn current() -> u32 {
    std::env::var(VAR)
        .ok()
        .and_then(|depth| depth.trim().parse().ok())
        .unwrap_or(0)
}
