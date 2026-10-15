#!/usr/bin/env python3
"""Real multi-project acceptance for shared immutable Cargo directory sources.

Extends multi-project-spike.py without changing production run policy. Reads
ordinary files from --source-home; it never opens that home's cache locks.
Only a NEW laboratory Cargo home is deliberately locked. Network is disabled.
Every consumer gets private mutable Cargo bookkeeping and the same frozen
directory sources. The experiment intentionally does not migrate user Cargo
home configuration or credentials; that remains a production integration gate.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import time
import tomllib
import uuid


sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location(
    "multi_project_spike", Path(__file__).with_name("multi-project-spike.py"))
BASE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(BASE)
REGISTRY = "index.crates.io-1949cf8c6b5b557f"
SOURCE = "registry+https://github.com/rust-lang/crates.io-index"


def index_key(name):
    name = name.lower()
    if len(name) == 1:
        return "1/" + name
    if len(name) == 2:
        return "2/" + name
    if len(name) == 3:
        return "3/" + name[0] + "/" + name
    return name[:2] + "/" + name[2:4] + "/" + name


def bytes_under(path):
    return sum(p.stat().st_size for p in path.rglob("*") if p.is_file())


def inventory(root):
    digest = hashlib.sha256()
    count = total = 0
    for path in sorted(p for p in root.rglob("*") if p.is_file()):
        name = path.relative_to(root).as_posix().encode()
        content = path.read_bytes()
        digest.update(len(name).to_bytes(8, "little"))
        digest.update(name)
        digest.update(len(content).to_bytes(8, "little"))
        digest.update(content)
        total += len(content)
        count += 1
    return {"sha256": digest.hexdigest(), "files": count, "logical_bytes": total}


class EpochSpike(BASE.Spike):
    def __init__(self, args):
        super().__init__(args)
        self.epoch = {}
        self.holder = self.control = None
        self.control_log = self.holder_log = None
        self.private_homes = {}
        self.toolchains = {}

    def prepare(self):
        super().prepare()
        # Resolve with each checkout's rust-toolchain selection BEFORE replacing
        # CARGO_HOME. Keep different checkout selections (including rustup
        # aliases pointing at equivalent versions) rather than forcing one.
        for prefix in ("h", "f"):
            checkout = self.root / "w" / (prefix + "1")
            self.toolchains[prefix] = tuple(
                BASE.checked(["rustup", "which", tool], checkout, self.base)
                for tool in ("cargo", "rustc"))
        cargo, rustc = self.toolchains["h"]
        self.base.update(CARGO=cargo, RUSTC=rustc,
                         PATH=str(Path(cargo).parent) + os.pathsep + self.base["PATH"])
        seed = self.root / "publisher-home"
        if self.args.reuse_epoch_from:
            previous_manifest = Path(self.args.reuse_epoch_from).resolve(strict=True)
            previous = json.loads(previous_manifest.read_text(encoding="utf-8"))["dependency_epoch"]
            vendor = Path(previous.get("vendor_path", str(previous_manifest.parent / "vendor-epoch-1"))).resolve(strict=True)
            self.vendor_config = ('[source.crates-io]\nreplace-with="vendored-sources"\n'
                                  '[source.vendored-sources]\ndirectory='
                                  + json.dumps(str(vendor)) + '\n')
            before = inventory(vendor)
            if before != previous["vendor_before"]:
                raise RuntimeError("Previously published vendor inventory changed")
            locks = {str(checkout / "Cargo.lock"): BASE.digest(checkout / "Cargo.lock")
                     for checkout in (self.root / "w/h1", self.root / "w/f1")}
            if sorted(locks.values()) != sorted(previous["lockfiles_before"].values()):
                raise RuntimeError("Reused snapshot requires the same locked dependency graphs")
            self.vendor = vendor
            self.epoch.update(reused_from=str(previous_manifest), vendor_before=before,
                              vendor_path=str(vendor), lockfiles_before=locks,
                              toolchains=self.toolchains)
            seed.mkdir()
            self.finish_prepare(seed)
            return
        source_home = Path(self.args.source_home).resolve(strict=True)
        packages = {}
        lock_hashes = {}
        for checkout in (self.root / "w/h1", self.root / "w/f1"):
            lock = checkout / "Cargo.lock"
            lock_hashes[str(lock)] = BASE.digest(lock)
            for package in tomllib.loads(lock.read_text(encoding="utf-8"))["package"]:
                if "source" not in package:
                    continue
                if package["source"] != SOURCE:
                    raise RuntimeError("Unimplemented source identity: " + package["source"])
                identity = (package["name"], package["version"])
                if identity in packages and packages[identity]["checksum"] != package["checksum"]:
                    raise RuntimeError(f"Conflicting lockfile checksum: {identity}")
                packages[identity] = package
        # Verify archives against Cargo.lock before copying; sparse index copies
        # are private, so Cargo may update its bookkeeping without shared writes.
        copies = {Path("index") / REGISTRY / "config.json"}
        archive_bytes = 0
        for (name, version), package in packages.items():
            relative = Path("cache") / REGISTRY / f"{name}-{version}.crate"
            archive = source_home / "registry" / relative
            if BASE.digest(archive) != package["checksum"]:
                raise RuntimeError(f"Archive checksum mismatch: {archive}")
            archive_bytes += archive.stat().st_size
            copies.add(relative)
            copies.add(Path("index") / REGISTRY / ".cache" / index_key(name))
        for relative in sorted(copies):
            destination = seed / "registry" / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source_home / "registry" / relative, destination)
        self.epoch.update(packages=len(packages), archive_bytes=archive_bytes,
                          source_home=str(source_home), lockfiles_before=lock_hashes,
                          cargo=cargo, rustc=rustc, toolchains=self.toolchains,
                          original_home_configuration="Not migrated in this isolated experiment")
        env = self.base | {"CARGO_HOME": str(seed)}
        vendor = self.root / "vendor-epoch-1"
        args = [cargo, "vendor", "--offline", "--locked", "--manifest-path",
                self.root / "w/h1/Cargo.toml", "--sync", self.root / "w/f1/Cargo.toml", vendor]
        print(f"Vendoring {len(packages)} locked packages from private seed", flush=True)
        result = BASE.command(args, self.root, env, timeout=600)
        BASE.emit(self.root / "vendor.json", result)
        if result["exit"]:
            raise RuntimeError("Offline vendor failed; see vendor.json")
        self.vendor_config = result["stdout"]
        # Validate before using generated source-replacement configuration.
        config = tomllib.loads(self.vendor_config)
        if Path(config["source"]["vendored-sources"]["directory"]).resolve() != vendor:
            raise RuntimeError("Unexpected Cargo vendor source replacement")
        for path in vendor.rglob("*"):
            if path.is_file():
                path.chmod(stat.S_IREAD | (stat.S_IEXEC if path.stat().st_mode & stat.S_IEXEC else 0))
        self.epoch["vendor_before"] = inventory(vendor)
        self.epoch["vendor_path"] = str(vendor)
        self.epoch["seed_bytes"] = bytes_under(seed)
        self.vendor = vendor
        self.finish_prepare(seed)

    def finish_prepare(self, seed):
        (seed / "config.toml").write_text(self.vendor_config, encoding="utf-8")
        # Administrative probes also avoid the real mutable home. They get a
        # DIFFERENT small home from the deliberately blocked control/publisher.
        self.base["CARGO_HOME"] = str(self.home_for("administration"))
        self.manifest["dependency_epoch"] = self.epoch
        BASE.emit(self.root / "manifest.json", self.manifest)
        self.begin_control(seed)

    def home_for(self, name):
        if name not in self.private_homes:
            home = self.root / "agent-homes" / name
            home.mkdir(parents=True, exist_ok=False)
            (home / "config.toml").write_text(self.vendor_config, encoding="utf-8")
            self.private_homes[name] = home
        return self.private_homes[name]

    def begin_control(self, seed):
        ready, release = self.root / "cache-lock-ready", self.root / "cache-lock-release"
        self.holder_log = (self.root / "cache-lock-holder.log").open("wb")
        self.holder = subprocess.Popen(
            [self.base["RUSTC_WRAPPER"], "--spike-hold-lock", str(seed / ".package-cache"),
             str(ready), str(release), str(self.args.timeout + 600)], cwd=self.root, env=self.base,
            stdout=self.holder_log, stderr=subprocess.STDOUT, creationflags=BASE.CREATE_FLAGS)
        deadline = time.monotonic() + 10
        while not ready.exists():
            if self.holder.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError("Private laboratory cache lock was not acquired")
            time.sleep(0.02)
        self.control_log = (self.root / "shared-home-control.log").open("wb")
        self.control = subprocess.Popen(
            [self.base["CARGO"], "metadata", "--frozen", "--format-version", "1"],
            cwd=self.root / "w/h1", env=self.base | {"CARGO_HOME": str(seed)},
            stdout=self.control_log, stderr=subprocess.STDOUT, creationflags=BASE.CREATE_FLAGS)
        deadline = time.monotonic() + 10
        while "Blocking waiting for file lock on package cache" not in (
                self.root / "shared-home-control.log").read_text(errors="replace"):
            if self.control.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError("Shared-home negative control did not demonstrate package-cache blocking")
            time.sleep(0.02)
        self.epoch["control_blocked_before_workload"] = True
        print("Laboratory shared-home control blocked; private-home agents can start", flush=True)

    def start(self, name, cwd, args, extra_env=None):
        # A writable home belongs to a checkout, not to an individual command.
        owner = cwd.relative_to(self.root).as_posix().replace("/", "-")
        relative = cwd.relative_to(self.root)
        prefix = relative.parts[1][0] if relative.parts[0] == "w" else "h"
        cargo, rustc = self.toolchains[prefix]
        extra = {"CARGO_HOME": str(self.home_for(owner)), "CARGO": cargo, "RUSTC": rustc,
                 "PATH": str(Path(cargo).parent) + os.pathsep + self.base["PATH"]} | (extra_env or {})
        if name == "heli-test" and self.args.full_heli_suite:
            args = ["test", "--locked", "--offline", "--no-fail-fast"]
        return super().start(name, cwd, args, extra)

    def load(self):
        try:
            super().load()
            self.epoch["control_still_blocked_after_workload"] = (
                self.control.poll() is None and self.holder.poll() is None)
            self.epoch["vendor_after"] = inventory(self.vendor)
            self.epoch["private_home_bytes"] = {name: bytes_under(home)
                                                 for name, home in self.private_homes.items()}
            self.epoch["lockfiles_after"] = {path: BASE.digest(Path(path))
                                               for path in self.epoch["lockfiles_before"]}
        finally:
            self.release_control()

    def release_control(self):
        if self.holder:
            (self.root / "cache-lock-release").write_text("release\n", encoding="utf-8")
            self.holder.wait(timeout=10)
            self.holder_log.close()
            self.holder = None
        if self.control:
            self.epoch["control_exit_after_release"] = self.control.wait(timeout=120)
            self.control_log.close()
            self.control = None
        if self.root.exists():
            BASE.emit(self.root / "dependency-epoch.json", self.epoch)

    def summary(self, error=None):
        required = (self.epoch.get("control_still_blocked_after_workload") is True
                    and self.epoch.get("control_exit_after_release") == 0
                    and self.epoch.get("vendor_before") == self.epoch.get("vendor_after")
                    and self.epoch.get("lockfiles_before") == self.epoch.get("lockfiles_after"))
        if not required and error is None:
            error = "Dependency epoch/control integrity checks failed; see dependency-epoch.json"
        return super().summary(error)

    def cleanup(self):
        self.release_control()
        super().cleanup()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--heli", required=True)
    parser.add_argument("--heli-repo", default="E:/heli")
    parser.add_argument("--flui-repo", default="D:/flui")
    parser.add_argument("--heli-ref", default="HEAD")
    parser.add_argument("--flui-ref", required=True)
    parser.add_argument("--source-home", default=str(Path.home() / ".cargo"))
    parser.add_argument("--reuse-epoch-from", help="Prior laboratory manifest; verify and reuse its immutable vendor instead of duplicating it")
    parser.add_argument("--output", default=f"E:/heli/target/epoch-{time.strftime('%m%d-%H%M%S')}-{uuid.uuid4().hex[:4]}")
    parser.add_argument("--budget-mib", type=int, default=2048)
    parser.add_argument("--result-cache", choices=["off", "refresh"], default="off")
    parser.add_argument("--payload-mib", type=int, default=128)
    parser.add_argument("--cycles", type=int, default=2)
    parser.add_argument("--timeout", type=int, default=2400)
    parser.add_argument("--keep-worktrees", action="store_true")
    parser.add_argument("--full-heli-suite", action="store_true")
    args = parser.parse_args()
    spike = EpochSpike(args)
    error = result = None
    try:
        spike.prepare()
        print(f"Isolated epoch experiment: {spike.root}", flush=True)
        spike.load()
    except Exception as exc:
        error = repr(exc)
        print(error, flush=True)
        spike.release_control()
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
