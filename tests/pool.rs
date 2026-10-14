//! The host pool (DESIGN.md §4): the daemon owns one jobserver with N slots,
//! and every run draws from it.

mod support;

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;

use support::{ConcurrencyLog, Fixture, Interval, assert_success, runs};

/// Runs `echo-args` through heli, which starts the fixture's daemon.
fn start_daemon(fixture: &Fixture) {
    let echo = fixture.echo_args();
    let output = fixture
        .heli(["exec", "--"])
        .arg(&echo)
        .output()
        .expect("run heli");
    assert_success(&output, "cargo heli exec");
}

/// `status --json` reports the pool: the slots set in the configuration,
/// all free while nothing builds. Without a daemon there is no pool.
#[test]
fn status_reports_the_pools_slots() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 3\n");
    assert!(fixture.status()["pool"].is_null(), "no daemon, no pool");

    start_daemon(&fixture);
    assert_eq!(
        fixture.status()["pool"],
        json!({"slots": 3, "free": 3, "withheld": 0, "in_use": 0})
    );
}

/// The value of `name` that `echo-args`, run through heli with `configure`,
/// saw: its `env <name>=…` line.
fn seen_by_a_run(fixture: &Fixture, name: &str, configure: impl Fn(&mut Command)) -> String {
    let echo = fixture.echo_args();
    let mut heli = fixture.heli(["exec", "--"]);
    heli.arg(&echo).env("ECHO_ENV", name);
    configure(&mut heli);
    let output = heli.output().expect("run heli");
    assert_success(&output, "cargo heli exec");
    String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_owned()
}

/// Every run gets `CARGO_MAKEFLAGS` pointing at the pool (on Windows the
/// semaphore's name, elsewhere `fifo:<path>`), in place of the client's own.
#[test]
fn a_run_draws_from_the_pool() {
    let fixture = Fixture::new("basic");
    let pool = fixture.pool().into_string().expect("a Unicode pool name");
    let auth = if cfg!(windows) {
        pool
    } else {
        format!("fifo:{pool}")
    };
    let expected = format!("env CARGO_MAKEFLAGS=Some(\"-j --jobserver-auth={auth}\")");
    assert_eq!(seen_by_a_run(&fixture, "CARGO_MAKEFLAGS", |_| {}), expected);
    let replaced = seen_by_a_run(&fixture, "CARGO_MAKEFLAGS", |heli| {
        heli.env("CARGO_MAKEFLAGS", "-j8");
    });
    assert_eq!(replaced, expected, "the client's own is replaced");
}

/// A run's environment holds `HELI_RUN`, its run id.
#[test]
fn a_run_knows_its_id() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let gate = fixture.path("gate");
    let mut heli = fixture.heli(["exec", "--"]);
    heli.arg(&echo)
        .env("ECHO_ENV", "HELI_RUN")
        .env("ECHO_WAIT_FOR", &gate)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let run = heli.spawn().expect("start heli");
    let status = fixture.wait_for_status("the run", |status| runs(status).len() == 1);
    let id = runs(&status)[0]["id"]
        .as_str()
        .expect("a run id")
        .to_owned();
    std::fs::write(&gate, "").expect("open the gate");
    let output = run.wait_with_output().expect("wait for heli");
    assert_success(&output, "cargo heli exec");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("env HELI_RUN=Some({id:?})\n")
    );
}

/// `cargo heli build --quiet` of the three `slow-*` crates in the target
/// directory `target` of its own, its build scripts logging to `log` and
/// sleeping `sleep_ms`, started in the background.
fn start_slow_build(fixture: &Fixture, target: &str, log: &ConcurrencyLog, sleep_ms: u64) -> Child {
    let mut build = fixture.heli(["build", "--quiet"]);
    build
        .args(["-p", "slow-a", "-p", "slow-b", "-p", "slow-c"])
        .env("CARGO_TARGET_DIR", fixture.path(target))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    log.attach(&mut build, sleep_ms);
    build.spawn().expect("start heli")
}

