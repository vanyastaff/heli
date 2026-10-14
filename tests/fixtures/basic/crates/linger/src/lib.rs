//! A test that lingers, as a long test under cargo or nextest does.
//!
//! With `HELI_FIXTURE_PIDS` naming a directory, the test `lingers` starts a
//! copy of itself in a process group of its own (as nextest starts each
//! test); each of the two writes an empty file named after its process id
//! there, then waits until `HELI_FIXTURE_GATE` exists (60 s at most).
//! Without the variable, the test passes at once.

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    #[test]
    fn lingers() {
        let Some(pids) = std::env::var_os("HELI_FIXTURE_PIDS") else {
            return;
        };
        if std::env::var_os("HELI_FIXTURE_LINGER_CHILD").is_none() {
            let mut child = Command::new(std::env::current_exe().expect("own path"));
            child
                .args(["--exact", "tests::lingers"])
                .env("HELI_FIXTURE_LINGER_CHILD", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            #[cfg(unix)]
            std::os::unix::process::CommandExt::process_group(&mut child, 0);
            #[cfg(windows)]
            {
                const CREATE_NEW_PROCESS_GROUP: u32 = 0x200;
                std::os::windows::process::CommandExt::creation_flags(
                    &mut child,
                    CREATE_NEW_PROCESS_GROUP,
                );
            }
            #[allow(clippy::zombie_processes, reason = "the child outlives this test")]
            let _child = child.spawn().expect("start the child");
        }
        let file = Path::new(&pids).join(std::process::id().to_string());
        std::fs::write(file, "").expect("write the process id");
        let gate = std::env::var_os("HELI_FIXTURE_GATE").expect("HELI_FIXTURE_GATE");
        let deadline = Instant::now() + Duration::from_secs(60);
        while !Path::new(&gate).exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
