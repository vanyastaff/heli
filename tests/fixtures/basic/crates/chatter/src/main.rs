//! Writes a known mix of output on stdout and stderr, for the run log tests.
//!
//! Arguments, in any order:
//! - `<count>`: numbered lines `line <i> out` on stdout (odd `i`, ending in
//!   `\n`) and `line <i> err` on stderr (even `i`, ending in `\r\n`), for
//!   `i` from 1 to `count`;
//! - `pace=<ms>`: sleep that long after each write, so that a reader of both
//!   pipes receives the writes in the order they were made;
//! - `mix`: then, lines that test a record's fidelity:
//!   - on stdout, a line of more than 64 KiB: `long `, 150 000 `x`, ` end`;
//!   - on stderr, `bytes ` with bytes that are not UTF-8 and a NUL;
//!   - on stdout, `bytes ` with other bytes that are not UTF-8;
//! - `unended`: last, `err without an end` on stderr, then
//!   `out without an end` on stdout, neither ending a line.
//!
//! Every write is flushed at once.

use std::io::Write;
use std::time::Duration;

fn main() {
    let mut count = 0;
    let mut pace = Duration::ZERO;
    let mut mix = false;
    let mut unended = false;
    for arg in std::env::args().skip(1) {
        if arg == "mix" {
            mix = true;
        } else if arg == "unended" {
            unended = true;
        } else if let Some(ms) = arg.strip_prefix("pace=") {
            pace = Duration::from_millis(ms.parse().expect("pace=<milliseconds>"));
        } else {
            count = arg.parse().expect("a line count");
        }
    }
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    let mut write = |stream: &mut dyn Write, bytes: &[u8]| {
        stream.write_all(bytes).expect("write");
        stream.flush().expect("flush");
        std::thread::sleep(pace);
    };
    for i in 1..=count {
        if i % 2 == 1 {
            write(&mut stdout, format!("line {i} out\n").as_bytes());
        } else {
            write(&mut stderr, format!("line {i} err\r\n").as_bytes());
        }
    }
    if mix {
        let mut long = b"long ".to_vec();
        long.extend(std::iter::repeat_n(b'x', 150_000));
        long.extend(b" end\n");
        write(&mut stdout, &long);
        write(&mut stderr, b"bytes \xff\xfe\x00 err\n");
        write(&mut stdout, b"bytes \xc3\x28 out\n");
    }
    if unended {
        write(&mut stderr, b"err without an end");
        write(&mut stdout, b"out without an end");
    }
}
