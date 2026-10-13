//! The run log: the daemon records each run's output, the footer names the
//! run, and `cargo heli log <run>` prints the record, whole or in part.

mod support;

use std::io::{Read, Write};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use support::fake_daemon::{Answer, FakeDaemon};
use support::{Fixture, runs};

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Starts `command` with its output captured.
fn start(command: &mut Command) -> Child {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start heli")
}

fn finish(child: Child) -> Output {
    child.wait_with_output().expect("wait for heli")
}

fn open(gate: &std::path::Path) {
    std::fs::write(gate, "").expect("open the gate");
}

/// After a run, heli prints one footer line on stderr: the run's id, its
/// exit code and duration, and the command that shows its output. stdout
/// stays the tool's alone.
#[test]
fn the_footer_names_the_run() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let gate = fixture.path("gate");
    let mut call = fixture.heli(["exec", "--"]);
    call.arg(&echo)
        .arg("x")
        .env("ECHO_WAIT_FOR", &gate)
        .env("ECHO_EXIT", "3");
    let call = start(&mut call);
    let status = fixture.wait_for_status("the run", |status| runs(status).len() == 1);
    let id = runs(&status)[0]["id"].as_str().expect("an id").to_owned();
    std::thread::sleep(Duration::from_secs(1));
    open(&gate);
    let output = finish(call);

    assert_eq!(output.status.code(), Some(3));
    assert_eq!(text(&output.stdout), "arg: \"x\"\n");
    let (stderr, footer) = support::split_footer(&output.stderr);
    assert_eq!(text(&stderr), "echo-args: stderr\n");
    assert_eq!(footer.run, id);
    let seconds: f64 = footer
        .line
        .strip_prefix(&format!("cargo-heli: {id} exit 3 in "))
        .and_then(|rest| rest.strip_suffix(&format!(" s (output: cargo heli log {id})")))
        .and_then(|seconds| seconds.parse().ok())
        .unwrap_or_else(|| panic!("footer {:?}", footer.line));
    assert!((0.9..60.0).contains(&seconds), "{}", footer.line);
}

/// `chatter` with `args`, run by heli: its output and the run the footer
/// names, after checking that heli passed the output through unchanged.
fn chatter_run(fixture: &Fixture, args: &[&str]) -> (Output, String) {
    let chatter = fixture.binary("chatter");
    let direct = fixture
        .command(&chatter, args)
        .output()
        .expect("run chatter");
    let output = fixture
        .heli(["exec", "--"])
        .arg(&chatter)
        .args(args)
        .output()
        .expect("run heli");
    support::assert_success(&output, "cargo heli exec -- chatter");
    let (stderr, footer) = support::split_footer(&output.stderr);
    assert!(output.stdout == direct.stdout, "heli passed stdout through");
    // The footer starts a line of its own.
    let mut expected = direct.stderr.clone();
    if expected.last().is_some_and(|&byte| byte != b'\n') {
        expected.push(b'\n');
    }
    assert!(
        stderr == expected,
        "heli passed stderr through: {:?}",
        text(&output.stderr)
    );
    (direct, footer.run)
}

/// `cargo heli log` with `args`.
fn log(fixture: &Fixture, args: &[&str]) -> Output {
    fixture
        .heli(["log"])
        .args(args)
        .output()
        .expect("run heli log")
}

/// `log <run>` prints what the run wrote, byte for byte: stdout on stdout,
/// stderr on stderr, including bytes that are not UTF-8, `\r\n` line ends,
/// a line longer than the daemon's reads, and last lines without an end.
#[test]
fn log_reproduces_the_output_byte_for_byte() {
    let fixture = Fixture::new("basic");
    let (direct, run) = chatter_run(&fixture, &["40", "mix", "unended"]);
    let logged = log(&fixture, &[&run]);
    support::assert_success(&logged, "cargo heli log");
    assert!(direct.stdout.len() > 150_000, "a long stdout");
    assert!(logged.stdout == direct.stdout, "stdout differs");
    assert!(logged.stderr == direct.stderr, "stderr differs");
}

/// The line longer than the daemon's reads that chatter's `mix` writes on
/// stdout.
fn long_line() -> Vec<u8> {
    let mut long = b"long ".to_vec();
    long.extend(std::iter::repeat_n(b'x', 150_000));
    long.extend(b" end\n");
    long
}

