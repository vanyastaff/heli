#!/usr/bin/env python3
"""Windows two-project sidecar; joins an owned FLUI acceptance lab. Default is prepare-only."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time

sys.dont_write_bytecode = True
HERE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("flui_acceptance", HERE / "flui-worktree-acceptance.py")
BASE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(BASE)

BUILD = '''fn main() {
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(out.join("asset"), std::env::var("CARGO_PKG_NAME").unwrap()).unwrap();
}
'''
TEST = r'''#[cfg(test)] mod tests {
    fn escaped(s: &str) -> String {
        s.chars().flat_map(|c| match c {
            '\\' => "\\\\".chars().collect::<Vec<_>>(),
            '"' => "\\\"".chars().collect(),
            c if c.is_control() => format!("\\u{:04x}", c as u32).chars().collect(),
            c => vec![c],
        }).collect()
    }
    #[test] fn gated_late_asset_read() {
        let gate = std::path::PathBuf::from(std::env::var_os("SIDECAR_GATE_DIR").unwrap());
        let out = env!("OUT_DIR");
        let asset = std::path::Path::new(out).join("asset");
        assert_eq!(std::fs::read_to_string(&asset).unwrap(), env!("CARGO_PKG_NAME"));
        let ready = format!("{{\"HELI_RUN\":\"{}\",\"CARGO_HOME\":\"{}\",\"OUT_DIR\":\"{}\"}}",
            escaped(&std::env::var("HELI_RUN").unwrap()),
            escaped(&std::env::var("CARGO_HOME").unwrap()), escaped(out));
        let temp = gate.join("ready.tmp");
        std::fs::write(&temp, ready).unwrap();
        std::fs::rename(temp, gate.join("ready.json")).unwrap();
        let started = std::time::Instant::now();
        while !gate.join("release").is_file() {
            assert!(started.elapsed().as_secs() < 180, "sidecar release deadline");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(std::fs::read_to_string(asset).unwrap(), env!("CARGO_PKG_NAME"));
    }
}
'''


def discover(live):
    result = json.loads((live / "results.json").read_text())
    doctor = json.loads((live / "status-before.json").read_text())
    state = BASE.canonical_path(result["native_homes"]["a"]["home"]).parent.parent
    log = (state / "daemon-1.log").read_text()
    starts = re.findall(r"\[(\d+)\] started, pid (\d+), protocol 2, endpoint (.+)", log)
    if not starts:
        raise RuntimeError("No explicit owned daemon endpoint; refusing to guess")
    owner, pid, endpoint = starts[-1]
    if owner != pid or int(pid) != doctor["daemon"]["pid"]:
        raise RuntimeError("Owned endpoint evidence differs from live baseline daemon")
    pools = re.findall(r"\[" + pid + r"\] pool ([^:]+):", log)
    if not pools:
        raise RuntimeError("No explicit owned pool evidence")
    store = BASE.canonical_path(result["managed_cache_root_marker"]["root"])
    BASE.validate_owned_marker(store, result["managed_cache_root_marker"])
    if os.name != "nt":
        raise RuntimeError("This bounded sidecar currently requires verified Windows pool evidence")
    if endpoint != "\\\\.\\pipe\\" + pools[-1]:
        raise RuntimeError("Owned endpoint/pool evidence inconsistent")
    return result, state, store, int(pid), endpoint, pools[-1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--live", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--run", action="store_true")
    args = parser.parse_args()
    live, root = BASE.canonical_path(args.live), BASE.canonical_path(args.output)
    previous, state, store, daemon_pid, endpoint, pool = discover(live)
    if root.exists() or root.is_relative_to(live) or root.is_relative_to(state):
        raise RuntimeError("Sidecar output must be NEW and separate from live evidence/state")
    if not args.run:
        print(json.dumps({"status": "PREPARED_ONLY", "daemon_pid": daemon_pid,
                          "output": str(root), "projects": 2, "no_processes_started": True}))
        return 0
    root.mkdir(parents=True, exist_ok=False)
    (root / "logs").mkdir()
    result = {"status": "RUNNING", "commands": [], "checks": {}, "ready": {}}
    env = {k: v for k, v in os.environ.items() if not k.startswith("HELI")}
    original_home = BASE.canonical_path(env.get("CARGO_HOME", str(Path.home() / ".cargo")))
    for key in ("CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR", "CARGO_BUILD_BUILD_DIR",
                "CARGO_MAKEFLAGS", "MAKEFLAGS"):
        env.pop(key, None)
    env |= {"HELI_STATE_DIR": str(state), "HELI_CONFIG": str(live / "heli.toml"),
            "HELI_ACTIVITY_DIR": str(live / "activity"), "HELI_ENDPOINT": endpoint,
            "HELI_POOL": pool, "CARGO_HOME": str(original_home), "CARGO_INCREMENTAL": "0"}
    heli = previous["heli_binary"]
    children, handles, run_ids = [], [], {}
    before = BASE.readonly_inputs(original_home, [])
    result["original_input_digests_before"] = before

    def save():
        BASE.HELPER.emit(root / "results.json", result)

    def launch(label, argv, cwd=root, child_env=env):
        handle = (root / "logs" / (label + ".log")).open("wb")
        handles.append(handle)
        process = subprocess.Popen([str(v) for v in argv], cwd=cwd, env=child_env,
                                   stdout=handle, stderr=subprocess.STDOUT,
                                   **BASE.HELPER.PROCESS_OPTIONS)
        children.append(process)
        result["commands"].append({"label": label, "argv": list(map(str, argv)),
                                   "pid": process.pid, "started_ns": time.time_ns()})
        save()
        return process

    def command(label, argv, cwd=root, child_env=env, timeout=20):
        process = launch(label, argv, cwd, child_env)
        code = process.wait(timeout=timeout)
        result["commands"][-1].update(exit=code, ended_ns=time.time_ns())
        if code:
            raise RuntimeError(label + " failed")
        return (root / "logs" / (label + ".log")).read_text(encoding="utf-8", errors="replace")

    def query(label, argv):
        doc, diagnostics = BASE.parse_cli_json(command(label, [heli, *argv]))
        if diagnostics:
            raise RuntimeError("Unexpected sidecar CLI diagnostic")
        BASE.HELPER.emit(root / (label + ".json"), doc)
        return doc

    main_worktrees = {BASE.canonical_path(previous["commands"][i]["cwd"])
                      for i in range(len(previous["commands"]))
                      if previous["commands"][i]["label"] in ("a-test", "b-test")}

    def active_paths(status):
        return {BASE.canonical_path(row["worktree"]) for row in status["runs"]
                if row.get("ended_at") is None and row.get("exit") is None
                and row.get("phase") in ("compiling", "testing", "running")}

    try:
        initial = query("status-initial", ["status", "--json"])
        if initial["daemon"]["pid"] != daemon_pid or len(main_worktrees) != 2:
            raise RuntimeError("Live daemon/worktree identity changed")
        if not main_worktrees <= active_paths(initial):
            raise RuntimeError("Both original FLUI worktrees must still be active before sidecar")
        native_cargo = previous["toolchains"]["a"]["cargo"]
        native_rustc = previous["toolchains"]["a"]["rustc"]
        homes = {BASE.canonical_path(value["home"]) for value in previous["native_homes"].values()}
        clients, repositories = {}, {}
        hooks = root / "empty-hooks"
        hooks.mkdir()
        for name in ("c", "d"):
            repo = root / name
            repo.mkdir()
            (repo / "src").mkdir()
            (repo / "gate").mkdir()
            package = "sidecar_" + name
            (repo / "Cargo.toml").write_text('[package]\nname="' + package + '"\nversion="0.1.0"\nedition="2024"\n', encoding="utf-8")
            (repo / "Cargo.lock").write_text('version = 4\n\n[[package]]\nname = "' + package + '"\nversion = "0.1.0"\n', encoding="utf-8")
            (repo / "build.rs").write_text(BUILD, encoding="utf-8")
            (repo / "src/lib.rs").write_text(TEST, encoding="utf-8")
            command("git-init-" + name, ["git", "-c", "core.hooksPath=" + str(hooks), "init", str(repo)])
            child_env = env | {"CARGO": native_cargo, "RUSTC": native_rustc,
                               "CARGO_TARGET_DIR": str(repo / "target"),
                               "CARGO_BUILD_BUILD_DIR": str(store / "{workspace-path-hash}"),
                               "SIDECAR_GATE_DIR": str(repo / "gate")}
            text = command("home-" + name, [heli, "--cache", "off", "--output", "full", "exec", "--",
                           sys.executable, "-c", "import os,json;print(json.dumps({'home':os.environ['CARGO_HOME']}))"], repo, child_env)
            home = BASE.canonical_path(json.loads(next(line for line in text.splitlines() if line.startswith('{"home":')))["home"])
            if home in homes or not home.is_relative_to(BASE.canonical_path(state)):
                raise RuntimeError("Sidecar native home is shared or escapes owned state")
            homes.add(home)
            repositories[name] = (repo, child_env, home)
        for name, (repo, child_env, _) in repositories.items():
            clients[name] = launch("test-" + name, [heli, "--cache", "off", "--output", "full",
                "test", "--offline", "--locked", "--", "--test-threads=1"], repo, child_env)
        deadline = time.monotonic() + 120
        while len(result["ready"]) < 2:
            for name, (repo, _, home) in repositories.items():
                ready = repo / "gate/ready.json"
                if ready.is_file() and name not in result["ready"]:
                    doc = json.loads(ready.read_text())
                    if set(doc) != {"HELI_RUN", "CARGO_HOME", "OUT_DIR"} or not re.fullmatch(r"r\d+", doc["HELI_RUN"]):
                        raise RuntimeError("Invalid readiness protocol")
                    if not BASE.same_existing(doc["CARGO_HOME"], home):
                        raise RuntimeError("Test changed selected native home")
                    out = BASE.canonical_path(doc["OUT_DIR"])
                    if not out.is_relative_to(BASE.canonical_path(store)) or (out / "asset").read_text() != "sidecar_" + name:
                        raise RuntimeError("Asset escaped owned store or changed")
                    result["ready"][name] = doc
                    run_ids[name] = doc["HELI_RUN"]
                if clients[name].poll() is not None:
                    raise RuntimeError("Sidecar exited before gated proof")
            if time.monotonic() > deadline:
                raise TimeoutError("Sidecar readiness deadline")
            time.sleep(.1)
        status = query("status-four-worktrees", ["status", "--json"])
        expected = main_worktrees | {value[0] for value in repositories.values()}
        result["checks"]["four_active_worktrees_three_projects"] = expected <= active_paths(status)
        if not result["checks"]["four_active_worktrees_three_projects"]:
            raise RuntimeError("Did not observe four active worktrees across three projects")
        gc = query("gc-while-gated", ["gc", "--json"])
        result["checks"]["busy_gc_preserved_all"] = gc["runtime_busy"] and not gc["victims"]
        if not result["checks"]["busy_gc_preserved_all"]:
            raise RuntimeError("Busy GC did not preserve active artifacts")
        result["checks"]["late_assets_intact_after_gc"] = all(
            (BASE.canonical_path(doc["OUT_DIR"]) / "asset").read_text() == "sidecar_" + name
            for name, doc in result["ready"].items())
        if not result["checks"]["late_assets_intact_after_gc"]:
            raise RuntimeError("GC changed gated sidecar assets")
        for name, (repo, _, _) in repositories.items():
            (repo / "gate/release").write_text("release", encoding="utf-8")
        result["client_exits"] = {name: process.wait(timeout=35) for name, process in clients.items()}
        after = query("status-completed", ["status", "--json"])
        result["checks"]["sidecars_completed_while_flui_active"] = (all(value == 0 for value in result["client_exits"].values())
                                                                           and main_worktrees <= active_paths(after))
        result["original_input_digests_after"] = BASE.readonly_inputs(original_home, [])
        result["checks"]["original_inputs_unchanged"] = before == result["original_input_digests_after"]
        result["checks"]["four_distinct_native_homes"] = len(homes) == 4
        result["status"] = "PASS" if all(result["checks"].values()) else "FAIL"
    except Exception as error:
        result.update(status="FAIL", error=repr(error))
    finally:
        # Release only our gates, then cancel only exact known run/cwd pairs if needed.
        for name in ("c", "d"):
            gate = root / name / "gate"
            if gate.is_dir():
                (gate / "release").write_text("cleanup release", encoding="utf-8")
        try:
            current = query("status-cleanup", ["status", "--json"])
            for row in current["runs"]:
                name = next((key for key in ("c", "d")
                             if BASE.canonical_path(row["worktree"]) == root / key
                             and row.get("class") == "test"), None)
                if name:
                    previous_id = run_ids.get(name)
                    if previous_id and previous_id != row["id"]:
                        raise RuntimeError("Sidecar cleanup run identity changed")
                    run_ids[name] = row["id"]
                if (name and BASE.canonical_path(row["worktree"]) == root / name
                        and row.get("ended_at") is None):
                    command("cancel-" + name, [heli, "cancel", row["id"]])
        except Exception as error:
            result["cleanup_error"] = repr(error)
        # Never kill daemon, guardian, compiler, or any A/B client. Only owned direct clients.
        for process in children:
            if process.poll() is None:
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.terminate()
                    process.wait(timeout=5)
        for handle in handles:
            handle.close()
        save()
    print(json.dumps({"status": result["status"], "checks": result["checks"], "evidence": str(root / "results.json")}))
    return 0 if result["status"] == "PASS" else 1


if __name__ == "__main__":
    raise SystemExit(main())
