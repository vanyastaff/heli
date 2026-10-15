//! Standalone, dependency-free instrumentation for multi-project-spike.py.
//! Build directly with rustc, never install as a user/global wrapper.
use std::{
    env, fs,
    path::PathBuf,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

fn escaped(value: &str) -> String {
    let mut out = String::new();
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn event(invocation: u128, phase: &str, crate_name: &str, code: Option<i32>) {
    let Some(root) = env::var_os("HELI_SPIKE_EVENTS") else {
        return;
    };
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let agent = env::var("HELI_SPIKE_AGENT").unwrap_or_default();
    let run = env::var("HELI_RUN").unwrap_or_default();
    let record = format!(
        "{{\"phase\":\"{}\",\"epoch_ns\":{},\"invocation\":\"{}\",\"pid\":{},\"agent\":\"{}\",\"heli_run\":\"{}\",\"crate\":\"{}\",\"exit\":{}}}\n",
        phase,
        nanos,
        invocation,
        pid,
        escaped(&agent),
        escaped(&run),
        escaped(crate_name),
        code.map_or("null".to_owned(), |n| n.to_string())
    );
    let path = PathBuf::from(root).join(format!("{invocation}-{pid}-{phase}.json"));
    // Failure is explicit: missing instrumentation must never look like evidence.
    fs::write(path, record).expect("cannot write compiler instrumentation event");
}

fn main() {
    let invocation = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let mut args = env::args_os().skip(1);
    let compiler = args
        .next()
        .expect("Cargo must supply the compiler executable");
    if compiler == "--spike-hold-lock" {
        let path = args.next().expect("lock path");
        let ready = args.next().expect("ready file");
        let release = PathBuf::from(args.next().expect("release file"));
        let file = fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .expect("open isolated host lock");
        file.lock().expect("take isolated host lock");
        fs::write(ready, std::process::id().to_string()).expect("signal lock held");
        let seconds = args
            .next()
            .map(|value| {
                value
                    .to_string_lossy()
                    .parse::<u64>()
                    .expect("lock timeout seconds")
            })
            .unwrap_or(120);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
        while !release.exists() {
            assert!(std::time::Instant::now() < deadline, "lock-holder deadline");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        return;
    }
    let args: Vec<_> = args.collect();
    let crate_name = args
        .windows(2)
        .find(|p| p[0] == "--crate-name")
        .map(|p| p[1].to_string_lossy().into_owned());
    let actual = crate_name.is_some()
        && !args.iter().any(|a| {
            a == "-vV"
                || a == "--version"
                || a == "--print"
                || a.to_string_lossy().starts_with("--print=")
        });
    if actual {
        event(invocation, "start", crate_name.as_deref().unwrap(), None);
    }
    let status = Command::new(compiler)
        .args(&args)
        .status()
        .expect("cannot start the original compiler supplied by Cargo");
    let code = status.code().unwrap_or(1);
    if actual {
        event(
            invocation,
            "end",
            crate_name.as_deref().unwrap(),
            Some(code),
        );
    }
    std::process::exit(code);
}
