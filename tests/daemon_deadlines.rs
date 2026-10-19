//! Unresponsive daemons and stalled followers have bounded waits.
mod support;

use std::process::Stdio;
use std::time::{Duration, Instant};
use support::Fixture;
use support::fake_daemon::{Answer, FakeDaemon};

#[test]
fn post_handshake_silence_falls_back_within_five_seconds() {
    let fixture = Fixture::new("basic");
    // An empty reply sequence still completes the current handshake; the
    // fake then reads and holds the connection without a reply.
    let fake = FakeDaemon::start(&fixture.endpoint(), Answer::Replies(&[]));
    let mut client = fixture
        .heli(["exec", "--", "cargo", "--version"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the public CLI");
    let started = Instant::now();
    let deadline = started + Duration::from_secs(7);
    let mut completed = false;
    while Instant::now() < deadline {
        if client.try_wait().expect("inspect the client").is_some() {
            completed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if !completed {
        client
            .kill()
            .expect("stop only this repro's stalled client");
    }
    let output = client.wait_with_output().expect("collect client output");
    fake.received();
    assert!(
        completed,
        "docs/daemon.md requires fallback after 5 seconds without a daemon answer; \
         the public CLI remained blocked for 7 seconds after a successful handshake. \
         stdout={:?}; stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        output.status.success(),
        "fallback cargo --version must succeed"
    );
    assert!(
        started.elapsed() >= Duration::from_secs(4),
        "did not wait for the submit deadline: {output:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("did not take the run"),
        "{output:?}"
    );
}

#[test]
fn completed_run_with_a_stalled_client_does_not_block_retirement() {
    use std::io::{Read, Write};

    let fixture = Fixture::new("basic");
    let chatter = fixture.binary("chatter");
    fixture.start_daemon();
    let daemon = fixture.daemon_pid().expect("a daemon");
    // Use the public wire protocol to model a client whose stdout reader
    // stops. More output than any socket/pipe buffer makes its follower
    // block, without blocking the run (the existing detach contract).
    let request = serde_json::json!({"Run": {
        "cwd": fixture.workspace().to_str().expect("Unicode fixture path"),
        "argv": [chatter.to_str().expect("Unicode executable path"), "100000", "mix"],
        "env": std::env::vars().collect::<Vec<_>>(),
    }});
    let bytes = serde_json::to_vec(&request).expect("request JSON");
    let mut stalled = support::endpoint::connect(&fixture.endpoint());
    stalled.write_all(b"HELI").expect("magic");
    stalled
        .write_all(&support::fake_daemon::PROTOCOL.to_le_bytes())
        .expect("current version");
    let mut hello = [0u8; 8];
    stalled.read_exact(&mut hello).expect("daemon hello");
    let mut frame = vec![0u8];
    frame.extend((bytes.len() as u32).to_le_bytes());
    frame.extend(bytes);
    stalled.write_all(&frame).expect("submit command");
    let mut header = [0u8; 5];
    stalled.read_exact(&mut header).expect("started header");
    let mut reply = vec![0u8; u32::from_le_bytes(header[1..].try_into().unwrap()) as usize];
    stalled.read_exact(&mut reply).expect("started reply");
    let reply: serde_json::Value = serde_json::from_slice(&reply).expect("reply JSON");
    assert!(reply.get("Started").is_some(), "{reply}");
    // Command completion is independently observable through status;
    // thereafter the deliberately stalled client has nothing to execute.
    fixture.wait_for_status("the chatter command to finish", |status| {
        support::runs(status).is_empty()
    });

    let mut newer = support::endpoint::connect(&fixture.endpoint());
    newer.write_all(b"HELI").expect("newer magic");
    newer
        .write_all(&u32::MAX.to_le_bytes())
        .expect("newer version");
    newer.read_exact(&mut hello).expect("newer hello reply");
    newer.write_all(b"R").expect("request retirement");
    // Closing this protocol connection acknowledges that retire() ran.
    let _ = newer.read_to_end(&mut Vec::new());
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut exited = false;
    while Instant::now() < deadline {
        if fixture.daemon_pid().is_none() {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let still_running = fixture.daemon_pid();
    // Always release our own stalled client before the assertion; on the
    // defective implementation this alone permits retirement to finish.
    drop(stalled);
    fixture.wait_for_status("retirement after releasing the stalled client", |status| {
        status["daemon"].is_null()
    });
    assert!(
        exited,
        "all commands completed, yet retired daemon {daemon} stayed alive for 3 seconds \
         solely because its completed run's client stopped reading; observed {still_running:?}"
    );
}
