//! Stands in for the service managers heli's autostart drives, which heli
//! reaches only through `HELI_SCHTASKS`, `HELI_SYSTEMCTL` and
//! `HELI_LAUNCHCTL`: `schtasks` (Windows), `systemctl` (Linux) and
//! `launchctl` (macOS). Nothing it does reaches the real ones.
//!
//! - Every call is appended to the file `FAKE_MANAGER_LOG` as one line: the
//!   arguments, separated by tabs.
//! - It keeps one registration, which exists while the file
//!   `FAKE_MANAGER_STATE` does. What a call does depends on its verb, the
//!   first argument that does not start with `--`:
//!   - `/Create` (schtasks) and `bootstrap` (launchctl) register;
//!   - `/Delete` and `bootout` unregister;
//!   - `/Query` and `print` exit 0 while registered, else 1;
//!   - `/Run`, `start` (systemctl) and `kickstart` start the program
//!     `FAKE_MANAGER_START` names, if any, with the arguments in
//!     `FAKE_MANAGER_START_ARGS` (separated by spaces) and this process's
//!     environment, and exit at once, as a service manager does: without
//!     waiting for it, and with no stdio;
//!   - any other verb (`daemon-reload`) does nothing.
//!
//!   Each prints `fake-manager: <verb>` on stdout, as the real tools say
//!   what they did.
//! - The verb `FAKE_MANAGER_FAIL` names fails instead: it prints
//!   `fake-manager: <verb> failed` on stderr and exits 1.
//! - With `FAKE_MANAGER_SLEEP_MS`, it first sleeps that long, as a service
//!   manager that does not answer.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn main() {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    if let Some(log) = std::env::var_os("FAKE_MANAGER_LOG") {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .expect("open the fake manager's log");
        file.write_all(format!("{}\n", args.join("\t")).as_bytes())
            .expect("write the fake manager's log");
    }
    if let Some(sleep) = std::env::var("FAKE_MANAGER_SLEEP_MS")
        .ok()
        .and_then(|ms| ms.parse().ok())
    {
        std::thread::sleep(std::time::Duration::from_millis(sleep));
    }
    let verb = args
        .iter()
        .find(|arg| !arg.starts_with("--"))
        .cloned()
        .unwrap_or_default();
    if std::env::var("FAKE_MANAGER_FAIL").is_ok_and(|fail| fail == verb) {
        eprintln!("fake-manager: {verb} failed");
        std::process::exit(1);
    }
    let state = std::env::var_os("FAKE_MANAGER_STATE").map(PathBuf::from);
    let registered = state.as_ref().is_some_and(|state| state.exists());
    println!("fake-manager: {verb}");
    match verb.as_str() {
        "/Create" | "bootstrap" => {
            if let Some(state) = &state {
                std::fs::write(state, "registered").expect("register");
            }
        }
        "/Delete" | "bootout" => {
            if let Some(state) = &state {
                let _ = std::fs::remove_file(state);
            }
        }
        "/Query" | "print" => std::process::exit(if registered { 0 } else { 1 }),
        "/Run" | "start" | "kickstart" => {
            if let Some(program) = std::env::var_os("FAKE_MANAGER_START") {
                let args = std::env::var("FAKE_MANAGER_START_ARGS").unwrap_or_default();
                #[allow(clippy::zombie_processes, reason = "a service manager does not wait")]
                let _started = Command::new(program)
                    .args(args.split_whitespace())
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("start the program");
            }
        }
        _ => {}
    }
}