/// With N = 2, three builds started together (each in a target directory of
/// its own, so cargo does not serialize them) never run more than N + 3 jobs
/// at once: each cargo has one job of its own beyond the pool. Each also
/// starts its first build script before the others have finished.
#[test]
fn concurrent_builds_share_the_pool() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 2\n");
    start_daemon(&fixture);
    let logs: Vec<ConcurrencyLog> = (0..3)
        .map(|build| fixture.concurrency_log_named(&format!("build-{build}")))
        .collect();
    let builds: Vec<Child> = logs
        .iter()
        .enumerate()
        .map(|(build, log)| start_slow_build(&fixture, &format!("target-{build}"), log, 4000))
        .collect();
    for build in builds {
        assert_success(&build.wait_with_output().expect("wait"), "cargo heli build");
    }

    let scripts: Vec<Vec<Interval>> = logs.iter().map(ConcurrencyLog::intervals).collect();
    for (build, intervals) in scripts.iter().enumerate() {
        assert_eq!(intervals.len(), 3, "build {build}: {intervals:?}");
    }
    let most = support::max_overlap(&scripts.concat());
    assert!(
        most <= 2 + 3,
        "{most} build scripts ran at once: {scripts:?}"
    );
    for (build, intervals) in scripts.iter().enumerate() {
        let first_start = intervals.iter().map(|script| script.start).min();
        for (other, others) in scripts
            .iter()
            .enumerate()
            .filter(|(other, _)| *other != build)
        {
            let last_end = others.iter().map(|script| script.end).max();
            assert!(
                first_start < last_end,
                "build {build} waited for build {other} to finish: {scripts:?}"
            );
        }
    }
}

/// Polls `done` until it holds; panics after 60 s, naming `what` it waited
/// for.
fn wait_until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A daemon that dies during a build is replaced by one that adopts its
/// pool rather than making a second one. With N = 2 and the build's three
/// build scripts running (one on its cargo's own job, two on pool slots),
/// the new daemon's `status` agrees with the log: both slots in use. Once
/// the build is over, both come back, and no more.
#[test]
fn a_restarted_daemon_adopts_the_pool() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 2\n");
    let echo = fixture.echo_args();
    let log = fixture.concurrency_log();
    let build = start_slow_build(&fixture, "target-slow", &log, 10_000);
    wait_until("three build scripts at once", || log.running() == 3);

    let daemon = fixture.daemon_pid().expect("a daemon");
    support::kill_process(daemon);
    fixture.wait_for_status("the daemon to be gone", |status| status["daemon"].is_null());
    let output = fixture
        .heli(["exec", "--"])
        .arg(&echo)
        .output()
        .expect("run heli");
    assert_success(&output, "a call that starts a new daemon");
    let status = fixture.status();
    let still_running = log.running();
    assert_ne!(support::daemon_pid(&status), Some(daemon), "a new daemon");
    assert_eq!(
        still_running, 3,
        "the scripts ran on as the status was taken"
    );
    assert_eq!(
        status["pool"],
        json!({"slots": 2, "free": 0, "withheld": 0, "in_use": 2}),
        "{still_running} scripts hold the pool's 2 slots"
    );

    // The build's client lost its daemon; its cargo builds on.
    let _ = build.wait_with_output().expect("wait for the client");
    let rlibs = ["slow_a", "slow_b", "slow_c"].map(|name| {
        fixture
            .path("target-slow")
            .join("debug")
            .join(format!("lib{name}.rlib"))
    });
    wait_until("the build to finish", || {
        rlibs.iter().all(|rlib| rlib.is_file())
    });
    let status = fixture.wait_for_status("the slots to come back", |status| {
        status["pool"]["in_use"] == 0
    });
    assert_eq!(
        status["pool"],
        json!({"slots": 2, "free": 2, "withheld": 0, "in_use": 0})
    );
    assert_eq!(pool_openings(&fixture), ["created", "adopted"]);
}

/// How each daemon of `fixture` found its pool, as its log says: `created`
/// or `adopted`, oldest first.
fn pool_openings(fixture: &Fixture) -> Vec<&'static str> {
    daemon_log(fixture)
        .lines()
        .filter(|line| line.contains("] pool "))
        .map(|line| {
            if line.contains(": created, ") {
                "created"
            } else if line.contains(": adopted, ") {
                "adopted"
            } else {
                panic!("a pool line that says neither: {line}")
            }
        })
        .collect()
}

/// A pool that another process holds, with one slot taken, as a cargo would
/// hold it.
struct Holder {
    #[cfg(windows)]
    semaphore: std::os::windows::io::OwnedHandle,
    #[cfg(unix)]
    fifo: std::fs::File,
}