/// The line that is not UTF-8 that chatter's `mix` writes on stdout, after
/// the long one.
const MIX_STDOUT: &[u8] = b"bytes \xc3\x28 out\n";
/// The line that is not UTF-8 that chatter's `mix` writes on stderr.
const MIX_STDERR: &[u8] = b"bytes \xff\xfe\x00 err\n";
/// What chatter's `unended` writes last on stdout, and just before on stderr.
const UNENDED_STDOUT: &[u8] = b"out without an end";
const UNENDED_STDERR: &[u8] = b"err without an end";

/// `--tail N` prints the last N lines of the record, counting the lines of
/// both streams in the order the daemon received them, each on its own
/// stream. A line longer than the daemon's reads is one line, and so is a
/// last line without an end, which counts as written when its last byte
/// was.
#[test]
fn tail_prints_the_last_lines() {
    let fixture = Fixture::new("basic");
    let (direct, run) = chatter_run(&fixture, &["6", "pace=100", "mix", "unended"]);
    let tail = |n: &str| {
        let output = log(&fixture, &[&run, "--tail", n]);
        support::assert_success(&output, "cargo heli log --tail");
        (output.stdout, output.stderr)
    };
    let both = |stdout: &[&[u8]], stderr: &[&[u8]]| (stdout.concat(), stderr.concat());

    assert_eq!(tail("1"), both(&[UNENDED_STDOUT], &[]));
    assert_eq!(tail("2"), both(&[UNENDED_STDOUT], &[UNENDED_STDERR]));
    assert_eq!(
        tail("4"),
        both(&[MIX_STDOUT, UNENDED_STDOUT], &[MIX_STDERR, UNENDED_STDERR])
    );
    assert_eq!(
        tail("6"),
        both(
            &[&long_line(), MIX_STDOUT, UNENDED_STDOUT],
            &[b"line 6 err\r\n", MIX_STDERR, UNENDED_STDERR]
        )
    );
    assert_eq!(tail("0"), both(&[], &[]));
    assert_eq!(tail("1000"), (direct.stdout, direct.stderr), "everything");
}

/// `--grep <re>` prints the lines that match a regular expression, each on
/// its own stream. A line is matched without its line end (`\n` or `\r\n`),
/// whatever its length, and whether or not it is UTF-8.
#[test]
fn grep_prints_the_matching_lines() {
    let fixture = Fixture::new("basic");
    let (_, run) = chatter_run(&fixture, &["20", "mix", "unended"]);
    let grep = |pattern: &str| {
        let output = log(&fixture, &[&run, "--grep", pattern]);
        support::assert_success(&output, "cargo heli log --grep");
        (output.stdout, output.stderr)
    };

    let (stdout, stderr) = grep("^line 1[0-9] (out|err)$");
    assert_eq!(
        text(&stdout),
        "line 11 out\nline 13 out\nline 15 out\nline 17 out\nline 19 out\n"
    );
    assert_eq!(
        text(&stderr),
        "line 10 err\r\nline 12 err\r\nline 14 err\r\nline 16 err\r\nline 18 err\r\n"
    );
    assert_eq!(grep("x end$"), (long_line(), Vec::new()));
    assert_eq!(grep("^bytes "), (MIX_STDOUT.to_vec(), MIX_STDERR.to_vec()));
    assert_eq!(
        grep("without an end$"),
        (UNENDED_STDOUT.to_vec(), UNENDED_STDERR.to_vec())
    );
    assert_eq!(grep("matches nothing"), (Vec::new(), Vec::new()));
}

/// Runs `chatter` with `args` three times through heli, with the run log
/// limited by `limit` (a variable and its value), and returns the run ids.
fn three_runs(fixture: &Fixture, args: &[&str], limit: (&str, &str)) -> Vec<String> {
    let chatter = fixture.binary("chatter");
    (0..3)
        .map(|_| {
            let output = fixture
                .heli(["exec", "--"])
                .arg(&chatter)
                .args(args)
                .env(limit.0, limit.1)
                .output()
                .expect("run heli");
            support::assert_success(&output, "cargo heli exec -- chatter");
            support::split_footer(&output.stderr).1.run
        })
        .collect()
}

