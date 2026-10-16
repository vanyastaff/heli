"""Run a tiny production-CLI dependency-epoch concurrency canary (Python 3.11+).

Requires native Cargo >=1.99, Git, a built cargo-heli, and an already-vendored
directory containing exactly itoa 1.0.18 and ryu 1.0.23. No network or third-party
Python modules. Runs three small Cargo builds; schedule separately from other
heavy local builds. Creates a fresh lab and preserves its evidence and caches.

Example: python experiments/dependency-epoch-concurrency.py --tool /path/to/cargo-heli
--vendor /path/to/prepared-vendor --lab /absolute/path/to/fresh-lab

Evidence concerns these tiny projects, not a full FLUI profile, host memory,
physical allocation, or host quotas. Source GC deliberately retains private homes.
"""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import time
import traceback
import tomllib


BUILD = r'''
use std::{env, fs, path::Path, thread, time::{Duration, Instant}};
fn hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(14695981039346656037u64, |n, b| (n ^ u64::from(*b)).wrapping_mul(1099511628211))
}
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    fs::write(env::var("CANARY_READY").unwrap(), env::var("CARGO_HOME").unwrap()).unwrap();
    let start = Instant::now();
    while !Path::new(&env::var("CANARY_RELEASE").unwrap()).is_file() {
        assert!(start.elapsed() < Duration::from_secs(240), "bounded canary gate timed out");
        thread::sleep(Duration::from_millis(25));
    }
    let mut evidence = String::new();
    for (path, digest) in env::var("CANARY_READS").unwrap().lines().zip(env::var("CANARY_HASHES").unwrap().lines()) {
        let bytes = fs::read(path).unwrap();
        assert_eq!(hash(&bytes), digest.parse::<u64>().unwrap(), "late source read differs: {path}");
        evidence.push_str(&format!("{} {} {}\n", path, bytes.len(), hash(&bytes)));
    }
    fs::write(env::var("CANARY_DONE").unwrap(), evidence).unwrap();
}
'''


def utc():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def inventory(root):
    result = {}
    for path in sorted(root.rglob('*')):
        if path.is_file():
            result[path.relative_to(root).as_posix()] = {
                'bytes': path.stat().st_size,
                'sha256': hashlib.sha256(path.read_bytes()).hexdigest(),
            }
    return result


def logical_bytes(root):
    # Follow directory junctions through os.walk, deliberately counting pathname views.
    return sum((Path(base) / name).stat().st_size
               for base, _, names in os.walk(root, followlinks=True)
               for name in names if (Path(base) / name).is_file()) if root.exists() else 0


def fnv(data):
    value = 14695981039346656037
    for byte in data:
        value = ((value ^ byte) * 1099511628211) & ((1 << 64) - 1)
    return value


