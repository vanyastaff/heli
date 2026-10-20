# Observability

`cargo heli status`:

```
pool   32 slots: 21 compiling (4 runs), 6 testing (1 run), 0 withheld   memory 41.2/64 GB, commit 52 %
runs   r17  agent-a64b...  nextest run -p my-crate     test     6 slots  4m12s
       r18  agent-f1c0...  clippy --all-targets        verify   9 slots  1m03s
       r19  agent-a10...   check -p other-crate        attached to r12 (same key)
       r20  agent-a64b...  fmt                         mutate   waiting for r17  12s
disk   D:\.heli\build  148/150 GB (incremental 61, deps 52, ...)  next: incremental >24h, ~40 GB
```

`cargo heli status --json` has a versioned schema (`schema`, now 1). Times
are whole seconds since 1970 (UTC); `status` never starts a daemon.

- `daemon`: `pid`, `protocol` and `started_at`, and where it runs, so that
  a user can see it runs in their interactive session ([daemon](daemon.md)): `session` (on
  Windows the logon session, elsewhere `getsid`), and on Windows
  `window_station` and `desktop` (`WinSta0` and `Default` for the user's
  desktop) and `interactive` (the window station `WinSta0` of a session
  other than 0; `null` elsewhere, where heli cannot tell). The text form
  says the same on the daemon's line:
  `daemon  pid 4242, protocol 1, session 1, WinSta0\Default (interactive), up 52s`.
  `null` when none answers;
- `pool`: `slots`, `free`, `withheld`, `in_use` (`slots − free − withheld`,
  never below 0), or `null` when no daemon answers;
- `memory`: the governor's last sample, in bytes: `total`, `available`,
  plus `commit_available` on Windows, and the `low_watermark` and
  `high_watermark`; `null` when no daemon answers or memory cannot be read;
- `runs`: one entry per run, with `id`, `worktree` (the nearest directory
  holding `.git`, else the working directory), `argv`, `class`, `priority`
  (`interactive`, `normal` or `background`, [host pool](host-pool.md); the text form shows it when
  not `normal`; `background` also for a run demoted for having no client,
  [runs](runs.md) "Detach"), `phase`
  (running for a run of one phase; building, testing, done; `unknown` for
  a newer daemon's phase the client does not know), `slots` (the
  host pool's slots it uses: lent to its run pool and not free there, plus
  its test slots), `share` (its share of the capacity, [host pool](host-pool.md), a number that
  need not be whole), `waiting_since` (`null` unless the run waits),
  `waiting_for`
  (the ids of the runs it waits for at the worktree lock),
  `attached_clients` (the clients following it now), `detached_since`
  (since when no client follows a run in flight; `null` while one does;
  the text form says `no client for 72s` in place of `0 attached`,
  [runs](runs.md) "Detach"), `rerun_of` (the run a
  rerun reruns, [runs](runs.md) "Reruns"; `null` for any other run, and the text form
  adds `rerun of r17` to the run's state), `started_at`, and
  once it has ended `ended_at` and `exit` (`code`, `signal` on Unix,
  `cancelled`, and `detached` when the daemon cancelled it for having no
  client, which the text form shows as `cancelled (no client)`), else
  `null`. The runs in flight are listed, and the last
  32 runs that ended while no client followed them, as done: nobody else
  was told how they ended. A run whose clients heard its end leaves the
  list; a done run holds no worktree lock and counts as no run in flight
  (for the top-up's quiescence, [host pool](host-pool.md));
- `disk`: `stores`, one entry per store, with `path`, `budget` (B, which
  every store and registered `target/` directory share, in bytes; `null`
  without one), `used` and `by_category` (`incremental`, `deps`,
  `build_scripts`, `fingerprints`, `tool_caches`, `other`, in bytes); and
  `targets`, one entry per registered `target/` directory, with `path`,
  `used` and `by_category`. `null` when no daemon answers;
- `last_gc`: the daemon's last eviction pass that evicted anything ([eviction](build-store.md#eviction)):
  `at`, and `evicted`, one entry per victim with its `path`, `tier` and
  `bytes`; `null` before the first, and when no daemon answers;
- `unmet_budget`: the budget the daemon's passes cannot meet, while the
  busy directories alone hold more than its target ([eviction](build-store.md#eviction)):
  `budget`, `used`, `busy_dirs`, `busy_bytes` (bytes) and `since`;
  `null` while the budget can be met, and when no daemon answers. The
  text form says it on a `budget` line:
  `budget  100.0 GiB cannot be met for 3600s: 18 busy directories hold 200.0 GiB, more than 85 % of it; live directories are kept whole`.

There is also a rotating log of admissions, loans, withholdings, top-ups,
drains and evictions, and `cargo heli gc --dry-run`.

**The event log** is `events.log` in the state directory
(`events-<session>.log` on Windows), one JSON object per line, each with `at`
(seconds since 1970, UTC) and `event`:

- `admit`: `run`, `worktree`, `argv`, `class` and `profile` (where the class
  came from: `cargo`, `built-in`, `user`, `repository`, `unknown` or `exec`;
  [runs](runs.md), "Tool profiles");
- `direct`, written by the client for a run it makes itself in the pool:
  `worktree`, `argv` and `profile`;
- `withhold`: `withheld` (after it), `available`, `commit_available` and
  `low_watermark`;
- `return`: `slots`, `available`, `commit_available` and `high_watermark`;
- `top-up` and `drain`: `slots`, `free` (after it) and `target`;
- `loans`, when a run ends: `run`, `loans` (slots lent to its run pool),
  `taken_back` (slots the broker took back while it had work pending),
  `most` (the most slots it was seen using), `wait_median_ms` and
  `wait_longest_ms` (how long it waited for a lent slot, at most; `null`
  without loans);
- `recycle`, when a worktree got an orphan's build directory ([recycling](build-store.md#recycling)):
  `trigger` (`run`, `hook` or `look`), `workspace` and `to` (the new
  workspace, and the build directory's path now), `donor` and `from` (the
  removed workspace, and the path the directory had), `invalidated`
  (the units whose fingerprints the path scan deleted), `mode` (`precise`
  or `conservative`) and `restored` (for precise recycling, the files that
  got their recorded times, else `null`). The hook writes it itself, to the
  log of its logon session's daemon;
- `evict`, for each victim an eviction pass of the daemon's or of `gc`
  renamed into the trash ([eviction](build-store.md#eviction)): `tier`, `path`, `bytes` and `reason`.

Past `[event_log] max_kb` (default 1024) it becomes `<name>.1`, replacing
the one before, and the next event starts a new file.
