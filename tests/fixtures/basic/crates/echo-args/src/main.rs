//! Prints each argument on stdout as `arg: <debug>`, then, when `ECHO_ENV`
//! names a variable, `env <name>=<debug of its value>`; prints
//! `echo-args: stderr` on stderr, and exits with `ECHO_EXIT` (default 0).

fn main() {
    for arg in std::env::args_os().skip(1) {
        println!("arg: {arg:?}");
    }
    if let Some(name) = std::env::var_os("ECHO_ENV") {
        let value = std::env::var_os(&name);
        println!("env {}={value:?}", name.to_string_lossy());
    }
    eprintln!("echo-args: stderr");
    let code = std::env::var("ECHO_EXIT")
        .ok()
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    std::process::exit(code);
}