/// Whether `log` finds a record of `run`: yes (exit 0), or "no run" (exit 1).
fn has_record(fixture: &Fixture, run: &str) -> bool {
    let output = log(fixture, &[run]);
    match output.status.code() {
        Some(0) => true,
        Some(1) if text(&output.stderr).contains(&format!("no run {run} ")) => false,
        _ => panic!("cargo heli log {run}: {output:?}"),
    }
}

/// Beyond `HELI_LOG_RUNS` records, the oldest are pruned first.
#[test]
fn the_oldest_records_go_first_beyond_the_run_limit() {
    let fixture = Fixture::new("basic");
    let runs = three_runs(&fixture, &["1"], ("HELI_LOG_RUNS", "2"));
    let kept: Vec<bool> = runs.iter().map(|run| has_record(&fixture, run)).collect();
    assert_eq!(kept, [false, true, true], "{runs:?}");
}

/// Beyond `HELI_LOG_BYTES` (here: room for one record of chatter's `mix`,
/// not two), the oldest records are pruned first.
#[test]
fn the_oldest_records_go_first_beyond_the_byte_limit() {
    let fixture = Fixture::new("basic");
    let runs = three_runs(&fixture, &["mix"], ("HELI_LOG_BYTES", "200000"));
    let kept: Vec<bool> = runs.iter().map(|run| has_record(&fixture, run)).collect();
    assert_eq!(kept, [false, true, true], "{runs:?}");
}

/// The record of a run in flight is never pruned: here, with room for one
/// record, two runs that end while a first one runs prune each other.
#[test]
fn a_run_in_flight_keeps_its_record() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let gate = fixture.path("gate");
    let mut call = fixture.heli(["exec", "--"]);
    call.arg(&echo)
        .arg("in flight")
        .env("ECHO_WAIT_FOR", &gate)
        .env("HELI_LOG_RUNS", "1");
    let call = start(&mut call);
    let status = fixture.wait_for_status("the run", |status| runs(status).len() == 1);
    let first = runs(&status)[0]["id"].as_str().expect("an id").to_owned();

    let later: Vec<String> = (0..2).map(|_| chatter_run(&fixture, &["1"]).1).collect();
    open(&gate);
    support::assert_success(&finish(call), "the first run");

    let logged = log(&fixture, &[&first]);
    support::assert_success(&logged, "cargo heli log");
    assert_eq!(text(&logged.stdout), "arg: \"in flight\"\n");
    assert_eq!(text(&logged.stderr), "echo-args: stderr\n");
    let kept: Vec<bool> = later.iter().map(|run| has_record(&fixture, run)).collect();
    assert_eq!(kept, [false, true], "{later:?}");
}

/// Records outlive the daemon: with none running, `log` reads the record
/// itself, the same way, and starts no daemon.
#[test]
fn log_reads_the_record_without_a_daemon() {
    let fixture = Fixture::new("basic");
    let (direct, run) = chatter_run(&fixture, &["4", "pace=50", "mix", "unended"]);
    fixture.kill_daemon();
    assert_eq!(fixture.daemon_pid(), None);

    let logged = log(&fixture, &[&run]);
    support::assert_success(&logged, "cargo heli log");
    assert!(logged.stdout == direct.stdout, "stdout differs");
    assert!(logged.stderr == direct.stderr, "stderr differs");
    let tail = log(&fixture, &[&run, "--tail", "2"]);
    assert_eq!(
        (tail.stdout, tail.stderr),
        (UNENDED_STDOUT.to_vec(), UNENDED_STDERR.to_vec())
    );
    let output = log(&fixture, &["r999999"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output.stderr),
        "cargo-heli: no run r999999 in the run log: no such run, or its record was pruned\n"
    );
    assert_eq!(fixture.daemon_pid(), None, "log started no daemon");
}

