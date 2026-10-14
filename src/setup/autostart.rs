//! `cargo heli setup --autostart` and `--no-autostart`: the registration
//! that starts the daemon at logon (`crate::autostart`).
//!
//! setup says what the registration is, shows each step that makes or
//! removes it (the definition's whole text, the links, the files it
//! removes, the service manager commands), then takes the steps, in order,
//! stopping at the first that fails. The flag is the consent, so nothing is
//! asked; without either flag, setup registers and removes nothing.
//!
//! - **Again.** A registration that is in place as it would be made is left
//!   alone ("registered already"), and so is the absence of one.
//! - **The record** for clients is removed before any step and written
//!   after the last, so clients never start the daemon through a
//!   registration that is half made or half removed.
//! - **`--print`** shows the steps of a first registration (or of a
//!   removal) and takes none; it does not ask the service manager anything.

use crate::autostart::{Registration, Step};

/// Registers the daemon to start at logon; false when that failed.
pub fn register(registration: &Registration, print: bool) -> bool {
    println!(
        "Autostart: {} starts the heli daemon when you log on, in your session, and only \
         while you are logged on.",
        registration.what()
    );
    let Some(steps) = registration.register_steps(!print) else {
        // A record removed by hand comes back.
        println!("It is registered already.");
        return record(registration);
    };
    show(&steps);
    if print {
        println!("Nothing registered: --print changes nothing.");
        return true;
    }
    if let Err(error) = take(registration, &steps) {
        eprintln!("cargo-heli: cannot register autostart: {error}");
        return false;
    }
    if !record(registration) {
        return false;
    }
    println!("Registered.");
    true
}

/// Removes the registration; false when that failed.
pub fn unregister(registration: &Registration, print: bool) -> bool {
    let steps = registration.unregister_steps(!print);
    match &steps {
        Some(steps) => {
            println!("Autostart: removing {}.", registration.what());
            show(steps);
        }
        None => println!("Autostart: {} is not registered.", registration.what()),
    }
    if print {
        println!("Nothing removed: --print changes nothing.");
        return true;
    }
    // With nothing registered, a record left behind still goes.
    if let Err(error) = take(registration, steps.as_deref().unwrap_or_default()) {
        eprintln!("cargo-heli: cannot remove autostart: {error}");
        return false;
    }
    if steps.is_some() {
        println!("Removed.");
    }
    true
}

/// Shows `steps`, each as it will be taken.
fn show(steps: &[Step]) {
    for step in steps {
        match step {
            Step::Write { path, content } => {
                println!("Definition {}:\n{}", path.display(), content.text);
            }
            Step::Link { path, target } => {
                println!("Link {} -> {}", path.display(), target.display());
            }
            Step::Remove(path) => println!("Remove {}", path.display()),
            Step::Run(tool) => println!("Run {tool}"),
        }
    }
}

/// Removes the record, then takes `steps`.
fn take(registration: &Registration, steps: &[Step]) -> Result<(), String> {
    registration
        .remove_record()
        .map_err(|error| format!("cannot remove the record of the registration: {error}"))?;
    steps.iter().try_for_each(Step::take)
}

/// Writes the record for clients; false, with the reason, when it cannot.
fn record(registration: &Registration) -> bool {
    match registration.write_record() {
        Ok(()) => true,
        Err(error) => {
            eprintln!("cargo-heli: cannot write the record of the registration: {error}");
            false
        }
    }
}
