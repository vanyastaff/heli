# The daemon

The daemon runs commands as its own user, in the client's working directory
and with the client's environment. Its IPC endpoint (a named pipe with an
owner-only ACL, or a socket with mode 0600) accepts only that user.

The daemon must run in the user's interactive session, never as a service:
runs inherit its session, and tests that drive real windows (flui's live
Win32 tests) need the user's desktop. It is started on demand by a client, or
by the logon registration `cargo heli setup --autostart` makes: a task that
runs only while the user is logged on, or its systemd and launchd
counterparts ([host pool](host-pool.md), "Autostart"). Once that registration exists, clients start
the daemon through it too (below).

**One daemon per user**, and on Windows per logon session: a lock in the
per-user state directory (`%LOCALAPPDATA%\heli`, `$XDG_STATE_HOME/heli` or
`~/.local/state/heli`) admits one. Named pipes are visible machine-wide,
unlike semaphores, so the Windows endpoint carries the session:
`\\.\pipe\heli-<hash of the state directory>-<session id>`. `HELI_STATE_DIR`
and `HELI_ENDPOINT` override both, and `HELI_POOL` and `HELI_CONFIG` the host
pool ([host pool](host-pool.md)) and the configuration file ([runs](runs.md)), which is how tests isolate their
daemons. A client resolves them, relative paths against its own working
directory, and passes them to the daemon it starts. `HELI_SCHTASKS`,
`HELI_SYSTEMCTL` and `HELI_LAUNCHCTL` name the programs heli runs as
`schtasks`, `systemctl` and `launchctl` ([host pool](host-pool.md), "Autostart"); tests point them
at a fake, so that none registers anything with the real ones.

