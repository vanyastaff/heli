#!/usr/bin/env python3
"""Tiny package-view regression; never modifies a supplied vendor tree.

Run with Python 3.12+ on Windows, Linux or macOS. Windows uses junctions;
POSIX uses directory symlinks. Every artifact is placed in a NEW output root.
Only verified link entries in this experiment's first view are retired.
Backing package objects and all other views remain intact for inspection.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import time
import tomllib
import uuid

CREATE_FLAGS = subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0


def save(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def invoke(argv, env, cwd):
    start = time.time_ns()
    proc = subprocess.run([str(a) for a in argv], cwd=cwd, env=env, capture_output=True,
                          text=True, encoding="utf-8", errors="replace", timeout=90,
                          creationflags=CREATE_FLAGS)
    return {"argv": [str(a) for a in argv], "exit": proc.returncode,
            "stdout": proc.stdout, "stderr": proc.stderr,
            "start_ns": start, "end_ns": time.time_ns()}


def inventory(root):
    rows = []
    for path in sorted(root.rglob("*")):
        if path.is_symlink() or path.is_junction():
            raise RuntimeError(f"Object input contains a link: {path}")
        if path.is_file():
            rows.append({"path": path.relative_to(root).as_posix(), "bytes": path.stat().st_size,
                         "sha256": hashlib.sha256(path.read_bytes()).hexdigest()})
    key = hashlib.sha256(json.dumps(rows, sort_keys=True).encode()).hexdigest()
    return {"sha256": key, "bytes": sum(row["bytes"] for row in rows), "files": rows}


def is_view_link(path):
    return path.is_junction() if os.name == "nt" else path.is_symlink()


def link_view(link, target, env, cwd):
    if os.name != "nt":
        link.symlink_to(target, target_is_directory=True)
        if not is_view_link(link) or link.resolve() != target.resolve():
            raise RuntimeError(f"Symlink target mismatch: {link}")
        return
    scoped = env | {"HELI_EPOCH_LINK": str(link), "HELI_EPOCH_OBJECT": str(target)}
    result = invoke(["powershell.exe", "-NoProfile", "-NonInteractive", "-Command",
                     "New-Item -ItemType Junction -Path $env:HELI_EPOCH_LINK -Target $env:HELI_EPOCH_OBJECT -ErrorAction Stop | Out-Null"], scoped, cwd)
    if result["exit"]:
        save(cwd / "junction-error.json", result)
        raise RuntimeError(json.dumps(result))
    if not is_view_link(link) or link.resolve() != target.resolve():
        raise RuntimeError(f"Junction target mismatch: {link}")


def retire_link(link, target):
    if not is_view_link(link) or link.resolve() != target.resolve():
        raise RuntimeError("Refusing to retire an unexpected path")
    if os.name == "nt":
        os.rmdir(link)  # Remove only the junction entry, not its target.
    else:
        link.unlink()  # Nonrecursive symlink removal, never its target.


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--vendor", default="E:/heli/target/dependency-epoch-spike-20261015-2216/vendor-epoch-1",
                      help="Existing vendor tree, read-only/offline mode")
    mode.add_argument("--prepare-vendor", action="store_true",
                      help="Fetch only the two pinned packages in a new private publisher home")
    parser.add_argument("--toolchain", default="stable", help="Installed rustup toolchain (default stable)")
    parser.add_argument("--output", default=str(Path("target") / f"package-epoch-view-{time.strftime('%m%d-%H%M%S')}-{uuid.uuid4().hex[:4]}"))
    args = parser.parse_args()
    if not hasattr(Path(), "is_junction"):
        raise RuntimeError("Requires Python 3.12+")
    source = None if args.prepare_vendor else Path(args.vendor).resolve(strict=True)
    root = Path(args.output).resolve()
    root.mkdir(parents=True, exist_ok=False)
    env = os.environ.copy()
    for name in ("CARGO_MAKEFLAGS", "MAKEFLAGS", "MFLAGS", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER",
                 "CARGO_BUILD_RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER", "CARGO_ENCODED_RUSTFLAGS",
                 "RUSTFLAGS", "CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR", "CARGO_BUILD_BUILD_DIR"):
        env.pop(name, None)
    tool_home = root / "tool-home"
    tool_home.mkdir()
    env.update(CARGO_INCREMENTAL="0", CARGO_BUILD_JOBS="2", RUSTUP_TOOLCHAIN=args.toolchain,
               CARGO_HOME=str(tool_home))
    save(root / "invocation.json", {"toolchain": args.toolchain, "prepare_vendor": args.prepare_vendor,
                                    "source": str(source) if source else None,
                                    "link_kind": "junction" if os.name == "nt" else "symlink"})
    tools = {}
    for program in ("cargo", "rustc"):
        found = invoke(["rustup", "which", "--toolchain", args.toolchain, program], env, root)
        if found["exit"]:
            raise RuntimeError(json.dumps(found))
        tools[program] = found["stdout"].strip()
    env["RUSTC"] = tools["rustc"]
    if args.prepare_vendor:
        publisher_home = root / "publisher-home"
        publisher_home.mkdir()
        publisher = root / "publisher-project"
        (publisher / "src").mkdir(parents=True)
        (publisher / "Cargo.toml").write_text('''[package]
name="epoch-view-publisher"
version="0.0.0"
edition="2024"
[workspace]
[dependencies]
itoa="=1.0.18"
ryu="=1.0.23"
''', encoding="utf-8")
        (publisher / "src/lib.rs").write_text("// Input only; publisher is never compiled.\n", encoding="utf-8")
        publisher_env = env | {"CARGO_HOME": str(publisher_home)}
        row = invoke([tools["cargo"], "generate-lockfile"], publisher_env, publisher)
        save(root / "publisher-lock.json", row)
        if row["exit"]:
            raise RuntimeError(json.dumps(row))
        source = root / "prepared-vendor"
        row = invoke([tools["cargo"], "vendor", "--locked", "--versioned-dirs", str(source)], publisher_env, publisher)
        save(root / "publisher-vendor.json", row)
        if row["exit"]:
            raise RuntimeError(json.dumps(row))
    objects = root / "objects"
    objects.mkdir()
    packages = {}
    for name in ("itoa", "ryu"):
        expected = {"itoa": "1.0.18", "ryu": "1.0.23"}[name]
        matching = []
        for entry in source.iterdir():
            manifest = entry / "Cargo.toml"
            if manifest.is_file():
                package = tomllib.loads(manifest.read_text(encoding="utf-8")).get("package", {})
                if package.get("name") == name and package.get("version") == expected:
                    matching.append(entry)
        if len(matching) != 1:
            raise RuntimeError(f"Expected one {name} {expected} package, found {len(matching)}")
        package_source = matching[0]
        before = inventory(package_source)
        target = objects / before["sha256"]
        shutil.copytree(package_source, target)
        for path in target.rglob("*"):
            if path.is_file():
                path.chmod(stat.S_IREAD)
        if inventory(target) != before:
            raise RuntimeError("Copied package content mismatch")
        packages[name] = {"source": str(package_source), "object": str(target), "inventory": before}
    views = [root / "epoch-1", root / "epoch-2"]
    try:
        for view in views:
            view.mkdir()
            for name, package in packages.items():
                link_view(view / name, Path(package["object"]), env, root)
    except RuntimeError as error:
        result = {"verdict": "BLOCKED", "root": str(root), "source_vendor": str(source),
                  "stage": "create-link-view", "error": str(error), "packages": packages,
                  "cargo_commands_run": 0, "deletion_attempted": False,
                  "limits": ["Junction creation unavailable in this execution context; Cargo compatibility remains untested"]}
        save(root / "results.json", result)
        print(json.dumps({key: value for key, value in result.items() if key != "packages"}, indent=2))
        return 2
    identities = {}
    for name, package in packages.items():
        paths = [view / name / "Cargo.toml" for view in views] + [Path(package["object"]) / "Cargo.toml"]
        identities[name] = [{"path": str(path), "dev": path.stat().st_dev,
                             "file_id": path.stat().st_ino} for path in paths]
    records = []
    agents = []
    for index, view in enumerate(views, 1):
        home = root / f"home-{index}"
        home.mkdir()
        (home / "config.toml").write_text(f'''[source.crates-io]
replace-with = "epoch-view"
[source.epoch-view]
directory = '{view.as_posix()}'
''', encoding="utf-8")
        project = root / f"project-{index}"
        (project / "src").mkdir(parents=True)
        (project / "Cargo.toml").write_text('''[package]
name="epoch-view-check"
version="0.0.0"
edition="2024"
[workspace]
[dependencies]
itoa="=1.0.18"
ryu="=1.0.23"
''', encoding="utf-8")
        (project / "src/lib.rs").write_text('''pub fn integer() -> String { itoa::Buffer::new().format(42).to_owned() }
pub fn float() -> String { ryu::Buffer::new().format(1.25).to_owned() }
#[test] fn formats() { assert_eq!(integer(), "42"); assert_eq!(float(), "1.25"); }
''', encoding="utf-8")
        scoped = env | {"CARGO_HOME": str(home), "CARGO_TARGET_DIR": str(root / f"target-{index}"),
                        "CARGO_BUILD_BUILD_DIR": str(root / f"build-{index}")}
        row = invoke([tools["cargo"], "generate-lockfile", "--offline"], scoped, project)
        records.append(row | {"phase": f"lock-{index}"})
        if row["exit"]:
            raise RuntimeError(json.dumps(row))
        agents.append((project, scoped))
    running = []
    for index, (project, scoped) in enumerate(agents, 1):
        stdout = (root / f"check-{index}.stdout").open("wb")
        stderr = (root / f"check-{index}.stderr").open("wb")
        argv = [tools["cargo"], "check", "--frozen", "--jobs", "2"]
        started = time.time_ns()
        proc = subprocess.Popen(argv, cwd=project, env=scoped, stdout=stdout, stderr=stderr,
                                creationflags=CREATE_FLAGS)
        running.append((index, proc, stdout, stderr, started, argv))
    for index, proc, stdout, stderr, started, argv in running:
        code = proc.wait(timeout=90)
        stdout.close()
        stderr.close()
        records.append({"phase": f"parallel-check-{index}", "argv": argv, "exit": code,
                        "start_ns": started, "end_ns": time.time_ns(),
                        "stdout": (root / f"check-{index}.stdout").read_text(encoding="utf-8"),
                        "stderr": (root / f"check-{index}.stderr").read_text(encoding="utf-8")})
    # A generated build-script gate keeps an actual fresh Cargo2 alive while
    # the already-finished Cargo1 view retires. It reads both shared packages
    # through epoch2 AFTER release, making continued access observable.
    project, scoped = agents[1]
    (project / "build.rs").write_text('''use std::{env, fs, path::PathBuf, thread, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};
fn main() {
    fs::write(env::var_os("EPOCH_GATE_READY").unwrap(), std::process::id().to_string()).unwrap();
    let release = PathBuf::from(env::var_os("EPOCH_GATE_RELEASE").unwrap());
    let deadline = Instant::now() + Duration::from_secs(60);
    while !release.exists() {
        assert!(Instant::now() < deadline, "epoch consumer gate timeout");
        thread::sleep(Duration::from_millis(10));
    }
    let epoch = PathBuf::from(env::var_os("EPOCH_GATE_VIEW").unwrap());
    let itoa = fs::read(epoch.join("itoa/Cargo.toml")).unwrap();
    let ryu = fs::read(epoch.join("ryu/Cargo.toml")).unwrap();
    fs::write(env::var_os("EPOCH_GATE_READ").unwrap(), format!("epoch_ns={}\\nitoa_bytes={}\\nryu_bytes={}\\n",
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(), itoa.len(), ryu.len())).unwrap();
}
''', encoding="utf-8")
    ready = root / "consumer-ready"
    release = root / "consumer-release"
    read_after_release = root / "consumer-read-packages"
    fresh = scoped | {"CARGO_TARGET_DIR": str(root / "target-active-consumer"),
                      "CARGO_BUILD_BUILD_DIR": str(root / "build-active-consumer"),
                      "EPOCH_GATE_READY": str(ready), "EPOCH_GATE_RELEASE": str(release),
                      "EPOCH_GATE_READ": str(read_after_release), "EPOCH_GATE_VIEW": str(views[1])}
    active_stdout = (root / "active-consumer.stdout").open("wb")
    active_stderr = (root / "active-consumer.stderr").open("wb")
    active_argv = [tools["cargo"], "test", "--frozen", "--jobs", "2"]
    active_start = time.time_ns()
    consumer = subprocess.Popen(active_argv, cwd=project, env=fresh, stdout=active_stdout,
                                stderr=active_stderr, creationflags=CREATE_FLAGS)
    gate = {"cargo_pid": consumer.pid, "started_ns": active_start, "retirement_states": []}
    retired = []
    try:
        deadline = time.monotonic() + 30
        while not ready.exists():
            if consumer.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError("Fresh Cargo2 did not reach its build-script gate")
            time.sleep(0.01)
        gate.update(ready_ns=time.time_ns(), build_script_pid=int(ready.read_text()),
                    alive_at_gate=consumer.poll() is None, release_absent_at_gate=not release.exists())
        # Retire only the first VIEW. Its Cargo process already ended above.
        for name, package in packages.items():
            link = views[0] / name
            retire_link(link, Path(package["object"]))
            retired.append(str(link))
            gate["retirement_states"].append({"path": str(link), "epoch_ns": time.time_ns(),
                                              "consumer_alive": consumer.poll() is None,
                                              "release_absent": not release.exists()})
        views[0].rmdir()  # Nonrecursive: must now be empty.
        gate.update(retired_ns=time.time_ns(), consumer_alive_after_retirement=consumer.poll() is None,
                    release_absent_after_retirement=not release.exists())
    finally:
        gate["release_ns"] = time.time_ns()
        release.write_text("release\n", encoding="utf-8")
        consumer.wait(timeout=90)
        gate.update(exit=consumer.returncode, completed_ns=time.time_ns(),
                    packages_read_after_release=read_after_release.read_text() if read_after_release.exists() else None)
        active_stdout.close()
        active_stderr.close()
        save(root / "active-consumer-gate.json", gate)
    records.append({"phase": "fresh-test-with-active-view-retirement", "argv": active_argv,
                    "exit": consumer.returncode, "start_ns": active_start, "end_ns": gate["completed_ns"],
                    "stdout": (root / "active-consumer.stdout").read_text(encoding="utf-8"),
                    "stderr": (root / "active-consumer.stderr").read_text(encoding="utf-8")})
    unchanged = all(inventory(Path(package["object"])) == package["inventory"]
                    and inventory(Path(package["source"])) == package["inventory"] for package in packages.values())
    post_read = dict(line.split("=", 1) for line in (gate.get("packages_read_after_release") or "").splitlines())
    read_after_retirement = (gate.get("retired_ns", 0) < gate["release_ns"]
                             < int(post_read.get("epoch_ns", "0")) < gate["completed_ns"]
                             and int(post_read.get("itoa_bytes", "0")) > 0
                             and int(post_read.get("ryu_bytes", "0")) > 0)
    checks = {"all_cargo_commands_succeeded": all(row["exit"] == 0 for row in records),
              "views_share_package_file_ids": all(len({(row["dev"], row["file_id"]) for row in rows}) == 1 for rows in identities.values()),
              "first_view_retired_only": not views[0].exists() and views[1].is_dir(),
              "original_and_copied_packages_unchanged": unchanged,
              "cargo2_alive_and_gated_during_view1_retirement": gate.get("alive_at_gate", False)
                  and gate.get("release_absent_at_gate", False) and gate.get("consumer_alive_after_retirement", False)
                  and gate.get("release_absent_after_retirement", False)
                  and all(row["consumer_alive"] and row["release_absent"] for row in gate["retirement_states"]),
              "cargo2_read_packages_after_release": read_after_retirement,
              "remaining_view_links_valid": all(is_view_link(views[1] / name) for name in packages)}
    result = {"verdict": "PASS" if all(checks.values()) else "FAIL", "checks": checks,
              "root": str(root), "source_vendor": str(source), "requested_toolchain": args.toolchain,
              "link_kind": "junction" if os.name == "nt" else "symlink",
              "cargo": invoke([tools["cargo"], "-vV"], env, root)["stdout"],
              "rustc": invoke([tools["rustc"], "-vV"], env, root)["stdout"],
              "unique_package_source_bytes": sum(p["inventory"]["bytes"] for p in packages.values()),
              "packages": packages, "file_identities": identities, "retired_view_links": retired,
              "commands": records, "active_consumer_gate": gate,
              "limits": ["Read-only file attributes are not an ACL or hostile-writer security boundary",
                         "No package-object deletion attempted; lease/publication protocol not implemented",
                         "File IDs prove shared objects, not total allocated-byte measurement",
                         "Only tiny pure-Rust registry crates plus a controlled consumer build-script gate; no native dependency scripts, git deps, IDE or large union benchmark"]}
    save(root / "results.json", result)
    print(json.dumps({"verdict": result["verdict"], "root": str(root), "checks": checks,
                      "unique_package_source_bytes": result["unique_package_source_bytes"]}, indent=2))
    return 0 if result["verdict"] == "PASS" else 1


if __name__ == "__main__":
    raise SystemExit(main())
