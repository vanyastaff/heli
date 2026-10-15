#!/usr/bin/env python3
"""Isolated public-CLI experiment; run ONLY after freezing the heli binary.

Example (PowerShell):
  python experiments/multi-project-spike.py --heli E:/heli/target/debug/cargo-heli.exe

No global configuration or existing cache is changed. Worktree sources are not
edited. Generated Cargo fixtures, compiler helper, configuration and telemetry
live below a brand-new --output path. Existing paths are never reused/deleted.
The helper adds measurement overhead; this is acceptance evidence, not a speed
benchmark. No claim is made about project-specific task-runner integration.
"""
import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time
import tomllib
import uuid

GIB = 1024 ** 3
CREATE_FLAGS = subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0


def emit(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def command(args, cwd, env, timeout=90):
    result = subprocess.run([str(a) for a in args], cwd=cwd, env=env,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            text=True, encoding="utf-8", errors="replace",
                            timeout=timeout, creationflags=CREATE_FLAGS)
    return {"args": [str(a) for a in args], "exit": result.returncode,
            "stdout": result.stdout, "stderr": result.stderr}


def checked(args, cwd, env, timeout=90):
    result = command(args, cwd, env, timeout)
    if result["exit"]:
        raise RuntimeError(json.dumps(result))
    return result["stdout"].strip()


def physical_memory():
    if os.name == "nt":
        class Memory(ctypes.Structure):
            _fields_ = [("length", ctypes.c_ulong), ("load", ctypes.c_ulong)] + [
                (name, ctypes.c_ulonglong) for name in (
                    "total_physical", "available_physical", "total_page_file",
                    "available_page_file", "total_virtual", "available_virtual",
                    "available_extended_virtual")]
        data = Memory()
        data.length = ctypes.sizeof(data)
        if not ctypes.windll.kernel32.GlobalMemoryStatusEx(ctypes.byref(data)):
            return {"error": "GlobalMemoryStatusEx failed"}
        # Page-file figures here are not treated as system CommitLimit/Total.
        return {"physical_total": data.total_physical,
                "physical_available": data.available_physical}
    try:
        values = dict(line.split(":", 1) for line in Path("/proc/meminfo").read_text().splitlines())
        return {"physical_total": int(values["MemTotal"].split()[0]) * 1024,
                "physical_available": int(values["MemAvailable"].split()[0]) * 1024}
    except (OSError, KeyError):
        return {"error": "physical memory telemetry unavailable"}


def fixture(root, payload_mib):
    """Runtime-generated input, independent of both real source repositories."""
    root.mkdir(parents=True)
    (root / "src").mkdir()
    (root / "Cargo.toml").write_text(
        '[package]\nname="spike-nested-outer"\nversion="0.0.0"\nedition="2024"\n[workspace]\n', encoding="utf-8")
    (root / "src/lib.rs").write_text('''#[test]
fn nested_cargo() {
    let status = std::process::Command::new(std::env::var_os("CARGO").unwrap())
        .args(["check", "--manifest-path", "child/Cargo.toml", "--offline"])
        .status().unwrap();
    assert!(status.success());
}
''', encoding="utf-8")
    child = root / "child"
    (child / "src").mkdir(parents=True)
    (child / "Cargo.toml").write_text(
        '[package]\nname="spike-nested-child"\nversion="0.0.0"\nedition="2024"\nbuild="build.rs"\n[workspace]\n', encoding="utf-8")
    (child / "src/lib.rs").write_text('pub fn value() -> u32 { 42 }\n', encoding="utf-8")
    (child / "build.rs").write_text('''use std::{env, fs::File, io::Write, path::PathBuf};
fn main() {
    let mut file = File::create(PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("payload.bin")).unwrap();
    let block = vec![0x5a; 1024 * 1024];
    for _ in 0..PAYLOAD_MIB { file.write_all(&block).unwrap(); }
    file.sync_all().unwrap();
    println!("cargo:rerun-if-changed=build.rs");
}
'''.replace("PAYLOAD_MIB", str(payload_mib)), encoding="utf-8")


class Spike:
    def __init__(self, args):
        self.args = args
        self.root = Path(args.output).resolve()
        self.heli = Path(args.heli).resolve(strict=True)
        self.repositories = {"h": Path(args.heli_repo).resolve(strict=True),
                             "f": Path(args.flui_repo).resolve(strict=True)}
        self.base = os.environ.copy()
        for key in ("RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER",
                    "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER"):
            if self.base.get(key):
                raise RuntimeError(f"Refusing to replace existing {key}; rerun with an explicitly scoped clean environment")
        config_dirs = {Path(self.base.get("CARGO_HOME", Path.home() / ".cargo"))}
        for repository in self.repositories.values():
            config_dirs.update(parent / ".cargo" for parent in (repository, *repository.parents))
        for directory in config_dirs:
            for filename in ("config", "config.toml"):
                path = directory / filename
                if path.is_file():
                    config = tomllib.loads(path.read_text(encoding="utf-8"))
                    build = config.get("build", {})
                    if build.get("rustc-wrapper") or build.get("rustc-workspace-wrapper"):
                        raise RuntimeError(f"Refusing to replace compiler wrapper from {path}")
        # All of these could redirect into a user's pool/store, so replace them.
        for key in ("HELI_ENDPOINT", "HELI_RUN", "HELI_OUTPUT", "CARGO_MAKEFLAGS", "MAKEFLAGS", "MFLAGS",
                    "CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR", "CARGO_BUILD_BUILD_DIR",
                    "CARGO_BUILD_JOBS", "FLUI_XTASK_NO_LOCK", "FLUI_XTASK_LOCK_HELD"):
            self.base.pop(key, None)
        self.base["CARGO_INCREMENTAL"] = "0"
        self.token = uuid.uuid4().hex[:12]
        self.worktrees = []
        self.clients = []
        self.samples = []
        self.gc_processes = []
        self.daemon = None
        self.lock_probe = None
        self.manifest = {}

    def heli_command(self, *args):
        return [str(self.heli), *args]

    def query(self, name, args, timeout=90):
        result = command(self.heli_command(*args), self.root, self.base, timeout)
        try:
            result["document"] = json.loads(result["stdout"])
        except ValueError:
            pass
        emit(self.root / f"{name}.json", result)
        return result

    def prepare(self):
        # Exclusive creation: never repurpose a prior experiment directory.
        self.root.mkdir(parents=True, exist_ok=False)
        for name in ("events", "logs", "w", "hooks", "fixtures"):
            (self.root / name).mkdir()
        store = self.root / "store"  # Leave absent: heli must claim a NEW root.
        unmarked = self.root / "preexisting-store"
        sentinel = unmarked / "aa" / "bb" / "debug" / "deps" / "sentinel.bin"
        sentinel.parent.mkdir(parents=True)
        sentinel.write_bytes(b"Unmarked UNKNOWN directory must survive whole-directory GC.\n")
        (sentinel.parent.parent / ".cargo-build-lock").touch()
        self.sentinel = (sentinel, digest(sentinel))
        config = self.root / "config.toml"
        config.write_text(f'''[pool]
slots = 8
[memory]
low_watermark_mb = 12288
high_watermark_mb = 16384
[event_log]
max_kb = 65536
[store]
paths = ['{store.as_posix()}', '{unmarked.as_posix()}']
budget_mb = {self.args.budget_mib}
floor_mb = 51200
unit_hours = 0
orphan_hours = 0
idle_hours = 0
dir_hours = 0
''', encoding="utf-8")
        self.base.update(HELI_STATE_DIR=str(self.root / "state"), HELI_CONFIG=str(config),
                         HELI_POOL=(f"heli-spike-{self.token}" if os.name == "nt" else str(self.root / "pool")),
                         CARGO_BUILD_BUILD_DIR=str(store / "{workspace-path-hash}"),
                         HELI_SPIKE_EVENTS=str(self.root / "events"))
        helper = self.root / ("rustc-observer.exe" if os.name == "nt" else "rustc-observer")
        source = Path(__file__).with_name("spike-rustc-wrapper.rs").resolve()
        checked(["rustc", "--edition=2024", str(source), "-o", str(helper)], self.root, self.base)
        self.base["RUSTC_WRAPPER"] = str(helper)
        refs = {"h": self.args.heli_ref, "f": self.args.flui_ref}
        for prefix, repository in self.repositories.items():
            sha = checked(["git", "rev-parse", refs[prefix] + "^{commit}"], repository, self.base)
            self.manifest[prefix] = {"repository": str(repository), "sha": sha}
            for index in (1, 2):
                path = self.root / "w" / f"{prefix}{index}"
                checked(["git", "-c", f"core.hooksPath={self.root / 'hooks'}", "worktree", "add", "--detach", str(path), sha],
                        repository, self.base, timeout=120)
                self.worktrees.append((repository, path))
        self.manifest.update(binary=str(self.heli), binary_sha256=digest(self.heli),
                             wrapper_source_sha256=digest(source),
                             rustc_version=checked(["rustc", "-vV"], self.root, self.base),
                             cargo_version=checked(["cargo", "-vV"], self.root, self.base),
                             budget_bytes=self.args.budget_mib * 1024 ** 2,
                             floor_bytes=50 * GIB, pool_slots=8,
                             memory_watermarks_mib=[12288, 16384],
                             cargo_incremental="0", store=str(store),
                             result_cache=self.args.result_cache,
                             eviction_ages_hours=0,
                             note="Accelerated ages test reclamation safety/convergence; not normal retention defaults.")
        emit(self.root / "manifest.json", self.manifest)
        log = (self.root / "logs/daemon.txt").open("wb")
        self.daemon = subprocess.Popen(self.heli_command("daemon"), cwd=self.root,
                                       env=self.base, stdout=log, stderr=subprocess.STDOUT,
                                       creationflags=CREATE_FLAGS)
        self.daemon_log = log
        checked(self.heli_command("ensure"), self.root, self.base)
        self.query("gc-before", ["gc", "--dry-run", "--json"])
        self.sample()

    def start(self, name, cwd, args, extra_env=None):
        env = self.base.copy()
        env.update(CARGO_TARGET_DIR=str(cwd / "target"), HELI_SPIKE_AGENT=name)
        env.update(extra_env or {})
        log = (self.root / "logs" / f"{name}.txt").open("wb")
        start = time.time_ns()
        process = subprocess.Popen(self.heli_command("--cache", self.args.result_cache, "--output", "full", *args),
                                   cwd=cwd, env=env, stdout=log, stderr=subprocess.STDOUT,
                                   creationflags=CREATE_FLAGS)
        entry = {"agent": name, "cwd": str(cwd), "args": args, "submitted_ns": start,
                 "pid": process.pid, "process": process, "log_handle": log, "exit": None}
        self.clients.append(entry)
        return entry

    def host_lock_probe(self):
        cwd = self.root / "w/f1"
        build = self.start("xtask-build", cwd, ["build", "-p", "xtask", "--bin", "xtask", "--locked", "--offline"])
        self.wait_clients([build])
        if build["exit"] != 0:
            raise RuntimeError("Cannot build real FLUI xtask for host-lock integration probe")
        probe = self.root / "host-lock-probe"
        probe.mkdir()
        executable = probe / ("xtask.exe" if os.name == "nt" else "xtask")
        shutil.copy2(cwd / "target/debug" / executable.name, executable)
        lock = probe / "host.lock"
        ready = probe / "holder-ready"
        release = probe / "holder-release"
        helper = self.base["RUSTC_WRAPPER"]
        holder_log = (probe / "holder.log").open("wb")
        holder = subprocess.Popen([helper, "--spike-hold-lock", str(lock), str(ready), str(release)],
                                  cwd=probe, env=self.base, stdout=holder_log, stderr=subprocess.STDOUT,
                                  creationflags=CREATE_FLAGS)
        bare = None
        bare_log = None
        result = {"xtask_sha256": digest(executable), "command": [str(executable), "clean-nested"]}
        try:
            deadline = time.monotonic() + 8
            while not ready.exists():
                if holder.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError("Isolated host-lock holder did not become ready")
                time.sleep(0.02)
            env = self.base.copy()
            env.update(CARGO_TARGET_DIR=str(probe / "empty-target"), FLUI_XTASK_LOCK_FILE=str(lock))
            env.pop("HELI_RUN", None)
            bare_log = (probe / "bare.log").open("wb")
            bare = subprocess.Popen([str(executable), "clean-nested"], cwd=cwd, env=env,
                                    stdout=bare_log, stderr=subprocess.STDOUT, creationflags=CREATE_FLAGS)
            time.sleep(1)
            result["bare_blocked_with_lock_held"] = bare.poll() is None and holder.poll() is None
            started = time.monotonic()
            managed = self.start("xtask-held-lock", cwd, ["exec", "--", str(executable), "clean-nested"],
                                 {"CARGO_TARGET_DIR": str(probe / "empty-target"), "FLUI_XTASK_LOCK_FILE": str(lock)})
            try:
                managed["process"].wait(timeout=8)
                managed["exit"] = managed["process"].returncode
                managed["completed_ns"] = time.time_ns()
                result["managed_completed_with_lock_held"] = holder.poll() is None and managed["exit"] == 0
            except subprocess.TimeoutExpired:
                result["managed_completed_with_lock_held"] = False
            result["managed_elapsed_seconds_before_release"] = time.monotonic() - started
            result["bare_still_blocked_before_release"] = bare.poll() is None
        finally:
            # Release only our isolated lock; never terminate a foreign holder.
            release.write_text("release\n", encoding="utf-8")
            holder.wait(timeout=10)
            holder_log.close()
            if bare:
                bare.wait(timeout=15)
                result["bare_exit_after_release"] = bare.returncode
            if bare_log:
                bare_log.close()
            for entry in self.clients:
                if entry["agent"] == "xtask-held-lock":
                    entry["process"].wait(timeout=15)
                    entry["exit"] = entry["process"].returncode
                    entry.setdefault("completed_ns", time.time_ns())
                    entry["log_handle"].close()
            self.lock_probe = result
            emit(self.root / "host-lock-probe.json", result)

    def sample(self):
        started = time.time_ns()
        result = command(self.heli_command("status", "--json"), self.root, self.base, timeout=10)
        row = {"epoch_ns": started, "host": physical_memory(),
               "volume_free": shutil.disk_usage(self.root).free,
               "status_exit": result["exit"]}
        try:
            row["status"] = json.loads(result["stdout"])
        except ValueError:
            row["status_error"] = result
        self.samples.append(row)
        with (self.root / "samples.jsonl").open("a", encoding="utf-8") as stream:
            stream.write(json.dumps(row) + "\n")

    def gc_background(self, label):
        log = (self.root / f"gc-{label}.json").open("wb")
        process = subprocess.Popen(self.heli_command("gc", "--json"), cwd=self.root,
                                   env=self.base, stdout=log, stderr=subprocess.STDOUT,
                                   creationflags=CREATE_FLAGS)
        self.gc_processes.append((process, log))

    def wait_clients(self, selected):
        deadline = time.monotonic() + self.args.timeout
        next_gc = time.monotonic() + 8
        next_progress = time.monotonic() + 10
        while any(entry["exit"] is None for entry in selected):
            for entry in selected:
                if entry["exit"] is None and entry["process"].poll() is not None:
                    entry["exit"] = entry["process"].returncode
                    entry["completed_ns"] = time.time_ns()
                    entry["log_handle"].close()
                    print(f"{entry['agent']}: exit {entry['exit']}", flush=True)
            self.sample()
            now = time.monotonic()
            if now > deadline:
                raise TimeoutError("Owned workload exceeded timeout; see retained run logs")
            if now > next_gc:
                if not any(p.poll() is None for p, _ in self.gc_processes):
                    self.gc_background(f"during-{len(self.gc_processes)}")
                next_gc = now + 15
            if now > next_progress:
                print("active: " + ", ".join(e["agent"] for e in selected if e["exit"] is None), flush=True)
                next_progress = now + 10
            time.sleep(1)
        for process, log in self.gc_processes:
            process.wait(timeout=120)
            log.close()

    def load(self):
        real = [self.start("heli-check", self.root / "w/h1", ["check", "--tests", "--locked", "--offline"]),
                self.start("heli-test", self.root / "w/h2", ["test", "--bin", "cargo-heli", "--locked", "--offline"]),
                self.start("flui-test", self.root / "w/f2", ["test", "-p", "flui-foundation", "--lib", "--locked", "--offline"])]
        # The fourth, short check arrives once actual compiler work began. It
        # uses its own worktree, never intentionally queues on an active target.
        deadline = time.monotonic() + 10
        while not any((self.root / "events").glob("*-start.json")) and time.monotonic() < deadline:
            self.sample()
            time.sleep(0.1)
        real.append(self.start("flui-check", self.root / "w/f1", ["check", "-p", "flui-foundation", "--lib", "--locked", "--offline"]))
        self.wait_clients(real)
        self.host_lock_probe()
        self.query("gc-after-real", ["gc", "--json"])
        # Repeated fresh nested workspaces force cumulative artifact growth above
        # B. Each child inherits the store template but is not itself a heli client.
        for cycle in range(self.args.cycles):
            batch = []
            for index in range(2):
                root = self.root / "fixtures" / f"c{cycle}-{index}"
                fixture(root, self.args.payload_mib)
                batch.append(self.start(f"nested-{cycle}-{index}", root, ["test", "--offline"]))
            self.wait_clients(batch)
            self.query(f"gc-cycle-{cycle}", ["gc", "--json"])
        self.query("gc-final-pass", ["gc", "--json"])
        # gc's document describes its pre-eviction survey. Freshly survey after
        # deletion, rather than reporting pre-GC bytes as a post-GC failure.
        self.final_gc = self.query("gc-final", ["gc", "--dry-run", "--json"])
        self.sample()

    def summary(self, error=None):
        events = []
        for path in (self.root / "events").glob("*.json"):
            events.append(json.loads(path.read_text(encoding="utf-8")))
        emit(self.root / "compiler-events.json", sorted(events, key=lambda event: event["epoch_ns"]))
        rows = []
        intervals = []
        for entry in self.clients:
            starts = [e for e in events if e["agent"] == entry["agent"] and e["phase"] == "start"]
            first = min((e["epoch_ns"] for e in starts), default=None)
            rows.append({key: value for key, value in entry.items() if key not in ("process", "log_handle")}
                        | {"first_rustc_ns": first,
                           "first_rustc_seconds": (first-entry["submitted_ns"]) / 1e9 if first else None,
                           "compiler_starts": len(starts)})
        ends = {(e["pid"], e["invocation"]): e for e in events if e["phase"] == "end"}
        real_names = {"heli-check", "heli-test", "flui-check", "flui-test"}
        for event in events:
            key = (event["pid"], event["invocation"])
            if event["phase"] == "start" and event["agent"] in real_names and key in ends:
                intervals.append((event["epoch_ns"], 1, event["agent"]))
                intervals.append((ends[key]["epoch_ns"], -1, event["agent"]))
        active = {}
        peak_agents = 0
        cross_repository_overlap = False
        for _, change, agent in sorted(intervals):
            active[agent] = active.get(agent, 0) + change
            peak_agents = max(peak_agents, sum(value > 0 for value in active.values()))
            prefixes = {name.split("-", 1)[0] for name, count in active.items() if count > 0}
            cross_repository_overlap |= {"heli", "flui"}.issubset(prefixes)
        late = next((entry["submitted_ns"] for entry in self.clients if entry["agent"] == "flui-check"), None)
        late_during_compile = any(event["phase"] == "start" and event["agent"] != "flui-check"
                                  and (event["pid"], event["invocation"]) in ends
                                  and event["epoch_ns"] <= late <= ends[(event["pid"], event["invocation"])]["epoch_ns"]
                                  for event in events) if late is not None else False
        memories = [s["host"]["physical_available"] for s in self.samples if "physical_available" in s["host"]]
        commits = [s["status"].get("memory", {}).get("commit_available")
                   for s in self.samples if s.get("status", {}).get("memory")]
        commits = [value for value in commits if value is not None]
        final = getattr(self, "final_gc", {}).get("document", {})
        used = final.get("used")
        budget = final.get("budget")
        checks = {
            "all_runs_succeeded": bool(rows) and all(row["exit"] == 0 for row in rows),
            "four_real_agents_observed_compiling": all(any(e["agent"] == name and e["phase"] == "start" for e in events) for name in real_names),
            "actual_compiler_overlap": peak_agents >= 2,
            "cross_repository_compiler_overlap": cross_repository_overlap,
            "late_short_check_arrived_during_other_compilation": late_during_compile,
            "first_compiler_under_10_seconds": all(row["first_rustc_seconds"] is not None and row["first_rustc_seconds"] <= 10 for row in rows if row["agent"] in real_names),
            "all_compilers_managed": bool(events) and all(e["heli_run"] for e in events),
            "idle_gc_budget_met": used is not None and budget is not None and used <= budget,
            "free_space_floor_met": bool(self.samples) and self.samples[-1]["volume_free"] >= 50 * GIB,
            "unrelated_sentinel_preserved": self.sentinel[0].is_file() and digest(self.sentinel[0]) == self.sentinel[1],
            "binary_unchanged": digest(self.heli) == self.manifest["binary_sha256"],
            "physical_and_commit_telemetry_available": bool(memories) and (bool(commits) or os.name != "nt"),
            "managed_xtask_bypasses_held_host_lock": bool(self.lock_probe) and self.lock_probe.get("managed_completed_with_lock_held", False),
            "bare_xtask_respects_held_host_lock": bool(self.lock_probe) and self.lock_probe.get("bare_blocked_with_lock_held", False)
                and self.lock_probe.get("bare_still_blocked_before_release", False) and self.lock_probe.get("bare_exit_after_release") == 0,
        }
        lock_lines = []
        for path in (self.root / "logs").glob("*.txt"):
            for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
                if "Blocking waiting for file lock" in line:
                    lock_lines.append({"log": path.name, "line": line})
        result = {"acceptance": "PASS" if all(checks.values()) and error is None else "FAIL",
                  "checks": checks, "error": error, "runs": rows,
                  "peak_distinct_real_agents_in_compiler_intervals": peak_agents,
                  "minimum_available_physical": min(memories) if memories else None,
                  "minimum_available_commit": min(commits) if commits else None,
                  "final_gc_used": used, "final_gc_budget": budget,
                  "cargo_file_lock_wait_messages": lock_lines,
                  "host_lock_probe": self.lock_probe,
                  "limitations": ["Windows local workload; not cross-platform acceptance",
                                  "FLUI host-lock probe uses real prebuilt xtask and an isolated lock, not every project runner",
                                  "Observed headroom is not a universal hard RAM cap",
                                  "First compiler under 10 seconds is this workload's target, not a universal guarantee; failures need cold metadata/resolution and lock diagnosis",
                                  "Accelerated GC ages are zero; ordinary retention not tested",
                                  "Compiler instrumentation adds overhead; no speed comparison baseline"]}
        emit(self.root / "result.json", result)
        lines = ["# Multi-project heli experiment", "", f"Result: **{result['acceptance']}**", "",
                 "| Check | Result |", "|---|---|"]
        lines.extend(f"| {name} | {'PASS' if value else 'FAIL'} |" for name, value in checks.items())
        lines.extend(["", "| Agent | Exit | First actual compiler (s) | Compiler starts |", "|---|---:|---:|---:|"])
        for row in rows:
            first = row["first_rustc_seconds"]
            lines.append(f"| {row['agent']} | {row['exit']} | {first:.3f} | {row['compiler_starts']} |" if first is not None
                         else f"| {row['agent']} | {row['exit']} | unavailable | {row['compiler_starts']} |")
        lines += ["", f"Peak distinct real agents in compiler intervals: {peak_agents}.",
                  f"Minimum available physical bytes: {result['minimum_available_physical']}.",
                  f"Minimum available commit bytes: {result['minimum_available_commit']}.",
                  f"Final GC accounted bytes: {used}; budget: {budget}.", "", "Limitations:", ""]
        lines.insert(-3, f"Cargo file-lock wait log messages: {len(lock_lines)} (not a claim that no other locks exist).")
        lines += ["- " + value for value in result["limitations"]]
        if error:
            lines += ["", "Experiment error: " + error]
        (self.root / "RESULTS.md").write_text("\n".join(lines) + "\n", encoding="utf-8")
        return result

    def cleanup(self):
        # Cancel only this isolated daemon's known runs; never global process scans.
        for sample in self.samples[-1:]:
            for run in sample.get("status", {}).get("runs", []):
                if run.get("id") is not None:
                    command(self.heli_command("cancel", str(run["id"])), self.root, self.base)
        for entry in self.clients:
            if entry["process"].poll() is None:
                entry["process"].wait(timeout=30)
            if not entry["log_handle"].closed:
                entry["log_handle"].close()
        if not self.args.keep_worktrees:
            cleanup = []
            for repository, path in reversed(self.worktrees):
                status = command(["git", "status", "--porcelain"], path, self.base)
                if status["exit"] == 0 and not status["stdout"].strip():
                    cleanup.append(command(["git", "-c", f"core.hooksPath={self.root / 'hooks'}",
                                            "worktree", "remove", str(path)], repository, self.base, 120))
                else:
                    cleanup.append({"path": str(path), "preserved": "dirty or unreadable", "status": status})
            emit(self.root / "worktree-cleanup.json", cleanup)
            self.query("gc-after-retirement", ["gc", "--json"])
        if self.daemon and self.daemon.poll() is None:
            self.daemon.terminate()  # Exact Popen-owned isolated daemon only.
            self.daemon.wait(timeout=30)
        if self.daemon:
            self.daemon_log.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--heli", required=True, help="Frozen cargo-heli binary; never built by this harness")
    parser.add_argument("--heli-repo", default="E:/heli")
    parser.add_argument("--flui-repo", default="D:/flui")
    parser.add_argument("--heli-ref", default="HEAD")
    parser.add_argument("--flui-ref", default="HEAD")
    parser.add_argument("--output", default=f"E:/heli/target/mps-{time.strftime('%m%d-%H%M%S')}-{uuid.uuid4().hex[:4]}")
    parser.add_argument("--budget-mib", type=int, default=2048)
    parser.add_argument("--result-cache", choices=["off", "refresh"], default="off",
                        help="off avoids verdict inventory on the foreground scheduling path")
    parser.add_argument("--payload-mib", type=int, default=512)
    parser.add_argument("--cycles", type=int, default=3)
    parser.add_argument("--timeout", type=int, default=1800)
    parser.add_argument("--keep-worktrees", action="store_true")
    args = parser.parse_args()
    spike = Spike(args)
    error = None
    result = None
    try:
        spike.prepare()
        print(f"Isolated experiment: {spike.root}", flush=True)
        spike.load()
    except Exception as exc:
        error = repr(exc)
        print(error, file=sys.stderr, flush=True)
    finally:
        if spike.manifest.get("binary_sha256"):
            result = spike.summary(error)
        if spike.worktrees or spike.daemon:
            spike.cleanup()
    if result:
        print(f"{result['acceptance']}: {spike.root / 'RESULTS.md'}", flush=True)
    return 0 if result and result["acceptance"] == "PASS" else 1


if __name__ == "__main__":
    raise SystemExit(main())