On Windows, a process an MSIX-packaged app starts sees `%LOCALAPPDATA%`
and `%APPDATA%` through package virtualization: the default state
directory and configuration are then the app's own, under
`...\Packages\<family>\LocalCache\`. Such a child need not have the
package's identity (`GetCurrentPackageFullName` can say it has none). The
path the file system gives for the file is the test, which `doctor` and
`ensure` run: a canonical path in a package's `LocalCache` that the spelled
one is not. Both then say to set `HELI_STATE_DIR` and `HELI_CONFIG` outside
`AppData` for every process (#70).

**The endpoint.** On Windows, a pipe whose DACL allows the user's SID alone
(without one, Everyone may read), which rejects remote clients and is
created as the first instance, so the daemon never joins a pipe someone else
created. The daemon sets its owner to the user's SID, elevated or not (an
elevated process would otherwise give it to the Administrators group). A
client connects at identification level and talks only to a pipe owned by
its own user's SID, elevated or not: never to one owned by the
Administrators group, which any administrator's elevated process can
create. On Unix, a socket with mode 0600 in the 0700 state directory; a
client talks only to a socket owned by its uid.

**The protocol** starts with a handshake frozen across versions: `HELI` and
the protocol version, each way. On a mismatch the client runs the command
directly, with one warning line. A client newer than the daemon also asks it
to retire: the daemon takes no new runs and exits once its runs in flight
have finished, so the next call starts a current daemon. As it exits it
first stops listening and releases the state directory's lock: the process
itself may take a while to be gone (seen to take over 10 s on a loaded
Windows host), and until then it would hold its endpoint, where a client
would connect and get no answer, and the lock, which a new daemon needs.
An older client asks
nothing, so two installed versions never take turns retiring each other's
daemon. The current protocol is 2: a dependency-epoch request must never be
accepted by an old daemon that ignores its optional ownership field. A daemon
answers a request it does not know (one a newer client added, such as `log`)
with "unsupported", and the client treats that like a mismatch from an older
daemon. It asks the daemon to retire and does without it, after one warning
line: it runs the command, or reads the run log, itself. Fields added to a
message are optional, so both sides still read each other's: a run request
without a class ([runs](runs.md)) comes from a client older than run classes, its run
counts as exec, and the daemon does not send it the "waiting" message, which
it could not read. Values added to a set are read as unknown: a run's phase
or class in `status` that the client does not know shows as `unknown` and
as no class, and a reply it does not know is skipped while it follows a run
(elsewhere it is an answer it did not expect). A daemon still sends a new
reply only to a client whose request says it reads it.

**Upgrading.** `cargo heli self-upgrade [--now [--yes]] [--to <version> |
--path <dir>]` installs the new version into `<state directory>/upgrade`
(`cargo install cargo-heli --locked --force --root …`, with
`CARGO_MAKEFLAGS` unset so that the build does not wait on a saturated
pool), moves it into the running program's place, and runs `restart` with
the new program. `cargo install` alone cannot do the middle step while
anything runs the program: on Windows a mapped image can be neither
overwritten nor deleted, and the daemon and every client waiting on a run
are such images (the install then leaves `cargo-heli <version> = []` in the
Cargo home's `.crates.toml`, and no program). A mapped image can be
renamed, so the old program goes aside as `<name>.old-<version>` beside the
new one, and the next upgrade deletes it once nothing runs it. Elsewhere
the new file is renamed over the old one, and the processes running it keep
their inode. `doctor` says when the daemon is older than the client
(`status` does too), when `.crates.toml` records such a failed install, and
when an old program is still beside the new one. The Cargo home's own
records are cargo's: `cargo install --list` keeps naming the version cargo
installed there.

**Starting on demand.** When no daemon answers, the client starts one, and
any client falls back to running the command directly when the daemon does
not start, listen or answer within 5 s.

- The client starts `cargo heli daemon --detach`, which starts the daemon and
  exits once it listens. The daemon's parent is then gone, so no tree walk
  from the client reaches it; the client goes on only once that stage has
  exited, within the same 5 s. Agent harnesses stop a command by killing its
  process tree found through parent processes: observed with Claude Code on
  Windows, where stopping a command killed the client, the daemon it had
  started directly, and that daemon's run.
- A daemon that finds the lock held exits at once (code 3). The holder may
  be starting, and will listen; or going away, killed or retired, and
  holding the lock until its process is gone, after its endpoint has gone.
  So while nothing listens, `daemon --detach` starts the daemon again, after
  a pause that grows from 50 ms to 400 ms, within the same 5 s. A daemon
  that finds its pipe already there on Windows (a predecessor's last
  connections, as its process goes) tries again to create it within 5 s too.
- An endpoint that is there but busy (on Windows every instance of the pipe
  taken: between two of the daemon's accepts, or on a host so loaded that
  the daemon has not run since) is a daemon, not an absent one: the client
  waits for a free instance and tries again, within the same 5 s as for an
  answer, and only then runs the command directly. The daemon creates the
  next instance before it serves the client it accepted, and creates its
  pipe only once it is ready to accept.
- It inherits no handle from the client (on Windows it is started with
  `CreateProcessW` and no inheritance, since the standard library lets a
  child inherit every inheritable handle). A daemon holding the client's
  stdout would keep an agent's tool call waiting until the daemon exits.
- It runs in the state directory, so it keeps no worktree busy, and leads a
  process group (Unix) or has a console of its own without a window
  (Windows), so the client's Ctrl-C does not reach it.
- On Windows it leaves the client's job object when the job allows breakaway.
  Claude Code's does not: its one job holds the whole app and every command
  it runs, with no kill-on-close limit, so a daemon started from an agent's
  command stays in that job and outlives the command. A harness that
  terminated that job as a whole would take the daemon and every run with
  it; only a daemon started outside the job, by the logon task, avoids that
  ([failure modes](failure-modes.md)). Hence the next point.
- **Through the registration.** When `setup --autostart` has registered the
  daemon for the client's own program and locations (its record in the state
  directory says which), the client does not start the daemon itself: it asks
  the service manager to (`schtasks /Run /TN <task>`,
  `systemctl --user start heli.service`,
  `launchctl kickstart gui/<uid>/<label>`), with no stdio, and waits for the
  daemon to listen within the same 5 s. That daemon belongs to the service
  manager, outside any agent's job, process tree or control group. A client
  that asked never also starts one: if none listens in time, it runs the
  command directly. When there is no such record (no registration, another
  program such as a second install, other locations), or the service manager
  refuses or does not answer within 5 s (the registration was removed behind
  heli's back), the client starts the daemon itself, as above.

**A run** is a child of the daemon, started in the client's working
directory with exactly the client's environment and no stdin, as the first
process of a process tree of its own ([runs](runs.md), lifecycle step 4). Its stdout and
stderr stream back to the client through the run's record, and the client
exits with the run's code (on Unix, of its signal). The run belongs to the
daemon: a client that goes away does not stop it. A run is over when its
process exits: output still arriving within 500 ms is forwarded, and
processes it left behind holding its pipes do not hold the client. The
daemon tells the client when it accepts the run, and again when the run's
command has started (its first process, after any wait for the worktree
lock or for recycling, [runs](runs.md)): a client that loses the daemon in between runs
the command itself, after one warning line, since the command never
started. Once the daemon has said the command started, the client never
runs it itself: if the daemon is lost then, it exits 1. A client asks for
the second message in its run request, and a
daemon older than it says in its first that none follows: with one, the
run counts as started once accepted, as before.
