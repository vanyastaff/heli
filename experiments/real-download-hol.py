#!/usr/bin/env python3
"""Reproduce Cargo download head-of-line blocking with a local sparse registry.

Preparation alone creates fixtures; --run explicitly permits tiny Cargo builds.
Compare a warm shared home, ordinary private source view, and Heli epoch while
another real Cargo fetch waits for its HTTP archive response. This is a CLI
regression fixture, not a throughput benchmark. Only owned processes, homes,
outputs, server and daemon are touched; no user cache or external network.
"""
import argparse
import gzip
import hashlib
import http.server
import io
import json
import os
from pathlib import Path
import re
import signal
import stat
import subprocess
import tarfile
import tempfile
import threading
import time
import tomllib
import urllib.parse
import uuid
CREATE_FLAGS = subprocess.CREATE_NO_WINDOW if os.name == 'nt' else 0
PROCESS_OPTIONS = dict(creationflags=CREATE_FLAGS) if os.name == 'nt' else dict(start_new_session=True)

def emit(p, v):
    Path(p).write_text(json.dumps(v, indent=2) + '\n', encoding='utf-8')

def sha(p):
    return hashlib.sha256(Path(p).read_bytes()).hexdigest()

def index_key(n):
    return n[:2] + '/' + n[2:4] + '/' + n

def archive(name):
    data = io.BytesIO()
    with gzip.GzipFile(fileobj=data, mode='wb', mtime=0) as gz:
        with tarfile.open(fileobj=gz, mode='w') as t:
            files = {
                'Cargo.toml': (
                    f'[package]\nname="{name}"\nversion="0.0.1"\nedition="2024"\n'),
                'src/lib.rs': 'pub fn value() -> u32 { 42 }\n',
            }
            for path, content in files.items():
                b = content.encode()
                m = tarfile.TarInfo(name + '-0.0.1/' + path)
                m.size = len(b)
                m.mode = 0o644
                m.mtime = 0
                t.addfile(m, io.BytesIO(b))
    return data.getvalue()

def project(path, dependency, gate_marker=False):
    (path / 'src').mkdir(parents=True)
    manifest = (
        '[package]\nname="' + path.name.replace('_', '-')
        + '"\nversion="0.0.0"\nedition="2024"\n[workspace]\n[dependencies]\n'
        + dependency + '={version="=0.0.1",registry="lab"}\n')
    (path / 'Cargo.toml').write_text(manifest, encoding='utf-8')
    (path / 'src/lib.rs').write_text('pub fn value() -> u32 { ' + dependency.replace('-', '_') + '::value() }\n')
    if gate_marker:
        (path / 'build.rs').write_text('''fn main() {
    std::fs::write(std::env::var_os("HOL_B_MARKER").unwrap(),
                   b"consumer-build-script-ran").unwrap();
    println!("cargo:rerun-if-env-changed=HOL_B_MARKER");
}
''', encoding='utf-8')

