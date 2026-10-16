#!/usr/bin/env python3
"""Real HTTP acceptance for automatic per-worktree Cargo homes.

Default mode prepares fixtures only. --run permits tiny generated Rust builds.
A real Cargo fetch in the original home and a cold isolated worktree download
remain HTTP-gated while another cold worktree compiles, including bare nested
Cargo with a registry dependency absent from the parent's graph. No vendor,
dependency epoch, source seeding, external network or user Cargo home is used.
"""
import argparse
import hashlib
import http.server
import importlib.util
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import tempfile
import threading
import time
import uuid
import urllib.error
import urllib.request

sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location(
    "real_download_hol", Path(__file__).with_name("real-download-hol.py"))
HTTP = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(HTTP)


def write_package(path, name, dependency, nested=False):
    (path / "src").mkdir(parents=True)
    (path / "Cargo.toml").write_text(
        f'[package]\nname="{name}"\nversion="0.0.0"\nedition="2024"\n'
        f'[dependencies]\n{dependency}={{version="=0.0.1",registry="lab"}}\n',
        encoding="utf-8")
    (path / "src/lib.rs").write_text(
        "pub fn value() -> u32 { " + dependency.replace("-", "_")
        + "::value() }\n", encoding="utf-8")
    build = r'''use std::{env, fs, path::PathBuf};
fn main() {
    let home = env::var("CARGO_HOME").unwrap();
    let config_origin = env::var("HOL_CONFIG_ORIGIN").unwrap();
    let credentials = fs::read_to_string(PathBuf::from(&home)
        .join("credentials.toml")).unwrap_or_default();
    fs::write(env::var_os("HOL_REPORT").unwrap(),
        format!("home={}\norigin={}\ncredentials={}\n", home,
            config_origin, credentials.contains("fake-lab-token"))).unwrap();
    println!("cargo:rerun-if-env-changed=HOL_REPORT");
'''
    if nested:
        build += r'''    if env::var_os("HOL_RUN_NESTED").is_some() {
        let child = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("child");
        fs::create_dir_all(child.join("src")).unwrap();
        fs::write(child.join("Cargo.toml"), r#"[package]
name="isolated-generated-child"
version="0.0.0"
edition="2024"
[workspace]
[dependencies]
hol-nested={version="=0.0.1",registry="lab"}
"#).unwrap();
        fs::write(child.join("src/lib.rs"),
            "pub fn value() -> u32 { hol_nested::value() }\n").unwrap();
        fs::write(child.join("build.rs"), r#"fn main() {
    std::fs::write(std::env::var_os("HOL_NESTED_REPORT").unwrap(),
        std::env::var("CARGO_HOME").unwrap()).unwrap();
}
"#).unwrap();
        let status = std::process::Command::new(env::var_os("CARGO").unwrap())
            .args(["check", "--manifest-path"]).arg(child.join("Cargo.toml"))
            .arg("--target-dir").arg(child.join("target"))
            .env_remove("CARGO_BUILD_BUILD_DIR").status().unwrap();
        assert!(status.success(), "bare nested Cargo failed");
    }
'''
    (path / "build.rs").write_text(build + "}\n", encoding="utf-8")