class CacheLock:
    def __init__(self, path):
        self.file = open(path, 'a+b')
        self.file.seek(0)
        if os.name == 'nt':
            import msvcrt
            # Cargo's lock overlaps byte zero; file contents have no Cargo meaning.
            if path.stat().st_size == 0:
                self.file.write(b'\0')
                self.file.flush()
            self.file.seek(0)
            msvcrt.locking(self.file.fileno(), msvcrt.LK_NBLCK, 1)
        else:
            import fcntl
            fcntl.flock(self.file.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)

    def close(self):
        if self.file:
            self.file.close()
            self.file = None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--lab', type=Path, help='Fresh absolute lab directory (default: unique target/ lab under cwd)')
    parser.add_argument('--tool', type=Path, required=True, help='Existing native cargo-heli executable; copied into the lab')
    parser.add_argument('--vendor', type=Path, required=True, help='Existing vendor input, read only; exactly itoa 1.0.18 and ryu 1.0.23')
    parser.add_argument('--cargo', type=Path, help='Native Cargo >=1.99 (default: rustup which cargo)')
    args = parser.parse_args()
    if args.lab is not None and not args.lab.is_absolute():
        parser.error('--lab must be absolute')
    root = args.lab or Path.cwd() / 'target' / ('dependency-epoch-concurrency-' + datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%S%fZ'))
    root = root.resolve()
    if root.exists():
        parser.error('--lab must not already exist')
    if not args.tool.is_file():
        parser.error('--tool must name an existing executable file')
    if args.cargo is not None and not args.cargo.is_file():
        parser.error('--cargo must name an existing executable file')
    vendor = args.vendor.resolve(strict=True)
    names = {}
    versions = {}
    for entry in vendor.iterdir():
        if entry.is_dir():
            package = tomllib.loads((entry / 'Cargo.toml').read_text(encoding='utf-8'))['package']
            if package['name'] in names:
                parser.error('Vendor contains multiple versions of ' + package['name'])
            names[package['name']] = entry.name
            versions[package['name']] = package['version']
    if versions != {'itoa': '1.0.18', 'ryu': '1.0.23'}:
        parser.error('Vendor must contain exactly itoa 1.0.18 and ryu 1.0.23; got ' + repr(versions))
    if os.name != 'nt' and len(os.fsencode(root / 'heli.sock')) > 100:
        parser.error('Lab path is too long for portable Unix sockets; choose a shorter --lab')
    root.mkdir(parents=True, exist_ok=False)
    tool = root / args.tool.name
    shutil.copy2(args.tool, tool)
    report = {'started': utc(), 'lab': str(root), 'verdict': 'INCOMPLETE',
              'tool_sha256': hashlib.sha256(tool.read_bytes()).hexdigest(), 'input_versions': versions,
              'scope': 'Three tiny native Cargo consumers, two clean Git worktrees; not FLUI profile validation.',
              'limitations': ['Logical byte counts are not physical allocation or host totals.',
                              'No host RAM bound is established.',
                              'Private Cargo homes remain after source GC; their cleanup is deferred.'],
              'events': [], 'commands': [], 'gc': [], 'consumers': {}}
    env = os.environ.copy()
    for key in list(env):
        if key.startswith('HELI_') or key in ('CARGO_HOME', 'CARGO', 'CARGO_MAKEFLAGS', 'MAKEFLAGS', 'MFLAGS',
                'CARGO_TARGET_DIR', 'CARGO_BUILD_TARGET_DIR', 'CARGO_BUILD_BUILD_DIR'):
            env.pop(key, None)
    token = hashlib.sha256(str(root).encode()).hexdigest()[:16]
    env.update(HELI_STATE_DIR=str(root / 'state'),
               HELI_ENDPOINT=(r'\\.\pipe\heli-shared-' + token if os.name == 'nt' else str(root / 'heli.sock')),
               HELI_POOL='heli-shared-' + token, HELI_ACTIVITY_DIR=str(root / 'activity'),
               HELI_CONFIG=str(root / 'heli.toml'), CARGO_TERM_COLOR='never')
    (root / 'heli.toml').write_text('[pool]\nslots=8\n[store]\npaths=[' + json.dumps(str(root / 'artifacts')) + ']\nbudget_mb=2048\n')
    original_home = root / 'original-home'
    original_home.mkdir()
    env['CARGO_HOME'] = str(original_home)
    children = []
    worktrees = []
    lock = None
    daemon_pid = None
    before = inventory(vendor)
    (root / 'original-vendor-before.json').write_text(json.dumps(before, indent=2))

    def event(kind, **data):
        report['events'].append({'at': utc(), 'kind': kind, **data})

    def run(argv, cwd=root, custom_env=None, timeout=90):
        result = subprocess.run(list(map(str, argv)), cwd=cwd, env=custom_env or env,
                                capture_output=True, text=True, timeout=timeout)
        record = {'argv': list(map(str, argv)), 'cwd': str(cwd), 'code': result.returncode,
                  'stdout': result.stdout, 'stderr': result.stderr, 'at': utc()}
        report['commands'].append(record)
        if result.returncode:
            raise RuntimeError(json.dumps(record))
        return result.stdout

    def config(path, source):
        path.write_text('[source.crates-io]\nreplace-with="heli-vendor"\n[source.heli-vendor]\ndirectory=' + json.dumps(str(source)) + '\n')

    def git(repo, *argv):
        return run(['git', '-c', 'core.hooksPath=' + str(root / 'empty-hooks'),
                    '-c', 'commit.gpgsign=false', '-C', repo, *argv])

    def project(path, name, with_ryu=False, gate=True):
        (path / 'src').mkdir(parents=True)
        (path / 'Cargo.toml').write_text('[workspace]\n[package]\nname=' + json.dumps(name) +
            '\nversion="0.1.0"\nedition="2024"\n[dependencies]\nitoa="=1.0.18"\n' + ('ryu="=1.0.23"\n' if with_ryu else ''))
        (path / 'src/lib.rs').write_text('pub fn real_crate() -> String { itoa::Buffer::new().format(42).to_owned() }\n'
            '#[test] fn real_itoa() { assert_eq!(real_crate(), "42"); }\n' +
            ('#[test] fn real_ryu() { assert_eq!(ryu::Buffer::new().format(1.5), "1.5"); }\n' if with_ryu else ''))
        if gate:
            (path / 'build.rs').write_text(BUILD)
        run([cargo, 'generate-lockfile', '--offline'], path)
        git(path, 'init', '-q')
        git(path, 'add', '.')
        git(path, '-c', 'user.name=Heli canary', '-c', 'user.email=canary@invalid', 'commit', '-qm', 'Frozen canary input')
        assert not git(path, 'status', '--porcelain')

    def start(label, argv, cwd, custom_env):
        out = open(root / (label + '.stdout'), 'wb')
        err = open(root / (label + '.stderr'), 'wb')
        try:
            child = subprocess.Popen(list(map(str, argv)), cwd=cwd, env=custom_env,
                                     stdout=out, stderr=err, start_new_session=(os.name != 'nt'))
        finally:
            out.close()
            err.close()
        children.append((label, child))
        event('started', label=label, pid=child.pid, argv=list(map(str, argv)))
        return child

    def require_control_blocked():
        assert control.poll() is None, 'Ordinary Cargo unexpectedly escaped original-home cache lock'
        assert 'file lock' in (root / 'control.stderr').read_text(errors='replace').lower(), 'No Cargo lock-wait evidence'

    def wait_until(predicate, description, timeout=120):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            if predicate():
                return
            for label, child in consumers.items():
                if child.poll() is not None:
                    raise RuntimeError(f'{label} finished before {description}: {child.returncode}; see {label}.stderr')
            time.sleep(.05)
        raise TimeoutError(description)

    def collect(phase):
        result = json.loads(run([tool, 'deps', 'gc', '--store', root / 'sources']))
        report['gc'].append({'phase': phase, 'at': utc(), **result})
        return result

    consumers = {}
    try:
        cargo = str(args.cargo.resolve()) if args.cargo else run(['rustup', 'which', 'cargo']).strip()
        env['CARGO'] = cargo
        report['cargo_path'] = cargo
        report['cargo_version'] = run([cargo, '--version', '--verbose']).strip()
        version = re.match(r'cargo (\d+)\.(\d+)\.(\d+)(?:[- ]|$)', report['cargo_version'])
        if not version or tuple(map(int, version.groups())) < (1, 99, 0):
            raise RuntimeError('This canary requires a recognizable native Cargo >=1.99')
        report['git_version'] = run(['git', '--version']).strip()
        report['tool_version'] = run([tool, '--version']).strip()
        config(original_home / 'config.toml', vendor)
        vendor_a = root / 'vendor-a'
        vendor_a.mkdir()
        shutil.copytree(vendor / names['itoa'], vendor_a / names['itoa'])
        config(root / 'ab.toml', vendor)
        config(root / 'a.toml', vendor_a)
        epoch_ab = Path(run([tool, 'deps', 'import', '--store', root / 'sources', '--vendor', vendor, '--config', root / 'ab.toml']).strip())
        epoch_a = Path(run([tool, 'deps', 'import', '--store', root / 'sources', '--vendor', vendor_a, '--config', root / 'a.toml']).strip())
        ma = json.loads((epoch_a / 'manifest.json').read_text())
        mab = json.loads((epoch_ab / 'manifest.json').read_text())
        assert ma['identity'] != mab['identity']
        assert ma['packages'][names['itoa']] == mab['packages'][names['itoa']]
        source_a = epoch_a / 'vendor' / names['itoa'] / 'src/lib.rs'
        source_ab = epoch_ab / 'vendor' / names['itoa'] / 'src/lib.rs'
        assert os.path.samefile(source_a, source_ab)
        report['epochs'] = {'A': str(epoch_a), 'AB': str(epoch_ab), 'manifest_A': ma, 'manifest_AB': mab}
        report['shared_itoa'] = {str(path): {'device': path.stat().st_dev, 'file_id': path.stat().st_ino} for path in (source_a, source_ab)}
        report['shared_itoa']['samefile'] = True
        main_repo = root / 'main-project'
        project(main_repo, 'shared-itoa-canary')
        c_repo = root / 'project-c'
        project(c_repo, 'shared-two-crates-canary', with_ryu=True)
        control_repo = root / 'control-project'
        project(control_repo, 'ordinary-cargo-control', gate=False)
        report['project_git_commits'] = {path.name: git(path, 'rev-parse', 'HEAD').strip()
                                         for path in (main_repo, c_repo, control_repo)}
        for label in ('A', 'B'):
            path = root / ('worktree-' + label)
            git(main_repo, 'worktree', 'add', '--detach', str(path), 'HEAD')
            worktrees.append((main_repo, path))
            assert not git(path, 'status', '--porcelain')
        lock = CacheLock(original_home / '.package-cache')
        event('original-cache-locked')
        control = start('control', [cargo, 'metadata', '--format-version', '1', '--offline', '--locked'], control_repo, env)
        wait_until(lambda: 'file lock' in (root / 'control.stderr').read_text(errors='replace').lower(), 'baseline cache-lock wait', 20)
        require_control_blocked()
        for label, cwd, epoch, command in [('A', root / 'worktree-A', epoch_a, 'check'),
                                          ('B', root / 'worktree-B', epoch_ab, 'test'),
                                          ('C', c_repo, epoch_ab, 'test')]:
            reads = [epoch / 'vendor' / names['itoa'] / 'src/lib.rs']
            if label != 'A':
                reads.append(epoch / 'vendor' / names['ryu'] / 'src/lib.rs')
            child_env = env.copy()
            child_env.update(CARGO_TARGET_DIR=str(root / 'targets' / label),
                             CARGO_BUILD_BUILD_DIR=str(root / 'intermediates' / label),
                             CANARY_READY=str(root / (label + '.ready')),
                             CANARY_RELEASE=str(root / (label + '.release')),
                             CANARY_DONE=str(root / (label + '.done')),
                             CANARY_READS='\n'.join(map(str, reads)),
                             CANARY_HASHES='\n'.join(str(fnv(path.read_bytes())) for path in reads))
            consumers[label] = start(label, [tool, '--output', 'full', '--dependency-epoch', epoch,
                '--cache', 'off', command, '--offline', '--locked'], cwd, child_env)
        wait_until(lambda: all((root / (label + '.ready')).exists() for label in consumers), 'all three overlapping build-script gates')
        homes = {label: (root / (label + '.ready')).read_text() for label in consumers}
        assert len(set(homes.values())) == 3, homes
        assert all(Path(home).resolve() != original_home for home in homes.values())
        report['private_homes'] = homes
        event('all-three-ready-and-blocked', homes=homes)
        state = json.loads(run([tool, 'status', '--json']))
        daemon_pid = state['daemon']['pid']
        report['isolated_daemon_pid'] = daemon_pid
        require_control_blocked()
        busy = collect('all-three-busy')
        assert not busy['retired_epochs'] and not busy['removed_objects'], busy
        assert set(busy['busy_epochs']) == {ma['identity'], mab['identity']}, busy
        report['logical_bytes_at_overlap'] = {
            'source_objects_unique_store_paths': logical_bytes(root / 'sources/objects'),
            'source_epoch_views_counts_shared_payload_twice': logical_bytes(root / 'sources/epochs'),
            'original_vendor_input': logical_bytes(vendor), 'copied_vendor_a_input': logical_bytes(vendor_a),
            'private_homes': {label: logical_bytes(Path(home)) for label, home in homes.items()},
            'targets': logical_bytes(root / 'targets'), 'intermediates': logical_bytes(root / 'intermediates')}
        (root / 'A.release').touch()
        event('release-A-only')
        assert consumers['A'].wait(timeout=90) == 0
        assert (root / 'A.done').exists()
        report['consumers']['A'] = {'code': 0, 'completed': utc(), 'late_read': (root / 'A.done').read_text()}
        assert consumers['B'].poll() is None and consumers['C'].poll() is None
        require_control_blocked()
        deadline = time.monotonic() + 15
        while True:
            result = collect('A-finished-BC-busy')
            if ma['identity'] in result['retired_epochs']:
                break
            assert time.monotonic() < deadline, 'A lease did not release after completed client'
            time.sleep(.15)
        assert not epoch_a.exists() and epoch_ab.exists()
        assert result['busy_epochs'] == [mab['identity']] and not result['removed_objects'], result
        assert {path.name for path in (root / 'sources/objects').iterdir()} == set(mab['packages'].values())
        assert len(mab['packages']) == 2
        assert source_ab.is_file()
        event('A-retired-two-shared-objects-preserved-BC-still-blocked')
        assert consumers['B'].poll() is None and consumers['C'].poll() is None
        require_control_blocked()
        for label in ('B', 'C'):
            (root / (label + '.release')).touch()
        event('release-BC')
        for label in ('B', 'C'):
            assert consumers[label].wait(timeout=90) == 0
            done = root / (label + '.done')
            assert done.exists()
            report['consumers'][label] = {'code': 0, 'completed': utc(), 'late_read': done.read_text()}
        require_control_blocked()
        report['control_blocked_through_managed_load'] = True
        lock.close()
        lock = None
        event('original-cache-unlocked')
        assert control.wait(timeout=45) == 0
        report['control'] = {'code': 0, 'stderr': (root / 'control.stderr').read_text(errors='replace'),
                             'metadata': json.loads((root / 'control.stdout').read_text())}
        deadline = time.monotonic() + 15
        while True:
            collect('all-consumers-idle')
            if not list((root / 'sources/epochs').iterdir()) and not list((root / 'sources/objects').iterdir()):
                break
            assert time.monotonic() < deadline, 'Idle epoch/object collection incomplete'
            time.sleep(.15)
        report['private_homes_retained_after_gc'] = {label: Path(home).exists() for label, home in homes.items()}
        assert all(report['private_homes_retained_after_gc'].values())
        for repo, path in worktrees:
            assert not git(path, 'status', '--porcelain'), 'Canary dirtied its Git worktree'
            git(repo, 'worktree', 'remove', str(path))
        worktrees.clear()
        after = inventory(vendor)
        (root / 'original-vendor-after.json').write_text(json.dumps(after, indent=2))
        assert before == after, 'Original vendor input changed'
        report['original_vendor_unchanged'] = True
        report['verdict'] = 'PASS'
    except BaseException:
        report['verdict'] = 'FAIL'
        report['error'] = traceback.format_exc()
        raise
    finally:
        for label in ('A', 'B', 'C'):
            (root / (label + '.release')).touch(exist_ok=True)
        if lock:
            lock.close()
        for label, child in children:
            try:
                child.wait(timeout=20)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=10)
            report.setdefault('final_child_codes', {})[label] = child.returncode
        # Only this fresh endpoint's returned daemon PID is eligible for cleanup.
        try:
            state = json.loads(run([tool, 'status', '--json']))
            current = state.get('daemon')
            if current and (daemon_pid is None or current['pid'] == daemon_pid):
                daemon_pid = current['pid']
                os.kill(daemon_pid, signal.SIGTERM)
                report['stopped_isolated_daemon_pid'] = daemon_pid
        except Exception as error:
            report['daemon_cleanup_error'] = str(error)
        for repo, path in worktrees:
            try:
                if not git(path, 'status', '--porcelain'):
                    git(repo, 'worktree', 'remove', str(path))
                else:
                    report.setdefault('cleanup_notes', []).append('Retained dirty own worktree: ' + str(path))
            except Exception as error:
                report.setdefault('cleanup_notes', []).append(str(error))
        report['finished'] = utc()
        (root / 'results.json').write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps({'verdict': report['verdict'], 'evidence': str(root / 'results.json')}))


if __name__ == '__main__':
    main()