/// A daemon that does not support `log` (an older one) is asked to retire,
/// and the client reads the record itself after one warning.
#[test]
fn log_reads_the_record_itself_when_the_daemon_does_not_support_it() {
    let fixture = Fixture::new("basic");
    let (direct, run) = chatter_run(&fixture, &["3"]);
    fixture.kill_daemon();
    // At an endpoint of its own: on Unix the killed daemon's socket remains.
    let mut endpoint = fixture.endpoint();
    endpoint.push("-older");
    let fake = FakeDaemon::start(&endpoint, Answer::Unsupported);

    let logged = fixture
        .heli(["log", &run])
        .env("HELI_ENDPOINT", &endpoint)
        .output()
        .expect("run heli log");
    support::assert_success(&logged, "cargo heli log");
    assert!(logged.stdout == direct.stdout, "stdout differs");
    let stderr = text(&logged.stderr);
    let (warning, rest) = stderr.split_once('\n').expect("a warning line");
    assert!(
        warning.starts_with("cargo-heli: warning: ")
            && warning.contains("does not support")
            && warning.ends_with("; reading the run log directly"),
        "{warning}"
    );
    assert_eq!(rest, text(&direct.stderr), "and the record's stderr");
    assert_eq!(fake.received(), b"R", "asked to retire");
}

/// The path of `run`'s record: `runs/<run>.log` in the state directory.
fn record_path(fixture: &Fixture, run: &str) -> std::path::PathBuf {
    fixture.state_dir().join("runs").join(format!("{run}.log"))
}

/// A record laid out by hand: the header (`HELI-LOG`, format 1), then
/// `events`, each a kind byte, a little-endian u32 length and the bytes.
fn record(events: &[(u8, &[u8])]) -> Vec<u8> {
    let mut record = b"HELI-LOG".to_vec();
    record.extend(1u32.to_le_bytes());
    for (kind, bytes) in events {
        record.push(*kind);
        record.extend((bytes.len() as u32).to_le_bytes());
        record.extend(*bytes);
    }
    record
}

/// A record is read as far as it is complete: events of kinds this heli
/// does not know (a later one may add some) are skipped, a partial event at
/// the end (one being written) is not read yet, and a record whose header
/// is not complete yet reads as empty.
#[test]
fn a_record_is_read_as_far_as_it_is_complete() {
    let fixture = Fixture::new("basic");
    let (_, run) = chatter_run(&fixture, &["1"]);
    let path = record_path(&fixture, &run);
    let mut contents = record(&[(1, b"out\n"), (9, b"a later kind"), (2, b"err\n")]);
    let partial = record(&[(1, b"partial")]);
    contents.extend(&partial[12..20]);
    std::fs::write(&path, &contents).expect("write the record");
    let output = log(&fixture, &[&run]);
    support::assert_success(&output, "cargo heli log");
    assert_eq!(
        (text(&output.stdout), text(&output.stderr)),
        ("out\n".into(), "err\n".into())
    );

    std::fs::write(&path, b"HELI-").expect("write the record");
    let output = log(&fixture, &[&run]);
    support::assert_success(&output, "cargo heli log");
    assert_eq!((output.stdout, output.stderr), (Vec::new(), Vec::new()));
}

/// Asked over the protocol for something that is not a run id, the daemon
/// reads no file outside the run log.
#[test]
fn the_daemon_reads_only_records() {
    let fixture = Fixture::new("basic");
    let (_, run) = chatter_run(&fixture, &["1"]);
    let outside = fixture.state_dir().join("outside.log");
    std::fs::copy(record_path(&fixture, &run), &outside).expect("copy a record outside");

    let mut client = support::endpoint::connect(&fixture.endpoint());
    let mut hello = b"HELI".to_vec();
    hello.extend(support::fake_daemon::PROTOCOL.to_le_bytes());
    client.write_all(&hello).expect("send the hello");
    let mut answer = [0u8; 8];
    client.read_exact(&mut answer).expect("the daemon's hello");
    // A message frame: kind 0, a little-endian u32 length, JSON.
    let request = br#"{"Log":{"run":"../outside","select":"All"}}"#;
    let mut frame = vec![0u8];
    frame.extend((request.len() as u32).to_le_bytes());
    frame.extend(request);
    client.write_all(&frame).expect("send the request");
    let mut header = [0u8; 5];
    client.read_exact(&mut header).expect("the daemon's answer");
    let mut reply =
        vec![0u8; u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize];
    client.read_exact(&mut reply).expect("the daemon's answer");
    assert_eq!(header[0], 0, "a message, not output: {}", text(&reply));
    let reply = text(&reply);
    assert!(
        reply.contains("LogFailed") && reply.contains("no run ../outside"),
        "{reply}"
    );
}