class Experiment:
    def __init__(self, args):
        self.args = args
        self.root = Path(args.output).resolve()
        self.root.mkdir(parents=True, exist_ok=False)
        self.owned = []
        self.streams = []
        self.worktrees = []
        self.endpoint_directory = None
        self.server = None
        self.events = []
        self.event_lock = threading.Lock()
        self.gates = {n: (threading.Event(), threading.Event())
                      for n in ("hol-a", "hol-shared-slow")}
        self.names = ("hol-a", "hol-b", "hol-nested", "hol-shared-slow")
        self.payloads = {name: HTTP.archive(name) for name in self.names}
        self.result = {
            "status": "PREPARED", "root": str(self.root), "commands": [],
            "authenticated_registry": args.authenticated,
            "scope": "Automatic full homes, real Git worktrees and HTTP downloads",
            "limitations": [
                "Tiny generated alternate registry, no real project benchmark",
                "No quota, cross-platform or universal concurrency claim",
            ],
        }
        self.env = {
            k: v for k, v in os.environ.items()
            if not k.startswith(("HELI", "CARGO_"))
            and k not in ("RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "MAKEFLAGS",
                          "RUSTFLAGS", "RUSTDOCFLAGS")
        }
        self.env |= {
            "NO_PROXY": "127.0.0.1,localhost",
            "no_proxy": "127.0.0.1,localhost",
            "CARGO_HTTP_PROXY": "http://127.0.0.1:9",
            "CARGO_HTTP_TIMEOUT": "30", "CARGO_BUILD_JOBS": "2",
            "CARGO_INCREMENTAL": "0", "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_CONFIG_NOSYSTEM": "1",
        }

    def save(self):
        HTTP.emit(self.root / "results.json", self.result)

    def record_http(self, **values):
        with self.event_lock:
            self.events.append(dict(epoch_ns=time.time_ns(), **values))
            HTTP.emit(self.root / "http-events.json", self.events)

    def start_server(self):
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_GET(self):
                path = self.path.split("?", 1)[0]
                owner.record_http(event="request", path=path)
                # Record only acceptance booleans, never header values/tokens.
                authenticated = (
                    self.headers.get("Authorization") == "fake-lab-token")
                if (owner.args.authenticated and path != "/index/config.json"
                        and not authenticated):
                    owner.record_http(event="auth-rejected", path=path)
                    self.send_error(401, "Registry authentication required")
                    return
                if path == "/index/config.json":
                    data = {"dl": owner.base + "/api/v1/crates",
                            "auth-required": owner.args.authenticated}
                    body = json.dumps(data).encode()
                elif path.startswith("/index/"):
                    name = path.rsplit("/", 1)[-1]
                    if (name not in owner.payloads
                            or path != "/index/" + HTTP.index_key(name)):
                        self.send_error(404)
                        return
                    data = {
                        "name": name, "vers": "0.0.1", "deps": [],
                        "cksum": hashlib.sha256(owner.payloads[name]).hexdigest(),
                        "features": {}, "yanked": False,
                    }
                    if owner.args.authenticated:
                        owner.record_http(event="index-authenticated", package=name)
                    body = (json.dumps(data) + "\n").encode()
                else:
                    match = re.fullmatch(
                        r"/api/v1/crates/([^/]+)/0[.]0[.]1/download", path)
                    if not match or match[1] not in owner.payloads:
                        self.send_error(404)
                        return
                    name = match[1]
                    if owner.args.authenticated:
                        owner.record_http(event="archive-authenticated", package=name)
                    if name in owner.gates:
                        entered, released = owner.gates[name]
                        entered.set()
                        owner.record_http(event="download-gated", package=name)
                        if not released.wait(25):
                            owner.record_http(event="gate-timeout", package=name)
                        owner.record_http(event="download-released", package=name)
                    else:
                        owner.record_http(event="download-served", package=name)
                    body = owner.payloads[name]
                self.send_response(200)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                try:
                    self.wfile.write(body)
                except (BrokenPipeError, ConnectionResetError):
                    owner.record_http(event="client-disconnected", path=path)

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.base = f"http://127.0.0.1:{self.server.server_port}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def check_unauthenticated_rejection(self):
        # Local negative controls establish that successful Cargo downloads
        # actually require its credential provider, not just an unused token.
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        paths = ("/index/" + HTTP.index_key("hol-b"),
                 "/api/v1/crates/hol-nested/0.0.1/download")
        codes = []
        for path in paths:
            try:
                with opener.open(self.base + path, timeout=5) as response:
                    codes.append(response.status)
            except urllib.error.HTTPError as error:
                codes.append(error.code)
        self.result["unauthenticated_control_statuses"] = codes
        if codes != [401, 401]:
            raise RuntimeError("local authenticated registry accepted missing token")

    def spawn(self, label, argv, cwd, extra=None):
        stream = (self.root / (label + ".log")).open("wb")
        self.streams.append(stream)
        process = subprocess.Popen(
            [str(x) for x in argv], cwd=cwd, env=self.env | (extra or {}),
            stdout=stream, stderr=subprocess.STDOUT, **HTTP.PROCESS_OPTIONS)
        self.owned.append(process)
        self.result["commands"].append({
            "label": label, "argv": [str(x) for x in argv], "cwd": str(cwd),
            "pid": process.pid, "started_ns": time.time_ns(),
        })
        self.save()
        return process

    def wait(self, label, process, timeout=30, success=True):
        row = next(r for r in reversed(self.result["commands"])
                   if r["label"] == label)
        try:
            code = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            row["timeout_seconds"] = timeout
            self.save()
            raise
        row |= {"exit": code, "finished_ns": time.time_ns()}
        self.save()
        if success and code:
            raise RuntimeError(label + " failed; see its log")
        return code

    def command(self, label, argv, cwd=None, extra=None, success=True):
        process = self.spawn(label, argv, cwd or self.root, extra)
        return self.wait(label, process, success=success)

    def cargo_args(self, manifest, offline=False):
        flags = [self.cargo, "check", "--manifest-path", manifest,
                 "--locked", "--lib"]
        return flags + (["--offline"] if offline else [])

    def heli_args(self, manifest, offline=False, mode=None):
        flags = [self.heli, "--cache", "off", "--output", "full"]
        if mode is not None:
            flags += ["--cargo-home", mode]
        return flags + ["exec", "--verify", "--"] + self.cargo_args(manifest, offline)

    def report(self, name):
        text = (self.root / name).read_text(encoding="utf-8")
        return dict(line.split("=", 1) for line in text.splitlines())

    def build_env(self, name, nested=False, offline=False):
        values = {
            "CARGO_TARGET_DIR": str(self.root / ("target-" + name)),
            "HOL_REPORT": str(self.root / ("report-" + name)),
        }
        if nested:
            values |= {
                "HOL_RUN_NESTED": "1",
                "HOL_NESTED_REPORT": str(self.root / ("nested-" + name)),
            }
        if offline:
            values["CARGO_NET_OFFLINE"] = "true"
        return values

    def prepare(self):
        fixture = self.root / "fixture-repo"
        fixture.mkdir()
        (fixture / "Cargo.toml").write_text(
            '[workspace]\nmembers=["a","b"]\nresolver="3"\n', encoding="utf-8")
        write_package(fixture / "a", "fixture-a", "hol-a")
        write_package(fixture / "b", "fixture-b", "hol-b", nested=True)
        control = self.root / "shared-b-control"
        write_package(control, "shared-b-control", "hol-b")
        with (control / "Cargo.toml").open("a") as stream:
            stream.write("[workspace]\n")
        downloader = self.root / "shared-downloader"
        write_package(downloader, "shared-downloader", "hol-shared-slow")
        with (downloader / "Cargo.toml").open("a") as stream:
            stream.write("[workspace]\n")
        return fixture, control, downloader

    def run(self):
        fixture, control, downloader = self.prepare()
        if not self.args.run:
            self.save()
            print("PREPARED: no Cargo/Git/server execution without --run")
            return 0
        if not self.args.heli:
            raise ValueError("--heli required with --run")
        self.heli = Path(self.args.heli).resolve(strict=True)
        self.result["heli_sha256"] = HTTP.sha(self.heli)
        if self.args.cargo:
            self.cargo = str(Path(self.args.cargo).resolve(strict=True))
            rustc = Path(self.cargo).with_name(
                "rustc.exe" if os.name == "nt" else "rustc")
        else:
            self.cargo = subprocess.check_output(
                ["rustup", "which", "cargo"], text=True, timeout=30,
                **HTTP.PROCESS_OPTIONS).strip()
            rustc = subprocess.check_output(
                ["rustup", "which", "rustc"], text=True, timeout=30,
                **HTTP.PROCESS_OPTIONS).strip()
        self.env |= {
            "CARGO": self.cargo, "RUSTC": str(rustc),
            "PATH": str(Path(self.cargo).parent) + os.pathsep + self.env["PATH"],
        }
        original = self.root / "original-home"
        original.mkdir()
        self.env["CARGO_HOME"] = str(original)
        try:
            self.start_server()
            if self.args.authenticated:
                self.check_unauthenticated_rejection()
            config = original / "config.toml"
            provider = ('credential-provider="cargo:token"\n'
                        if self.args.authenticated else "")
            config.write_text(
                '[registries.lab]\nindex="sparse+' + self.base + '/index/"\n'
                + provider +
                '[env]\nHOL_CONFIG_ORIGIN={value="config-input",relative=true}\n',
                encoding="utf-8")
            credentials = original / "credentials.toml"
            credentials.write_text(
                '[registries.lab]\ntoken="fake-lab-token"\n', encoding="utf-8")
            credentials.chmod(0o600)
            original_digests = {str(p): HTTP.sha(p) for p in (config, credentials)}
            self.command("cargo-version", [self.cargo, "-Vv"])
            self.command("rustc-version", [str(rustc), "-Vv"])
            version = (self.root / "cargo-version.log").read_text()
            if tuple(map(int, re.match(r"cargo (\d+)\.(\d+)", version).groups())) < (1, 99):
                raise RuntimeError("Cargo 1.99 or newer required")
            for name, path in (("fixture", fixture), ("control", control),
                               ("downloader", downloader)):
                self.command(name + "-lock", [
                    self.cargo, "generate-lockfile", "--manifest-path",
                    path / "Cargo.toml"])
            self.command("warm-original-b", [
                self.cargo, "fetch", "--locked", "--manifest-path",
                control / "Cargo.toml"])
            hooks = self.root / "empty-hooks"
            hooks.mkdir()
            git = ["git", "-c", "core.hooksPath=" + str(hooks)]
            self.command("git-init", git + ["init", "-b", "fixture", str(fixture)])
            self.command("git-add", git + ["add", "."], fixture)
            self.command("git-commit", git + [
                "-c", "user.name=Heli fixture", "-c", "user.email=fixture@invalid",
                "commit", "--no-verify", "-m", "Owned fixture"], fixture)
            for name in ("a", "b"):
                path = self.root / "w" / name
                path.parent.mkdir(exist_ok=True)
                self.command("git-worktree-" + name,
                             git + ["worktree", "add", "--detach", str(path)],
                             fixture)
                self.worktrees.append((fixture, path))
            a_manifest = self.root / "w/a/a/Cargo.toml"
            b_manifest = self.root / "w/b/b/Cargo.toml"
            locks = [self.root / "w" / n / "Cargo.lock" for n in ("a", "b")]
            lock_digests = {str(p): HTTP.sha(p) for p in locks}
            parent_graph = locks[0].read_text()
            if 'name = "hol-nested"' in parent_graph:
                raise RuntimeError("nested dependency accidentally in parent graph")

            heli_config = self.root / "heli.toml"
            heli_config.write_text(
                '[cargo]\nhome="isolated"\n[pool]\nslots=4\n[store]\n'
                'paths=["' + (self.root / "build-store").as_posix()
                + '"]\nbudget_mb=512\nfloor_mb=0\n', encoding="utf-8")
            token = "heli-fullhome-" + uuid.uuid4().hex
            endpoint = str(self.root / "heli.sock")
            if os.name == "nt":
                endpoint = "\\\\.\\pipe\\" + token
            elif len(os.fsencode(endpoint)) > 100:
                self.endpoint_directory = Path(tempfile.mkdtemp(
                    prefix="heli-fullhome-", dir="/tmp"))
                endpoint = str(self.endpoint_directory / "h.sock")
            self.env |= {
                "HELI_STATE_DIR": str(self.root / "state"),
                "HELI_CONFIG": str(heli_config), "HELI_ENDPOINT": endpoint,
                "HELI_POOL": token if os.name == "nt" else str(self.root / "pool"),
                "HELI_ACTIVITY_DIR": str(self.root / "activity"),
            }
            daemon = self.spawn("own-daemon", [self.heli, "daemon"], self.root)
            self.command("own-ensure", [self.heli, "ensure"])
            if daemon.poll() is not None:
                raise RuntimeError("owned daemon failed")

            # Warm original B cannot rescue an empty isolated B in offline mode.
            code = self.command(
                "cold-isolated-offline", self.heli_args(b_manifest, True),
                self.root / "w/b", self.build_env("cold-offline", offline=True),
                success=False)
            self.result["cold_offline_exit"] = code
            if code == 0:
                raise RuntimeError("cold isolated offline unexpectedly used original cache")

            shared_a = self.spawn(
                "shared-real-fetch", [self.cargo, "fetch", "--locked",
                                      "--manifest-path", downloader / "Cargo.toml"],
                downloader)
            if not self.gates["hol-shared-slow"][0].wait(8):
                raise RuntimeError("shared real download did not reach HTTP gate")
            isolated_a = self.spawn(
                "isolated-a", self.heli_args(a_manifest),
                self.root / "w/a", self.build_env("a"))
            if not self.gates["hol-a"][0].wait(8):
                raise RuntimeError("isolated A blocked before its own cold HTTP request")
            control_b = self.spawn(
                "ordinary-shared-b", self.cargo_args(control / "Cargo.toml", True),
                control, self.build_env("shared-control"))
            started_ns = time.time_ns()
            b = self.spawn(
                "isolated-b", self.heli_args(b_manifest, mode="isolated"),
                self.root / "w/b", self.build_env("b", nested=True))
            self.wait("isolated-b", b, timeout=12)
            progress = {
                "shared_fetch_alive": shared_a.poll() is None,
                "isolated_a_alive": isolated_a.poll() is None,
                "both_release_events_absent": all(
                    not released.is_set() for _, released in self.gates.values()),
                "ordinary_b_alive": control_b.poll() is None,
                "ordinary_b_wait_log": "Blocking waiting for file lock on package cache"
                    in (self.root / "ordinary-shared-b.log").read_text(errors="replace"),
                "b_rmeta": [str(p.relative_to(self.root))
                            for p in (self.root / "target-b").rglob("*.rmeta")],
            }
            b_report = self.report("report-b")
            nested_home = (self.root / "nested-b").read_text()
            with self.event_lock:
                progress["http_responses_not_released"] = not any(
                    event["event"] in ("download-released", "gate-timeout")
                    for event in self.events)
            self.result["b_progress_while_both_downloads_gated"] = progress
            self.result["b_report"] = b_report
            self.result["nested_home_equals_b"] = nested_home == b_report["home"]
            self.result["b_completed_ns"] = time.time_ns()
            self.result["b_started_ns"] = started_ns

            self.command(
                "warm-isolated-offline", self.heli_args(b_manifest, True),
                self.root / "w/b",
                self.build_env("warm-offline", nested=True, offline=True))
            self.result["warm_offline_report"] = self.report("report-warm-offline")
            self.result["warm_offline_reused_home"] = (
                self.result["warm_offline_report"]["home"] == b_report["home"])
            self.result["release_ns"] = time.time_ns()
            for _, released in self.gates.values():
                released.set()
            for label, process in (("isolated-a", isolated_a),
                                   ("shared-real-fetch", shared_a),
                                   ("ordinary-shared-b", control_b)):
                self.wait(label, process)
            a_report = self.report("report-a")
            shared_report = self.report("report-shared-control")
            self.result["a_report"] = a_report
            self.result["shared_report"] = shared_report
            self.result["checks"] = {
                "two_distinct_native_homes": (
                    a_report["home"] != b_report["home"]
                    and Path(a_report["home"]).resolve() != original
                    and Path(b_report["home"]).resolve() != original),
                "b_completed_with_actual_compiler_outputs_under_gate": (
                    progress["shared_fetch_alive"] and progress["isolated_a_alive"]
                    and progress["both_release_events_absent"]
                    and progress["http_responses_not_released"]
                    and bool(progress["b_rmeta"])),
                "ordinary_shared_negative_control_blocked": (
                    progress["ordinary_b_alive"] and progress["ordinary_b_wait_log"]),
                "nested_new_dependency_uses_same_home": self.result["nested_home_equals_b"],
                "warm_offline_reuses_home": self.result["warm_offline_reused_home"],
                "configuration_origin_matches_shared": (
                    a_report["origin"] == b_report["origin"] == shared_report["origin"]),
                "fake_credentials_preserved": (
                    a_report["credentials"] == b_report["credentials"]
                    == shared_report["credentials"] == "true"),
                "original_config_credentials_unchanged": all(
                    HTTP.sha(p) == digest for p, digest in original_digests.items()),
                "both_project_locks_unchanged": all(
                    HTTP.sha(p) == digest for p, digest in lock_digests.items()),
                "no_http_gate_timed_out": not any(
                    event["event"] == "gate-timeout" for event in self.events),
            }
            if self.args.authenticated:
                archives = [event for event in self.events
                            if event["event"] == "archive-authenticated"]
                self.result["authenticated_archive_counts"] = {
                    name: sum(event["package"] == name for event in archives)
                    for name in self.names
                }
                self.result["authenticated_index_count"] = sum(
                    event["event"] == "index-authenticated" for event in self.events)
                during_b = {
                    event["package"] for event in archives
                    if started_ns <= event["epoch_ns"] <= self.result["b_completed_ns"]
                }
                self.result["checks"] |= {
                    "unauthenticated_index_and_archive_refused":
                        self.result["unauthenticated_control_statuses"] == [401, 401],
                    "isolated_a_authenticated_actual_archive":
                        self.result["authenticated_archive_counts"]["hol-a"] >= 1,
                    "isolated_b_and_nested_authenticated_actual_archives":
                        {"hol-b", "hol-nested"}.issubset(during_b),
                }
            self.result["status"] = (
                "PASS" if all(self.result["checks"].values()) else "FAIL")
        except Exception as error:
            self.result["status"] = "FAIL"
            self.result["error"] = repr(error)
        finally:
            self.cleanup()
            self.save()
        print(json.dumps({"status": self.result["status"],
                          "checks": self.result.get("checks"),
                          "error": self.result.get("error"),
                          "evidence": str(self.root / "results.json")}, indent=2))
        return 0 if self.result["status"] == "PASS" else 1

    def cleanup(self):
        for _, released in self.gates.values():
            released.set()
        for process in reversed(self.owned):
            HTTP.kill_owned(process)
        if self.server is not None:
            self.server.shutdown()
            self.server.server_close()
        for stream in self.streams:
            stream.close()
        if self.endpoint_directory is not None:
            endpoint = self.endpoint_directory / "h.sock"
            try:
                if endpoint.exists() and stat.S_ISSOCK(endpoint.lstat().st_mode):
                    endpoint.unlink()
                self.endpoint_directory.rmdir()
            except OSError as error:
                self.result["endpoint_cleanup_error"] = str(error)
        # Git removes only clean, explicitly created worktrees; never force/delete.
        for repository, path in reversed(self.worktrees):
            check = subprocess.run(
                ["git", "status", "--porcelain"], cwd=path, env=self.env,
                capture_output=True, text=True, timeout=10, **HTTP.PROCESS_OPTIONS)
            if check.returncode == 0 and not check.stdout.strip():
                removed = subprocess.run(
                    ["git", "worktree", "remove", str(path)], cwd=repository,
                    env=self.env, capture_output=True, text=True, timeout=10,
                    **HTTP.PROCESS_OPTIONS)
                if removed.returncode:
                    self.result.setdefault("worktree_cleanup_errors", []).append(
                        {"path": str(path), "error": removed.stderr})
            else:
                self.result.setdefault("retained_dirty_worktrees", []).append(str(path))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True)
    parser.add_argument("--heli")
    parser.add_argument("--cargo", help="Native Cargo binary with sibling rustc")
    parser.add_argument("--run", action="store_true")
    parser.add_argument("--authenticated", action="store_true",
                        help="Require a fake token via Cargo's real credential provider")
    return Experiment(parser.parse_args()).run()


if __name__ == "__main__":
    raise SystemExit(main())

