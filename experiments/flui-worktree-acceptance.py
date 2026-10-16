#!/usr/bin/env python3
"""Real FLUI worktree acceptance. Nothing runs without --run.

Explicit pinned FLUI commit, two clean Git worktrees in a brand-new lab.
Submit fmt/clippy/check/test for BOTH worktrees concurrently through automatic
native isolated Cargo homes. No vendor/import/prefetch or source edits.
The configurable artifact budget covers tracked artifacts, NOT private registry homes/all disk.
Use --instrument for actual compiler interval evidence; an existing outer
RUSTC_WRAPPER is chained. Configuration wrappers/includes are inspected without
logging config bytes; --original-wrapper can override the recorded selection.
Running requires --instrument so the compiler-overlap acceptance can be proved.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import time
import tomllib
import uuid

sys.dont_write_bytecode = True
REPO = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "heli_real_http", REPO / "experiments/real-download-hol.py")
HELPER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(HELPER)
GIB = 1024 ** 3


def plain_path(value):
    text = os.fspath(value)
    if os.name == "nt":
        if text[:8].casefold() == "\\\\?\\unc\\":
            text = "\\\\" + text[8:]
        elif text.startswith("\\\\?\\"):
            text = text[4:]
    return Path(text)


def canonical_path(value):
    """Resolve existing ancestors, retaining nonexistent Cargo metadata suffixes."""
    path = plain_path(value).absolute()
    suffix = []
    ancestor = path
    while not ancestor.exists():
        if ancestor == ancestor.parent:
            raise RuntimeError("no existing ancestor for filesystem path")
        suffix.append(ancestor.name)
        ancestor = ancestor.parent
    base = plain_path(ancestor.resolve(strict=True))
    return base.joinpath(*reversed(suffix))


def same_existing(left, right):
    try:
        return plain_path(left).samefile(plain_path(right))
    except OSError:
        return False


def parse_cli_json(text, allow_gc_retention=False):
    """One JSON frame plus explicit CLI diagnostics; reject ambiguous extra data."""
    decoder = json.JSONDecoder()
    value, end = decoder.raw_decode(text.lstrip())
    if not isinstance(value, dict) or value.get("schema") != 1:
        raise ValueError("unsupported CLI JSON frame schema")
    diagnostics = []
    for line in text.lstrip()[end:].splitlines():
        if not line.strip():
            continue
        if (not allow_gc_retention or not re.fullmatch(
                r"cargo-heli: cannot empty .+ yet: .+", line)):
            raise ValueError("unexpected data after CLI JSON frame: " + line[:120])
        diagnostics.append(line)
    return value, diagnostics


def validate_owned_marker(store, marker):
    for path, directory in ((store, True), (store / ".heli-cache-root.json", False)):
        info = path.lstat()
        reparse = getattr(info, "st_file_attributes", 0) & getattr(
            stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0x400)
        expected = stat.S_ISDIR(info.st_mode) if directory else stat.S_ISREG(info.st_mode)
        if not expected or path.is_symlink() or path.is_junction() or reparse:
            raise RuntimeError("SETUP: cache root/marker is not regular and nonreparse")
    if (marker.get("schema") != 1 or not plain_path(marker.get("root", "")).is_absolute()
            or not same_existing(marker["root"], store)):
        raise RuntimeError("SETUP: cache root marker belongs to a different filesystem root")


def closed_compiler_intervals(events):
    groups = {}
    counts = {"malformed_events": 0, "unpaired_invocations": 0,
              "ambiguous_invocations": 0, "invalid_pairs": 0,
              "failed_completed_invocations": 0}
    for event in events:
        if (not isinstance(event, dict) or event.get("phase") not in ("start", "end")
                or not isinstance(event.get("invocation"), str)
                or not isinstance(event.get("pid"), int)
                or not isinstance(event.get("epoch_ns"), int)):
            counts["malformed_events"] += 1
            continue
        groups.setdefault((event["invocation"], event["pid"]), []).append(event)
    intervals = []
    fields = ("agent", "heli_run", "cargo_home", "worktree", "crate")
    for (invocation, pid), group in groups.items():
        starts = [event for event in group if event["phase"] == "start"]
        ends = [event for event in group if event["phase"] == "end"]
        if not starts or not ends:
            counts["unpaired_invocations"] += 1
            continue
        if len(starts) != 1 or len(ends) != 1:
            counts["ambiguous_invocations"] += 1
            continue
        start, end = starts[0], ends[0]
        if (end["epoch_ns"] <= start["epoch_ns"]
                or any(not isinstance(start.get(field), str) or not start[field]
                       or start[field] != end.get(field) for field in fields)
                or not isinstance(end.get("exit"), int)):
            counts["invalid_pairs"] += 1
            continue
        interval = {field: start[field] for field in fields}
        interval |= {"invocation": invocation, "pid": pid,
                     "start_ns": start["epoch_ns"], "end_ns": end["epoch_ns"],
                     "exit": end["exit"], "worktree_label": start["agent"].split("-")[0]}
        intervals.append(interval)
        if interval["exit"] != 0:
            counts["failed_completed_invocations"] += 1
    return intervals, counts


def successful_overlap_pair(intervals):
    active = []
    for current in sorted(intervals, key=lambda interval: interval["start_ns"]):
        if current["exit"] != 0 or current["worktree_label"] not in ("a", "b"):
            continue
        active = [interval for interval in active
                  if interval["end_ns"] > current["start_ns"]]
        for other in active:
            if (other["worktree_label"] != current["worktree_label"]
                    and other["worktree"] != current["worktree"]):
                return [other, current]
        active.append(current)
    return None


def tree_size(root):
    """Logical bytes, tolerant of files disappearing during compilation/GC."""
    total = count = skipped = 0
    if not root.exists():
        return dict(bytes=0, files=0, skipped=0)
    try:
        info = root.lstat()
        if (root.is_symlink() or root.is_junction()
                or getattr(info, "st_file_attributes", 0) & getattr(
                    stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0x400)):
            return dict(bytes=0, files=0, skipped=1)
    except OSError:
        return dict(bytes=0, files=0, skipped=1)
    for directory, dirs, files in os.walk(root, followlinks=False):
        safe_dirs = []
        for name in dirs:
            path = Path(directory) / name
            try:
                info = path.lstat()
                reparse = getattr(info, "st_file_attributes", 0) & getattr(
                    stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0x400)
                if (name != ".git" and not path.is_symlink()
                        and not path.is_junction() and not reparse):
                    safe_dirs.append(name)
            except OSError:
                skipped += 1
        dirs[:] = safe_dirs
        for name in files:
            try:
                path = Path(directory) / name
                info = path.stat(follow_symlinks=False)
                if stat.S_ISREG(info.st_mode):
                    total += info.st_size
                    count += 1
            except OSError:
                skipped += 1
    return dict(bytes=total, files=count, skipped=skipped)


def readonly_inputs(home, worktrees):
    paths = [home / name for name in ("config", "config.toml",
                                      "credentials", "credentials.toml")]
    for wt in worktrees:
        paths += [wt / "Cargo.lock", wt / "AGENTS.md"]
    return {str(path): HELPER.sha(path) for path in paths if path.is_file()}


def configured_wrappers(home, worktrees, digests):
    """Read wrapper selection only; keep config contents/credentials out of logs."""
    files = []
    def config_at(directory):
        legacy = directory / "config"
        return legacy if legacy.is_file() else directory / "config.toml"

    files.append(config_at(home))
    for wt in worktrees:
        for parent in reversed((wt, *wt.parents)):
            files.append(config_at(parent / ".cargo"))
    found = []
    stack = set()
    def inspect(path):
        if path.is_file():
            digests[str(path)] = HELPER.sha(path)
            resolved = canonical_path(path)
            if resolved in stack:
                raise RuntimeError("Cargo config include cycle; cannot instrument safely")
            stack.add(resolved)
            data = tomllib.loads(path.read_text(encoding="utf-8"))
            includes = data.get("include", [])
            if isinstance(includes, str):
                includes = [includes]
            if not isinstance(includes, list):
                raise RuntimeError("unsupported Cargo config include format")
            for entry in includes:
                optional = False
                if isinstance(entry, dict):
                    optional = entry.get("optional", False)
                    entry = entry.get("path")
                if not isinstance(entry, str):
                    raise RuntimeError("unsupported Cargo config include entry")
                included = path.parent / entry
                if not included.is_file() and not optional:
                    raise RuntimeError("required Cargo config include unavailable")
                inspect(included)
            value = data.get("build", {}).get("rustc-wrapper")
            if value is not None:
                wrapper = value
                if value and not Path(value).is_absolute() and ("/" in value or "\\" in value):
                    wrapper = str(canonical_path(path.parent.parent / value))
                found.append({"config": str(path), "wrapper": wrapper})
            stack.remove(resolved)
    for path in dict.fromkeys(files):
        inspect(path)
    return found


class Run:
    def __init__(self, args):
        self.args = args
        self.root = canonical_path(args.output)
        if not re.fullmatch(r"[0-9a-fA-F]{40}", args.flui_ref):
            raise ValueError("--flui-ref must be an explicit full commit hash")
        if args.budget_mb <= 0:
            raise ValueError("--budget-mb must be positive")
        self.lab = self.root
        self.state = self.lab / "state"
        self.store = self.lab / "store"
        self.root_created = False
        self.worktrees = {}
        self.clients = []
        self.owned = []
        self.logs = []
        self.gc_jobs = []
        self.endpoint_directory = None
        self.result = {
            "status": "PREPARED", "flui_ref": self.args.flui_ref,
            "commands": [], "checks": {}, "scope": __doc__,
            "source_homes_outside_artifact_budget": True,
            "all_disk_or_hard_ram_quota_claim": False,
        }
        self.env = {k: v for k, v in os.environ.items()
                    if not k.startswith("HELI")
                    and k not in ("CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR",
                                  "CARGO_BUILD_BUILD_DIR", "CARGO_MAKEFLAGS",
                                  "MAKEFLAGS", "CARGO_INCREMENTAL")}
        self.original_home = canonical_path(self.env.get(
            "CARGO_HOME", str(Path.home() / ".cargo")))
        self.env["CARGO_HOME"] = str(self.original_home)
        self.env["CARGO_INCREMENTAL"] = "0"
        self.original_wrapper = args.original_wrapper or self.env.get(
            "RUSTC_WRAPPER") or self.env.get("CARGO_BUILD_RUSTC_WRAPPER")

    def save(self):
        HELPER.emit(self.root / "results.json", self.result)

    def command(self, label, argv, cwd=None, env=None, timeout=60, required=True):
        process, log = self.spawn(label, argv, cwd, env)
        try:
            code = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            self.result.setdefault("timeouts", []).append(label)
            raise
        row = next(x for x in reversed(self.result["commands"])
                   if x["label"] == label)
        row |= {"exit": code, "ended_ns": time.time_ns()}
        log.flush()
        text = (self.root / "logs" / (label + ".log")).read_text(
            encoding="utf-8", errors="replace")
        if required and code:
            raise RuntimeError(label + " failed; see owned log")
        return code, text

    def spawn(self, label, argv, cwd=None, env=None):
        log = (self.root / "logs" / (label + ".log")).open("wb")
        self.logs.append(log)
        process = subprocess.Popen(
            [str(value) for value in argv], cwd=cwd or self.root,
            env=env or self.env, stdout=log, stderr=subprocess.STDOUT,
            **HELPER.PROCESS_OPTIONS)
        self.owned.append(process)
        self.result["commands"].append({
            "label": label, "argv": [str(value) for value in argv],
            "cwd": str(cwd or self.root), "pid": process.pid,
            "submitted_ns": time.time_ns(),
        })
        self.save()
        return process, log

    def query(self, label, args, timeout=15):
        _, text = self.command(label, [self.heli, *args], timeout=timeout)
        value, diagnostics = parse_cli_json(
            text, allow_gc_retention=args[0] == "gc" and "--dry-run" not in args)
        if diagnostics:
            self.validate_gc_diagnostics(diagnostics)
            self.result.setdefault("cli_diagnostics", {})[label] = diagnostics
        HELPER.emit(self.root / (label + ".json"), value)
        return value

    def validate_gc_diagnostics(self, diagnostics):
        expected = [self.store / ".trash"]
        expected += [wt / "target/.heli-trash" for wt in self.worktrees.values()]
        for line in diagnostics:
            match = re.fullmatch(r"cargo-heli: cannot empty (.+) yet: (.+)", line)
            if match is None or not any(
                    canonical_path(match[1]) == canonical_path(path) for path in expected):
                raise RuntimeError("unexpected CLI diagnostic, not owned GC retention")

    def native_home_sizes(self):
        bases = [self.state / name for name in ("h", "dependency-homes")]
        selected = [canonical_path(value["home"])
                    for value in self.result.get("native_homes", {}).values()]
        roots = list(bases)
        roots += [home for home in selected
                  if not any(home.is_relative_to(canonical_path(base)) for base in bases)]
        unique = {str(canonical_path(root)): root for root in roots}
        sizes = {name: tree_size(path) for name, path in unique.items()}
        return {"bytes": sum(value["bytes"] for value in sizes.values()),
                "roots": sizes, "selected_homes": [str(path) for path in selected]}

    def client_env(self, name, phase="probe"):
        env = self.env | {
            "CARGO": self.tools[name]["cargo"],
            "RUSTC": self.tools[name]["rustc"],
            "CARGO_TARGET_DIR": str(self.worktrees[name] / "target"),
            "CARGO_BUILD_BUILD_DIR": str(self.store / "{workspace-path-hash}"),
            "HELI_SPIKE_AGENT": name + "-" + phase,
            "HELI_FLUI_WORKTREE": str(self.worktrees[name]),
        }
        env["PATH"] = str(Path(self.tools[name]["cargo"]).parent) + os.pathsep + env["PATH"]
        if self.args.instrument:
            selected = self.wrapper_by_worktree[name]
            if selected:
                env["HELI_FLUI_ORIGINAL_WRAPPER"] = selected
            else:
                env.pop("HELI_FLUI_ORIGINAL_WRAPPER", None)
        return env

    def prepare(self):
        self.root.mkdir(parents=True, exist_ok=False)
        self.root_created = True
        for name in ("logs", "events", "empty-hooks"):
            (self.root / name).mkdir()
        (self.lab / "w").mkdir()
        self.heli = Path(self.args.heli).resolve(strict=True)
        self.result["heli_binary"] = str(self.heli)
        self.result["heli_sha256"] = HELPER.sha(self.heli)
        git = ["git", "-c", "core.hooksPath=" + str(self.root / "empty-hooks")]
        git_env = self.env | {"GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}
        self.command("verify-flui-ref", git + ["cat-file", "-e", self.args.flui_ref + "^{commit}"],
                     Path(self.args.flui), git_env)
        for name in ("a", "b"):
            wt = self.lab / "w" / name
            self.command("worktree-" + name, git + [
                "worktree", "add", "--detach", str(wt), self.args.flui_ref],
                Path(self.args.flui), git_env, timeout=120)
            self.worktrees[name] = wt
            _, status = self.command("clean-" + name, git + ["status", "--porcelain"],
                                     wt, git_env)
            if status.strip():
                raise RuntimeError("new worktree is not clean")
        self.before = readonly_inputs(self.original_home, self.worktrees.values())
        self.result["input_digests_before"] = self.before
        self.tools = {}
        for name, wt in self.worktrees.items():
            self.tools[name] = {}
            for tool in ("cargo", "rustc"):
                _, path = self.command("resolve-" + name + "-" + tool,
                                       ["rustup", "which", tool], wt)
                self.tools[name][tool] = path.strip()
            for tool in ("cargo", "rustc"):
                _, version = self.command("version-" + name + "-" + tool,
                                          [self.tools[name][tool], "-Vv"], wt)
                self.tools[name][tool + "_version"] = version
            match = re.match(r"cargo (\d+)\.(\d+)", self.tools[name]["cargo_version"])
            if match is None or tuple(map(int, match.groups())) != (1, 99):
                raise RuntimeError("real acceptance is pinned to Cargo 1.99")
        self.result["toolchains"] = self.tools
        config_digests = {}
        wrappers = {
            name: configured_wrappers(self.original_home, [wt], config_digests)
            for name, wt in self.worktrees.items()
        }
        self.wrapper_by_worktree = {
            name: self.original_wrapper or (values[-1]["wrapper"] if values else None)
            for name, values in wrappers.items()
        }
        self.before.update(config_digests)
        self.result["input_digests_before"] = self.before
        self.result["wrapper_selection"] = {
            "original_outer": self.original_wrapper,
            "configured_wrapper_metadata": wrappers,
            "per_worktree_outer": self.wrapper_by_worktree,
            "instrumentation": self.args.instrument,
        }
        if self.args.instrument:
            self.instrument()

        config = self.root / "heli.toml"
        config.write_text(
            '[cargo]\nhome="isolated"\n[pool]\nslots=16\n[store]\n'
            'paths=["' + self.store.as_posix() + '"]\n'
            f'budget_mb={self.args.budget_mb}\nfloor_mb=0\nunit_hours=0\norphan_hours=0\n'
            'idle_hours=0\ndir_hours=0\n', encoding="utf-8")
        token = "heli-flui-native-" + uuid.uuid4().hex
        endpoint = str(self.root / "heli.sock")
        if os.name == "nt":
            endpoint = "\\\\.\\pipe\\" + token
        elif len(os.fsencode(endpoint)) > 100:
            self.endpoint_directory = Path(tempfile.mkdtemp(
                prefix="heli-flui-", dir="/tmp"))
            endpoint = str(self.endpoint_directory / "h.sock")
        self.env |= {
            "HELI_STATE_DIR": str(self.state),
            "HELI_CONFIG": str(config), "HELI_ENDPOINT": endpoint,
            "HELI_POOL": token if os.name == "nt" else str(self.root / "pool"),
            "HELI_ACTIVITY_DIR": str(self.root / "activity"),
        }
        self.spawn("own-daemon", [self.heli, "daemon"])
        self.command("own-ensure", [self.heli, "ensure"])
        marker = self.store / ".heli-cache-root.json"
        metadata = json.loads(marker.read_text())
        validate_owned_marker(self.store, metadata)
        self.result["managed_cache_root_marker"] = metadata
        for name, wt in self.worktrees.items():
            env = self.client_env(name)
            argv = [self.heli, "--cache", "off", "--output", "full", "exec", "--"]
            _, home = self.command("home-" + name, argv + [
                sys.executable, "-c",
                "import json,os;print(json.dumps({'home':os.environ.get('CARGO_HOME'),"
                "'run':os.environ.get('HELI_RUN')}))"], wt, env)
            # Heli full-mode footer shares stderr in this log: first line is child JSON.
            value = json.loads(next(line for line in home.splitlines()
                                    if line.startswith('{"home":')))
            selected = canonical_path(value["home"])
            if (same_existing(selected, self.original_home)
                    or not selected.is_relative_to(canonical_path(self.state))):
                raise RuntimeError("SETUP: command did not receive an owned isolated home")
            self.result.setdefault("native_homes", {})[name] = value
            _, raw = self.command("metadata-" + name, argv + [
                self.tools[name]["cargo"], "metadata", "--no-deps",
                "--format-version", "1", "--locked"], wt, env, timeout=120)
            document = json.loads(next(line for line in raw.splitlines()
                                       if line.startswith('{"packages":')))
            build = canonical_path(document["build_directory"])
            parts = build.relative_to(canonical_path(self.store)).parts
            if (len(parts) != 2 or len(parts[0]) != 2
                    or any(not re.fullmatch("[0-9a-fA-F]+", part) for part in parts)):
                raise RuntimeError("SETUP: Cargo template produced unmanaged layout")
            self.result.setdefault("validated_build_directories", {})[name] = str(build)
        if same_existing(self.result["native_homes"]["a"]["home"],
                         self.result["native_homes"]["b"]["home"]):
            raise RuntimeError("SETUP: both Git worktrees share a mutable Cargo home")
        self.query("status-before", ["status", "--json"])
        self.query("gc-before", ["gc", "--dry-run", "--json"])
        HELPER.emit(self.root / "setup-doctor.json", self.result)

    def instrument(self):
        source = (REPO / "experiments/spike-rustc-wrapper.rs").read_text(encoding="utf-8")
        needle = '    let path = PathBuf::from(root).join(format!("{invocation}-{pid}-{phase}.json"));'
        assert needle in source
        addition = r'''    let record = format!("{},\"cargo_home\":\"{}\",\"worktree\":\"{}\"}}\n",
        record.trim_end().strip_suffix('}').unwrap(),
        escaped(&env::var("CARGO_HOME").unwrap_or_default()),
        escaped(&env::var("HELI_FLUI_WORKTREE").unwrap_or_default()));
'''
        source = source.replace(needle, addition + needle)
        old = """    let status = Command::new(compiler)
        .args(&args)"""
        new = """    let mut command = if let Some(wrapper) = env::var_os("HELI_FLUI_ORIGINAL_WRAPPER") {
        let mut command = Command::new(wrapper);
        command.arg(compiler);
        command
    } else {
        Command::new(compiler)
    };
    let status = command.args(&args)"""
        assert old in source
        source = source.replace(old, new)
        rust = self.root / "observer.rs"
        rust.write_text(source, encoding="utf-8")
        binary = self.root / ("observer.exe" if os.name == "nt" else "observer")
        compile_env = self.env.copy()
        compile_env.pop("RUSTC_WRAPPER", None)
        self.command("build-observer", [
            self.tools["a"]["rustc"], "--edition=2024", rust, "-o", binary],
            env=compile_env, timeout=60)
        self.env["RUSTC_WRAPPER"] = str(binary)
        self.env["HELI_SPIKE_EVENTS"] = str(self.root / "events")
        if self.original_wrapper:
            self.env["HELI_FLUI_ORIGINAL_WRAPPER"] = self.original_wrapper

    def telemetry(self, index):
        status = self.query("status-" + str(index), ["status", "--json"], timeout=15)
        row = {
            "epoch_ns": time.time_ns(), "status": status,
            "free_bytes": shutil.disk_usage(self.root).free,
        }
        if index % 15 == 0:
            row["sizes"] = {
                "store": tree_size(self.store),
                "native_homes": self.native_home_sizes(),
                "targets": {name: tree_size(wt / "target")
                            for name, wt in self.worktrees.items()},
            }
            total = row["sizes"]["store"]["bytes"] + row["sizes"]["native_homes"]["bytes"]
            total += sum(value["bytes"] for value in row["sizes"]["targets"].values())
            row["observed_cache_bytes"] = total
            if total >= 40 * GIB and not self.result.get("cache_review_40gib"):
                self.result["cache_review_40gib"] = row
                print("40GiB CACHE REVIEW: no additional workload will be started", flush=True)
            if total >= 60 * GIB:
                self.result["cache_limit_60gib"] = row
                self.result["no_extra_heavy_launches"] = True
                print("60GiB CAPACITY REVIEW: finish existing wave; no extra heavy runs",
                      flush=True)
        with (self.root / "samples.jsonl").open("a", encoding="utf-8") as stream:
            stream.write(json.dumps(row) + "\n")
        return status

    def workload(self):
        phases = {
            "fmt": ["fmt", "--all"],
            "clippy": ["clippy", "--workspace", "--all-targets", "--locked"],
            "check": ["check", "--workspace", "--all-targets", "--locked"],
            "test": ["test", "--workspace", "--locked", "--no-fail-fast"],
        }
        for phase, args in phases.items():
            for name, wt in self.worktrees.items():
                label = name + "-" + phase
                # Normal builtins preserve Heli's Mutate and test-phase contracts.
                argv = [self.heli, "--cache", "off", "--output", "full", *args]
                process, _ = self.spawn(label, argv, wt, self.client_env(name, phase))
                self.clients.append((label, process))
        deadline = time.monotonic() + self.args.timeout
        index = 0
        while any(process.poll() is None for _, process in self.clients):
            if time.monotonic() >= deadline:
                raise TimeoutError("full two-worktree FLUI workload deadline")
            self.telemetry(index)
            if index % 15 == 0 and not any(p.poll() is None for _, p in self.gc_jobs):
                label = "gc-during-" + str(index)
                process, _ = self.spawn(label, [self.heli, "gc", "--json"])
                self.gc_jobs.append((label, process))
            index += 1
            time.sleep(1)
        self.result["client_exits"] = {label: p.returncode for label, p in self.clients}
        for label, process in self.gc_jobs:
            process.wait(timeout=120)
            if process.returncode:
                raise RuntimeError(label + " failed")
            data, diagnostics = parse_cli_json(
                (self.root / "logs" / (label + ".log")).read_text(),
                allow_gc_retention=True)
            if diagnostics:
                self.validate_gc_diagnostics(diagnostics)
                self.result.setdefault("cli_diagnostics", {})[label] = diagnostics
            HELPER.emit(self.root / (label + ".json"), data)
            for directory in data.get("dirs", []):
                if directory.get("store") and same_existing(directory["store"], self.store):
                    if directory.get("managed_cache_root") is not True:
                        raise RuntimeError("observed store directory is unmanaged; setup/layout issue")
                    self.result.setdefault("observed_managed_directories", []).append(
                        str(canonical_path(directory["path"])))
                elif directory.get("store"):
                    raise RuntimeError("GC reported an unexpected root outside this owned lab")
        self.query("status-after", ["status", "--json"])
        self.query("gc-after", ["gc", "--json"], timeout=120)
        final = self.query("gc-final", ["gc", "--dry-run", "--json"], timeout=120)
        # Best-effort deletion can leave open files or race another deleter.
        # Observe later passes, rather than invent an immediate-empty contract.
        deadline = time.monotonic() + self.args.trash_settle_seconds
        debt = lambda doc: sum(volume.get("pending", 0) for volume in doc.get("volumes", []))
        history = [{"epoch_ns": time.time_ns(), "pending_bytes": debt(final)}]
        retries = 0
        while debt(final) and retries < 3 and time.monotonic() < deadline:
            time.sleep(min(15, max(0, deadline - time.monotonic())))
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                break
            retries += 1
            self.query("gc-settle-" + str(retries), ["gc", "--json"],
                       timeout=max(1, min(30, remaining)))
            final = self.query("gc-settle-survey-" + str(retries),
                               ["gc", "--dry-run", "--json"], timeout=15)
            history.append({"epoch_ns": time.time_ns(), "pending_bytes": debt(final)})
        self.result["trash_reclamation"] = {
            "status": "settled" if not debt(final) else "pending_after_bounded_retry",
            "history": history, "retry_passes": retries,
            "remaining_bytes": debt(final),
            "note": "Retention debt is reported separately from tracked artifact budget.",
        }
        self.result["final_gc"] = final
        self.result["cache_sizes_after_gc"] = {
            "store": tree_size(self.store),
            "native_homes_outside_budget": self.native_home_sizes(),
            "targets": {name: tree_size(wt / "target") for name, wt in self.worktrees.items()},
        }
        events = []
        for path in (self.root / "events").glob("*.json"):
            try:
                event = json.loads(path.read_text())
                if not isinstance(event, dict) or not isinstance(event.get("epoch_ns"), int):
                    raise ValueError("malformed compiler event")
                events.append(event)
            except (OSError, ValueError):
                self.result.setdefault("incomplete_event_files", []).append(str(path))
        events.sort(key=lambda event: event["epoch_ns"])
        HELPER.emit(self.root / "compiler-events.json", events)
        intervals, incomplete = closed_compiler_intervals(events)
        HELPER.emit(self.root / "compiler-closed-intervals.json", intervals)
        self.result["compiler_interval_counts"] = incomplete | {
            "closed_intervals": len(intervals),
            "unreadable_or_invalid_event_files": len(
                self.result.get("incomplete_event_files", [])),
        }
        homes = set()
        eligible = []
        for interval in intervals:
            homes.add(str(canonical_path(interval["cargo_home"])))
            owner = interval["worktree_label"]
            if (owner in self.worktrees and canonical_path(interval["worktree"])
                    == canonical_path(self.worktrees[owner])):
                eligible.append(interval)
        pair = successful_overlap_pair(eligible)
        overlap = pair is not None
        self.result["actual_compiler_overlap_pair"] = pair
        self.result["actual_compiler_overlap"] = overlap if self.args.instrument else None
        self.result["actual_compiler_events"] = len(events)
        self.result["input_digests_after"] = {
            path: HELPER.sha(path) for path in self.before if Path(path).is_file()
        }
        final_sources = {}
        for name, wt in self.worktrees.items():
            _, head = self.command("final-head-" + name,
                                   ["git", "rev-parse", "HEAD"], wt)
            _, dirty = self.command("final-clean-" + name,
                                    ["git", "status", "--porcelain"], wt)
            final_sources[name] = {
                "head": head.strip(), "clean": not dirty.strip(),
            }
        self.result["source_state_after"] = final_sources
        self.result["checks"] = {
            "all_eight_full_commands_succeeded": len(self.clients) == 8
                and all(p.returncode == 0 for _, p in self.clients),
            "actual_cross_worktree_compiler_overlap": overlap,
            "compiler_events_have_heli_run": bool(events)
                and all(event.get("heli_run") for event in events),
            "compiler_native_homes_distinct": len(homes - {None, ""}) == 2,
            "original_config_credentials_and_project_locks_unchanged":
                self.before == self.result["input_digests_after"],
            "both_worktrees_still_clean_at_pinned_commit": len(final_sources) == 2
                and all(value["clean"] and value["head"].lower()
                        == self.args.flui_ref.lower() for value in final_sources.values()),
            "idle_artifact_gc_budget_met": final["used"] <= final["budget"],
        }
        self.result["status"] = (
            "PASS" if all(self.result["checks"].values()) else "FAIL")
        if self.result["status"] == "PASS" and debt(final):
            self.result["status"] = "PASS_WITH_RETENTION_DEBT"

    def cancel_owned_runs(self):
        # The unique endpoint contains only this lab's requests. Filter cwd too.
        try:
            status = self.query("cleanup-status", ["status", "--json"], timeout=10)
            owned_paths = {str(wt).casefold() for wt in self.worktrees.values()}
            for run in status.get("runs", []):
                if (str(run.get("worktree", "")).casefold() in owned_paths
                        and run.get("phase") != "done"):
                    self.command("cancel-" + run["id"],
                                 [self.heli, "cancel", run["id"]],
                                 timeout=15, required=False)
        except Exception as error:
            self.result["cancel_cleanup_error"] = repr(error)

    def cleanup(self):
        if any(process.poll() is None for _, process in self.clients):
            self.cancel_owned_runs()
        for process in reversed(self.owned):
            HELPER.kill_owned(process)
        for log in self.logs:
            log.close()
        for name, wt in self.worktrees.items():
            check = subprocess.run(
                ["git", "status", "--porcelain"], cwd=wt, env=self.env,
                capture_output=True, text=True, timeout=30, **HELPER.PROCESS_OPTIONS)
            if check.returncode or check.stdout.strip():
                self.result.setdefault("retained_dirty_worktrees", {})[name] = check.stdout
                continue
            head = subprocess.run(
                ["git", "rev-parse", "HEAD"], cwd=wt, env=self.env,
                capture_output=True, text=True, timeout=30, **HELPER.PROCESS_OPTIONS)
            if head.returncode or head.stdout.strip().lower() != self.args.flui_ref.lower():
                self.result.setdefault("retained_worktrees", {})[name] = {
                    "reason": "head_unavailable" if head.returncode else "head_differs_from_pinned_commit",
                    "expected_head": self.args.flui_ref,
                    "actual_head": head.stdout.strip() if not head.returncode else None,
                }
                continue
            removed = subprocess.run(
                ["git", "-c", "core.hooksPath=" + str(self.root / "empty-hooks"),
                 "worktree", "remove", str(wt)], cwd=self.args.flui,
                env=self.env, capture_output=True, text=True, timeout=120,
                **HELPER.PROCESS_OPTIONS)
            if removed.returncode:
                self.result.setdefault("worktree_cleanup_errors", {})[name] = removed.stderr
        if self.endpoint_directory is not None:
            try:
                sock = self.endpoint_directory / "h.sock"
                if sock.exists() and stat.S_ISSOCK(sock.lstat().st_mode):
                    sock.unlink()
                self.endpoint_directory.rmdir()
            except OSError as error:
                self.result["endpoint_cleanup_error"] = repr(error)

    def run(self):
        if not self.args.run:
            print(json.dumps({
                "status": "PREPARED_ONLY", "flui_ref": self.args.flui_ref,
                "output_must_be_new": str(self.root),
                "commands": ["fmt --all", "clippy --workspace --all-targets --locked",
                             "check --workspace --all-targets --locked",
                             "test --workspace --locked --no-fail-fast"],
                "concurrent_clients": 8, "worktrees": 2, "pool": 16,
                "build_template": str(self.store / "{workspace-path-hash}"),
                "artifact_budget_bytes": self.args.budget_mb * 1024 ** 2,
                "source_homes_excluded_from_budget": True,
                "instrumentation_requires": "--instrument",
                "no_Cargo_Git_daemon_or_server_started": True,
            }, indent=2))
            return 0
        try:
            self.prepare()
            self.workload()
        except Exception as error:
            self.result["status"] = "FAIL"
            self.result["error"] = repr(error)
        finally:
            if self.root_created:
                self.cleanup()
                self.save()
        print(json.dumps({"status": self.result["status"],
                          "checks": self.result["checks"],
                          "error": self.result.get("error"),
                          "evidence": str(self.root / "results.json")}, indent=2))
        return 0 if self.result["status"] in ("PASS", "PASS_WITH_RETENTION_DEBT") else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--heli", required=True)
    parser.add_argument("--flui", required=True)
    parser.add_argument("--flui-ref", required=True)
    parser.add_argument("--budget-mb", type=int, required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--timeout", type=int, default=4 * 60 * 60)
    parser.add_argument("--trash-settle-seconds", type=int, default=60)
    parser.add_argument("--instrument", action="store_true", help="record compiler intervals; required with --run")
    parser.add_argument("--original-wrapper")
    parser.add_argument("--run", action="store_true")
    args = parser.parse_args()
    if args.run and not args.instrument:
        parser.error("--run requires --instrument for compiler-overlap acceptance")
    return Run(args).run()


if __name__ == "__main__":
    raise SystemExit(main())