impl Holder {
    /// Opens the pool `name` as cargo's jobserver does, and takes a slot.
    #[cfg(windows)]
    fn take(name: &std::ffi::OsStr) -> Self {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::io::FromRawHandle;

        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::{
            OpenSemaphoreW, SEMAPHORE_MODIFY_STATE, SYNCHRONIZATION_SYNCHRONIZE,
            WaitForSingleObject,
        };

        let wide: Vec<u16> = name.encode_wide().chain([0]).collect();
        // SAFETY: `wide` is NUL-terminated; the handle is owned below.
        let handle = unsafe {
            OpenSemaphoreW(
                SYNCHRONIZATION_SYNCHRONIZE | SEMAPHORE_MODIFY_STATE,
                0,
                wide.as_ptr(),
            )
        };
        assert!(!handle.is_null(), "open the semaphore");
        // SAFETY: a semaphore handle just opened.
        let taken = unsafe { WaitForSingleObject(handle, 0) };
        assert_eq!(taken, WAIT_OBJECT_0, "take a slot");
        Self {
            // SAFETY: the handle is this holder's from here on.
            semaphore: unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle) },
        }
    }

    /// Opens the pool `name` as cargo's jobserver does, and takes a slot.
    #[cfg(unix)]
    fn take(name: &std::ffi::OsStr) -> Self {
        use std::io::Read;
        let mut fifo = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(name)
            .expect("open the FIFO");
        fifo.read_exact(&mut [0u8]).expect("take a slot");
        Self { fifo }
    }

    /// Gives the slot back.
    #[cfg(windows)]
    fn give_back(&self) {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: a semaphore handle with SEMAPHORE_MODIFY_STATE.
        let released = unsafe {
            windows_sys::Win32::System::Threading::ReleaseSemaphore(
                self.semaphore.as_raw_handle(),
                1,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(released, 0, "give the slot back");
    }

    /// Gives the slot back.
    #[cfg(unix)]
    fn give_back(&self) {
        use std::io::Write;
        (&self.fifo).write_all(b"|").expect("give the slot back");
    }
}

/// A pool that some process still holds, with a slot taken, is adopted as it
/// is by the next daemon: the slot is in use, and comes back to the same
/// pool. On Windows the semaphore also keeps the size it was made with,
/// whatever the configuration now says.
#[test]
fn a_held_pool_is_adopted_as_it_is() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 2\n");
    // The holder is no process the top-up's scan sees; with the governor's
    // clock standing still, no top-up counts its slot as lost.
    fixture.set_clock(0);
    start_daemon(&fixture);
    let holder = Holder::take(&fixture.pool());
    let daemon = fixture.daemon_pid().expect("a daemon");
    support::kill_process(daemon);
    fixture.wait_for_status("the daemon to be gone", |status| status["daemon"].is_null());
    if cfg!(windows) {
        fixture.configure("[pool]\nslots = 5\n");
    }

    start_daemon(&fixture);
    assert_eq!(
        fixture.status()["pool"],
        json!({"slots": 2, "free": 1, "withheld": 0, "in_use": 1})
    );
    holder.give_back();
    assert_eq!(
        fixture.status()["pool"],
        json!({"slots": 2, "free": 2, "withheld": 0, "in_use": 0})
    );
    assert_eq!(pool_openings(&fixture), ["created", "adopted"]);
}

/// A daemon killed while nothing builds leaves a pool that nobody holds: the
/// next one finds it with every slot free (a FIFO that nobody held open has
/// lost its bytes, and is filled again).
#[test]
fn a_daemon_after_an_idle_one_has_every_slot() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 3\n");
    start_daemon(&fixture);
    let daemon = fixture.daemon_pid().expect("a daemon");
    support::kill_process(daemon);
    fixture.wait_for_status("the daemon to be gone", |status| status["daemon"].is_null());
    start_daemon(&fixture);
    let status = fixture.status();
    assert_ne!(support::daemon_pid(&status), Some(daemon), "a new daemon");
    assert_eq!(
        status["pool"],
        json!({"slots": 3, "free": 3, "withheld": 0, "in_use": 0})
    );
    assert_eq!(pool_openings(&fixture), ["created", "created"]);
}

/// Runs `echo-args` through `heli`, whose daemon cannot start, and returns
/// that daemon's log: the command ran directly, after one warning.
fn refused(fixture: &Fixture, heli: &mut Command) -> String {
    let echo = fixture.echo_args();
    let output = heli
        .args(["exec", "--"])
        .arg(&echo)
        .output()
        .expect("run heli");
    assert_success(&output, "cargo heli exec");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with("cargo-heli: warning: the heli daemon exited with code 1"),
        "{stderr}"
    );
    assert!(stderr.ends_with("echo-args: stderr\n"), "{stderr}");
    daemon_log(fixture)
}

/// A pool size the daemon cannot use keeps it from starting: the command
/// runs directly after one warning, and the daemon's log says why.
#[test]
fn an_unusable_pool_size_is_refused() {
    for slots in ["0", "5000", "\"many\""] {
        let fixture = Fixture::new("basic");
        fixture.configure(&format!("[pool]\nslots = {slots}\n"));
        let log = refused(&fixture, &mut fixture.heli(Vec::<String>::new()));
        assert!(log.contains("slots"), "slots = {slots}: {log}");
    }
}