def kill_owned(p):
    if p is None or p.poll() is not None:
        return
    if os.name == 'nt':
        subprocess.run(
            ['taskkill', '/PID', str(p.pid), '/T', '/F'],
            capture_output=True, timeout=10, creationflags=CREATE_FLAGS)
    else:
        os.killpg(p.pid, signal.SIGKILL)
    try:
        p.wait(timeout=5)
    except subprocess.TimeoutExpired:
        pass

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', required=True)
    parser.add_argument('--heli', help='Built cargo-heli binary, required with --run')
    parser.add_argument('--cargo', help='Native Cargo binary; default: rustup which cargo in this checkout')
    parser.add_argument('--run', action='store_true',
                        help='Permit tiny Cargo setup and checks')
    args = parser.parse_args()
    root = Path(args.output).resolve()
    root.mkdir(parents=True, exist_ok=False)
    names = ['hol-warm', 'hol-slow-shared', 'hol-slow-private', 'hol-slow-epoch']
    payloads = {n: archive(n) for n in names}
    generated = root / 'registry-input'
    generated.mkdir()
    for n, b in payloads.items():
        (generated / (n + '-0.0.1.crate')).write_bytes(b)
    bproject = root / 'consumer-b'
    project(bproject, 'hol-warm', True)
    aproj = {}
    for n in names[1:]:
        aproj[n] = root / (n + '-project')
        project(aproj[n], n)
    result = {
        'status': 'PREPARED',
        'scope': 'Local real Cargo download; no artificial cache-lock holder',
        'root': str(root),
        'packages': {n: hashlib.sha256(b).hexdigest() for n, b in payloads.items()},
        'commands': [], 'phases': [],
        'limitations': [
            'Synthetic tiny registry workload, genuine Cargo/HTTP locking',
            'No throughput or universal-concurrency claim',
            'No disk/RAM quota claim',
        ],
    }
    if not args.run:
        emit(root / 'results.json', result)
        print('PREPARED; Cargo execution disabled. New --output and --run required for actual experiment.')
        return 0
    if not args.heli:
        raise ValueError('--heli required with --run')
    heli = Path(args.heli).resolve(strict=True)
    result['heli_sha256'] = sha(heli)
    env = {
        k: v for k, v in os.environ.items()
        if not k.startswith(('HELI', 'CARGO_'))
        and k not in ('RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER', 'MAKEFLAGS')
    }
    env['NO_PROXY'] = '127.0.0.1,localhost'
    env['no_proxy'] = env['NO_PROXY']
    # Unexpected external Cargo URLs go to a closed local proxy port.
    env['CARGO_HTTP_PROXY'] = 'http://127.0.0.1:9'
    if args.cargo:
        cargo = str(Path(args.cargo).resolve(strict=True))
        rustc = str(Path(cargo).with_name('rustc.exe' if os.name == 'nt' else 'rustc'))
        if not Path(rustc).is_file():
            raise ValueError('--cargo must be a native toolchain binary with sibling rustc')
    else:
        # Resolve before replacing CARGO_HOME, retaining this checkout's toolchain.
        cargo = subprocess.check_output(
            ['rustup', 'which', 'cargo'], text=True, timeout=30, **PROCESS_OPTIONS).strip()
        rustc = subprocess.check_output(
            ['rustup', 'which', 'rustc'], text=True, timeout=30, **PROCESS_OPTIONS).strip()
    env |= {
        'CARGO': cargo, 'RUSTC': rustc,
        'PATH': str(Path(cargo).parent) + os.pathsep + env['PATH'],
        'CARGO_BUILD_JOBS': '2', 'CARGO_INCREMENTAL': '0',
        'CARGO_HTTP_TIMEOUT': '30',
    }
    shared = root / 'shared-home'
    shared.mkdir()
    events = []
    event_lock = threading.Lock()
    # Each arm downloads a different cold package, so no manual cache deletion.
    gates = {n: (threading.Event(), threading.Event()) for n in names[1:]}
    owned = []
    logs = []
    daemon = None
    server = None
    endpoint_directory = None

    def log_event(**data):
        with event_lock:
            events.append(dict(epoch_ns=time.time_ns(), **data))
            emit(root / 'http-events.json', events)

    class Handler(http.server.BaseHTTPRequestHandler):

        def log_message(self, *a):
            pass

        def do_GET(self):
            path = urllib.parse.urlsplit(self.path).path
            log_event(path=path, event='request')
            if path == '/index/config.json':
                body = json.dumps({'dl': base + '/api/v1/crates', 'api': base, 'auth-required': False}).encode()
            elif path.startswith('/index/'):
                n = path.rsplit('/', 1)[-1]
                if n not in payloads or path != '/index/' + index_key(n):
                    self.send_error(404)
                    return
                entry = {
                    'name': n, 'vers': '0.0.1', 'deps': [],
                    'cksum': hashlib.sha256(payloads[n]).hexdigest(),
                    'features': {}, 'yanked': False,
                }
                body = (json.dumps(entry) + '\n').encode()
            else:
                match = re.fullmatch('/api/v1/crates/([^/]+)/0[.]0[.]1/download', path)
                if not match or match[1] not in payloads:
                    self.send_error(404)
                    return
                n = match[1]
                if n in gates:
                    gates[n][0].set()
                    log_event(event='download-gated', package=n)
                    if not gates[n][1].wait(25):
                        log_event(event='gate-timeout', package=n)
                    log_event(event='download-released', package=n)
                body = payloads[n]
            self.send_response(200)
            self.send_header('Content-Length', str(len(body)))
            self.send_header('Cache-Control', 'max-age=0')
            self.end_headers()
            try:
                self.wfile.write(body)
            except (BrokenPipeError, ConnectionResetError):
                log_event(event='client-disconnected', path=path)

    def cmd(label, argv, cwd, home, timeout=30, extra=None, require=True):
        start = time.monotonic()
        p = subprocess.Popen(
            [str(x) for x in argv], cwd=cwd,
            env=env | {'CARGO_HOME': str(home)} | (extra or {}),
            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, encoding='utf-8', errors='replace', **PROCESS_OPTIONS)
        owned.append(p)
        try:
            stdout, stderr = p.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            kill_owned(p)
            stdout, stderr = p.communicate(timeout=5)
            result['commands'].append(dict(
                label=label, timeout_seconds=timeout, stdout=stdout, stderr=stderr))
            emit(root / 'results.json', result)
            raise
        row = dict(label=label, argv=[str(x) for x in argv], exit=p.returncode,
                   seconds=time.monotonic() - start, stdout=stdout, stderr=stderr)
        result['commands'].append(row)
        emit(root / 'results.json', result)
        if require and p.returncode:
            raise RuntimeError(label + ' failed')
        return row

    def spawn(label, argv, cwd, home, extra=None):
        stream = (root / (label + '.log')).open('wb')
        logs.append(stream)
        p = subprocess.Popen(
            [str(x) for x in argv], cwd=cwd,
            env=env | {'CARGO_HOME': str(home)} | (extra or {}),
            stdout=stream, stderr=subprocess.STDOUT, **PROCESS_OPTIONS)
        owned.append(p)
        return p
    try:
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        server.daemon_threads = True
        base = 'http://127.0.0.1:' + str(server.server_port)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        registry = '[registries.lab]\nindex="sparse+' + base + '/index/"\n'
        (shared / 'config.toml').write_text(registry)
        result['registry_url'] = base
        result['cargo'] = cmd('cargo-version', [cargo, '-Vv'], root, shared)['stdout']
        result['rustc'] = cmd('rustc-version', [rustc, '-Vv'], root, shared)['stdout']
        version = re.match(r'cargo (\d+)\.(\d+)', result['cargo'])
        if version is None or tuple(map(int, version.groups())) < (1, 99):
            raise RuntimeError('This fixture requires Cargo 1.99 or newer')
        for n, path in aproj.items():
            cmd(n + '-lock', [cargo, 'generate-lockfile'], path, shared)
        cmd('b-lock', [cargo, 'generate-lockfile'], bproject, shared)
        lock_before = sha(bproject / 'Cargo.lock')
        cmd('b-warm-fetch', [cargo, 'fetch', '--locked'], bproject, shared)
        vendor = root / 'actual-cargo-vendor-b'
        vc = cmd('b-actual-vendor', [
            cargo, 'vendor', '--locked', '--offline',
            '--respect-source-config', vendor], bproject, shared)['stdout']
        vendor_config = root / 'actual-vendor-config.toml'
        vendor_config.write_text(vc)
        result['actual_vendor_config'] = tomllib.loads(vc)
        ordinary_home = root / 'ordinary-private-home'
        ordinary_home.mkdir()
        (ordinary_home / 'config.toml').write_text(registry + '\n' + vc)
        imported = cmd('public-epoch-import', [
            heli, 'deps', 'import', '--store', root / 'source-store',
            '--vendor', vendor, '--config', vendor_config], root, shared)
        epoch = Path(imported['stdout'].strip()).resolve()
        result['epoch'] = str(epoch)
        heli_config = root / 'heli.toml'
        heli_config.write_text(
            '[pool]\nslots=4\n[store]\npaths=["'
            + (root / 'managed-store').as_posix()
            + '"]\nbudget_mb=512\nfloor_mb=0\n', encoding='utf-8')
        isolated = {
            'HELI_STATE_DIR': str(root / 'state'),
            'HELI_CONFIG': str(heli_config),
            'HELI_ACTIVITY_DIR': str(root / 'activity'),
            'HELI_POOL': 'heli-http-hol-' + uuid.uuid4().hex,
        }
        isolated['HELI_ENDPOINT'] = (
            '\\\\.\\pipe' + '\\' + isolated['HELI_POOL']
            if os.name == 'nt' else str(root / 'heli.sock'))
        if os.name != 'nt':
            isolated['HELI_POOL'] = str(root / 'pool')
            # macOS sockaddr_un is smaller than Linux's; count encoded bytes.
            # A newly created private /tmp directory avoids long CI temp paths.
            if len(os.fsencode(isolated['HELI_ENDPOINT'])) > 100:
                endpoint_directory = Path(tempfile.mkdtemp(
                    prefix='heli-hol-', dir='/tmp'))
                isolated['HELI_ENDPOINT'] = str(endpoint_directory / 'h.sock')
        result['endpoint'] = isolated['HELI_ENDPOINT']
        daemon = spawn('own-daemon', [heli, 'daemon'], root, shared, isolated)
        if daemon.poll() is not None:
            raise RuntimeError('own daemon failed before workload')
        # Public readiness handshake; the endpoint is unique even on Windows.
        cmd('own-ensure', [heli, 'ensure'], root, shared, extra=isolated)
        if daemon.poll() is not None:
            raise RuntimeError('own daemon exited during readiness handshake')
        arms = [('shared', 'hol-slow-shared'),
                ('ordinary-private', 'hol-slow-private'),
                ('heli-epoch', 'hol-slow-epoch')]
        for arm, n in arms:
            phase = dict(arm=arm, a_package=n, submitted_ns=time.time_ns())
            result['phases'].append(phase)
            start = time.monotonic()
            deadline = start + 30
            a = spawn('a-' + arm, [cargo, 'fetch', '--locked'], aproj[n], shared)
            phase['a_pid'] = a.pid
            if not gates[n][0].wait(min(10, max(0, deadline - time.monotonic()))):
                raise RuntimeError('actual slow HTTP download never reached gate')
            phase['http_gate_ready_ns'] = time.time_ns()
            marker = root / ('marker-' + arm)
            target = root / ('target-' + arm)
            extra = {'HOL_B_MARKER': str(marker), 'CARGO_TARGET_DIR': str(target)}
            if arm == 'shared':
                argv = [cargo, 'check', '--lib', '--locked', '--offline']
                home = shared
            elif arm == 'ordinary-private':
                argv = [cargo, 'check', '--lib', '--locked', '--offline']
                home = ordinary_home
            else:
                argv = [heli, '--cache', 'off', '--output', 'full',
                        '--dependency-epoch', epoch, 'exec', '--',
                        cargo, 'check', '--lib', '--locked', '--offline']
                home = shared
                extra |= isolated
            b = spawn('b-' + arm, argv, bproject, home, extra)
            phase['b_pid'] = b.pid
            phase['b_submitted_ns'] = time.time_ns()
            hold = 3 if arm == 'shared' else 10
            until = min(deadline, time.monotonic() + hold)
            while b.poll() is None and time.monotonic() < until:
                time.sleep(0.025)
            b_log = (root / ('b-' + arm + '.log')).read_text(errors='replace')
            phase |= {
                'b_exit_while_a_gated': b.poll(),
                'a_alive_while_gated': a.poll() is None,
                'http_release_absent': not gates[n][1].is_set(),
                'marker_while_a_gated': marker.exists(),
                'b_log_package_cache_wait': (
                    'Blocking waiting for file lock on package cache' in b_log),
                'rmeta_while_gated': [
                    str(p.relative_to(target)) for p in target.rglob('*.rmeta')
                ] if target.exists() else [],
            }
            phase['release_ns'] = time.time_ns()
            gates[n][1].set()
            phase['a_exit'] = a.wait(timeout=max(1, deadline - time.monotonic()))
            phase['b_exit_after_release'] = b.wait(timeout=max(1, deadline - time.monotonic()))
            phase['elapsed_seconds'] = time.monotonic() - start
            phase['b_marker_final'] = marker.exists()
            emit(root / 'results.json', result)
        # A progressing shared-home baseline is a counterexample, not a forced failure.
        by = {p['arm']: p for p in result['phases']}
        shared_phase = by['shared']
        reproduced = (
            shared_phase['b_exit_while_a_gated'] is None
            and shared_phase['b_log_package_cache_wait']
            and not shared_phase['marker_while_a_gated']
            and shared_phase['a_alive_while_gated'])
        result['genuine_download_hol_reproduced'] = reproduced
        def completed_during_download(arm):
            p = by[arm]
            return (p['b_exit_while_a_gated'] == 0 and p['a_alive_while_gated']
                    and p['marker_while_a_gated'] and bool(p['rmeta_while_gated']))

        result['checks'] = {
            'all_actual_cargo_runs_succeeded': all(
                p['a_exit'] == 0 and p['b_exit_after_release'] == 0
                for p in result['phases']),
            'b_lockfile_unchanged': sha(bproject / 'Cargo.lock') == lock_before,
            'ordinary_private_completed_during_real_download':
                completed_during_download('ordinary-private'),
            'heli_epoch_completed_during_real_download':
                completed_during_download('heli-epoch'),
        }
        if all(result['checks'].values()):
            result['status'] = 'PASS' if reproduced else 'NOT_REPRODUCED'
        else:
            result['status'] = 'FAIL'
    except Exception as e:
        result['status'] = 'FAIL'
        result['error'] = repr(e)
    finally:
        for _, release in gates.values():
            release.set()
        for p in reversed(owned):
            kill_owned(p)
        if endpoint_directory is not None:
            endpoint = endpoint_directory / 'h.sock'
            try:
                if endpoint.exists() and stat.S_ISSOCK(endpoint.lstat().st_mode):
                    endpoint.unlink()
                # Never recurse or delete unexpected contents, even in our dir.
                endpoint_directory.rmdir()
            except OSError as error:
                result['endpoint_cleanup_error'] = str(error)
        if server:
            server.shutdown()
            server.server_close()
        for stream in logs:
            stream.close()
        emit(root / 'http-events.json', events)
        emit(root / 'results.json', result)
    print(json.dumps({
        'status': result['status'], 'error': result.get('error'),
        'checks': result.get('checks'),
        'genuine_download_hol_reproduced': result.get('genuine_download_hol_reproduced'),
        'evidence': str(root / 'results.json'),
    }, indent=2))
    return 0 if result['status'] in ('PASS', 'NOT_REPRODUCED') else 1
if __name__ == '__main__':
    raise SystemExit(main())