/// Records live in `runs/` in the state directory, as `<run>.log`. One this
/// heli cannot read (not a record, or of a later format) is a clear error.
#[test]
fn a_record_heli_cannot_read_is_a_clear_error() {
    let fixture = Fixture::new("basic");
    let (_, run) = chatter_run(&fixture, &["1"]);
    let record = record_path(&fixture, &run);
    assert!(record.is_file(), "{} is missing", record.display());

    let mut later = b"HELI-LOG".to_vec();
    later.extend(2u32.to_le_bytes());
    for contents in [b"not a record at all".to_vec(), later] {
        std::fs::write(&record, &contents).expect("replace the record");
        let output = log(&fixture, &[&run]);
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(output.stdout.is_empty());
        assert_eq!(
            text(&output.stderr),
            format!("cargo-heli: the record of {run} is not a run log this heli can read\n")
        );
    }
}

/// A pattern that is not a regular expression, and two selectors at once,
/// are usage errors.
#[test]
fn bad_selectors_are_usage_errors() {
    let fixture = Fixture::new("basic");
    let (_, run) = chatter_run(&fixture, &["2"]);
    for args in [
        &[run.as_str(), "--grep", "(unclosed"][..],
        &[run.as_str(), "--grep", "line", "--tail", "1"][..],
    ] {
        let output = log(&fixture, args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(
            text(&output.stderr).contains("--grep"),
            "{args:?}: {}",
            text(&output.stderr)
        );
    }
}

/// A record is written as the output arrives: `log` prints what a run in
/// flight has written so far.
#[test]
fn log_shows_a_run_in_flight_so_far() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let gate = fixture.path("gate");
    let mut call = fixture.heli(["exec", "--"]);
    call.arg(&echo).arg("so far").env("ECHO_WAIT_FOR", &gate);
    let call = start(&mut call);
    let status = fixture.wait_for_status("the run", |status| runs(status).len() == 1);
    let id = runs(&status)[0]["id"].as_str().expect("an id").to_owned();

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let logged = log(&fixture, &[&id]);
        support::assert_success(&logged, "cargo heli log");
        if text(&logged.stdout) == "arg: \"so far\"\n" {
            assert_eq!(text(&logged.stderr), "", "nothing on stderr yet");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the record never showed the output: {logged:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(fixture.runs().len(), 1, "the run is still in flight");
    open(&gate);
    support::assert_success(&finish(call), "cargo heli exec");
}

/// Output is recorded whether or not a client listens: a run whose client
/// was killed has all of its output in the log, including what it wrote
/// after.
#[test]
fn output_is_recorded_after_the_client_is_gone() {
    let fixture = Fixture::new("basic");
    let echo = fixture.echo_args();
    let gate = fixture.path("gate");
    let mut call = fixture.heli(["exec", "--"]);
    call.arg(&echo).arg("before").env("ECHO_WAIT_FOR", &gate);
    let call = start(&mut call);
    let status = fixture.wait_for_status("the run", |status| runs(status).len() == 1);
    let id = runs(&status)[0]["id"].as_str().expect("an id").to_owned();

    support::kill_tree(call.id());
    let _ = finish(call);
    open(&gate);
    fixture.wait_for_status("the run to end", |status| runs(status).is_empty());
    let logged = log(&fixture, &[&id]);
    support::assert_success(&logged, "cargo heli log");
    assert_eq!(text(&logged.stdout), "arg: \"before\"\n");
    assert_eq!(text(&logged.stderr), "echo-args: stderr\n", "written after");
}

/// A run id with no record (it never ran, or its record was pruned) is a
/// clear error, exit 1. Something that is not a run id at all is a usage
/// error, exit 2. A bare number means the run of that number.
#[test]
fn an_unknown_run_is_a_clear_error() {
    let fixture = Fixture::new("basic");
    let (direct, run) = chatter_run(&fixture, &["3"]);

    let output = log(&fixture, &["r999999"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        text(&output.stderr),
        "cargo-heli: no run r999999 in the run log: no such run, or its record was pruned\n"
    );

    let output = log(&fixture, &["banana"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        text(&output.stderr).contains("a run id is r<number>"),
        "{}",
        text(&output.stderr)
    );

    let number = run.strip_prefix('r').expect("r<number>");
    for id in [number.to_owned(), format!("r0{number}")] {
        let output = log(&fixture, &[&id]);
        support::assert_success(&output, &format!("cargo heli log {id}"));
        assert!(output.stdout == direct.stdout, "{id} is {run}");
    }
}