/// A pool that `CARGO_MAKEFLAGS` cannot carry (the value ends at the first
/// space, and cargo opens a semaphore by an ASCII name) is refused too.
#[test]
fn a_pool_cargo_cannot_name_is_refused() {
    let cases: &[&str] = if cfg!(windows) {
        &["heli test", "heli-tést"]
    } else {
        &["job server"]
    };
    for case in cases {
        let fixture = Fixture::new("basic");
        let name = if cfg!(windows) {
            (*case).into()
        } else {
            fixture.path(case).into_os_string()
        };
        let mut heli = fixture.heli(Vec::<String>::new());
        heli.env("HELI_POOL", &name);
        let log = refused(&fixture, &mut heli);
        assert!(log.contains("cannot open the pool"), "{name:?}: {log}");
    }
}

/// A relative `HELI_CONFIG` is relative to the caller's directory, for the
/// daemon too.
#[test]
fn a_relative_configuration_file_is_the_callers() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    std::fs::write(fixture.workspace().join("heli.toml"), "[pool]\nslots = 3\n")
        .expect("write the configuration");
    let output = fixture
        .heli(["exec", "--"])
        .arg(&echo)
        .env("HELI_CONFIG", "heli.toml")
        .output()
        .expect("run heli");
    assert_success(&output, "cargo heli exec");
    assert_eq!(fixture.status()["pool"]["slots"], 3);
}

/// The daemon's own log, whatever its session suffix.
fn daemon_log(fixture: &Fixture) -> String {
    std::fs::read_dir(fixture.state_dir())
        .expect("the state directory")
        .map(|entry| entry.expect("an entry").path())
        .filter(|path| {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            name.starts_with("daemon") && name.ends_with(".log")
        })
        .map(|path| std::fs::read_to_string(path).expect("read the daemon's log"))
        .collect()
}

/// `cargo heli status` prints the pool on a line of its own.
#[test]
fn status_prints_the_pool_for_people() {
    let fixture = Fixture::new("basic");
    fixture.configure("[pool]\nslots = 3\n");
    start_daemon(&fixture);
    let output = fixture.heli(["status"]).output().expect("run heli status");
    assert_success(&output, "cargo heli status");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.lines()
            .any(|line| line == "pool    3 slots: 0 in use, 3 free, 0 withheld"),
        "{text}"
    );
}

/// Without `HELI_POOL`, the pool belongs to the user's state directory: on
/// Windows the semaphore `heli-<hash>`, one per state directory; elsewhere
/// the FIFO `~/.cache/heli/jobserver` (without `XDG_RUNTIME_DIR`), mode 0600
/// in a directory of mode 0700.
#[test]
fn the_default_pool_is_the_users() {
    // Built with the real home: cargo and rustup live there.
    let builder = Fixture::new("basic");
    let echo = builder.echo_args();
    let makeflags = |fixture: &Fixture| {
        let output = fixture
            .heli(["exec", "--"])
            .arg(&echo)
            .env("ECHO_ENV", "CARGO_MAKEFLAGS")
            .output()
            .expect("run heli");
        assert_success(&output, "cargo heli exec");
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.trim_end()
            .strip_prefix("env CARGO_MAKEFLAGS=Some(\"-j --jobserver-auth=")
            .and_then(|rest| rest.strip_suffix("\")"))
            .unwrap_or_else(|| panic!("no jobserver: {text}"))
            .to_owned()
    };
    let homes = [
        Fixture::with_home("basic", false),
        Fixture::with_home("basic", false),
    ];
    let pools: Vec<String> = homes.iter().map(makeflags).collect();
    if cfg!(windows) {
        for pool in &pools {
            let hash = pool.strip_prefix("heli-").unwrap_or_default();
            assert!(
                hash.len() == 16 && hash.chars().all(|c| c.is_ascii_hexdigit()),
                "{pool}"
            );
        }
        assert_ne!(pools[0], pools[1], "one pool per state directory");
    }
    #[cfg(unix)]
    for (home, pool) in homes.iter().zip(&pools) {
        use std::os::unix::fs::{FileTypeExt, PermissionsExt};
        let fifo = home.home().join(".cache").join("heli").join("jobserver");
        assert_eq!(pool, &format!("fifo:{}", fifo.display()));
        let metadata = std::fs::metadata(&fifo).expect("the FIFO exists");
        assert!(metadata.file_type().is_fifo(), "{}", fifo.display());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600, "the FIFO");
        let dir = fifo.parent().expect("a directory");
        let mode = std::fs::metadata(dir)
            .expect("the directory")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "{}", dir.display());
    }
}

/// Without a configuration file, the pool has one slot per logical CPU.
#[test]
fn the_pool_has_a_slot_per_logical_cpu_by_default() {
    let fixture = Fixture::new("basic");
    start_daemon(&fixture);
    let cpus = std::thread::available_parallelism().expect("the CPU count");
    let pool = &fixture.status()["pool"];
    assert_eq!(pool["slots"], cpus.get(), "{pool}");
    assert_eq!(pool["free"], cpus.get(), "{pool}");
}
