"""Managed build/native ownership for the standalone inspector; no commands on import."""
import argparse
import fcntl
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time
import uuid

from . import forwarding, host, lifecycle, memory, processes

GIB = 1024 ** 3
PROTECTED = re.compile(r'(engine(?:-[A-Za-z0-9]+)?|model[-_]inspector(?:-.*)?|human[-_]goat.*|actor_idle_probe|SkyrimSE(?:\.exe)?)', re.I)
FAULT = re.compile(r'INSPECTOR_LOAD_FAILED|INSPECTOR_LOAD_TIMEOUT|(?i:panicked at|validation error|device.?lost)|\bERROR\b')


def need(condition, message):
    if not condition:
        raise RuntimeError(message)


def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def query(command):
    return subprocess.check_output(command, text=True, timeout=10)


def save(path, value):
    temporary = path.with_suffix(path.suffix + '.partial')
    temporary.write_text(json.dumps(value, indent=2) + '\n')
    temporary.replace(path)


def options(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--assets', type=Path, required=True)
    parser.add_argument('--model', action='append', default=[])
    parser.add_argument('--record', action='append', default=[], help='hexadecimal base FormID')
    parser.add_argument('--catalog', type=Path)
    parser.add_argument('--build-only', action='store_true', help='build without opening a window')
    parser.add_argument('--needed-gib', type=float, default=12, help='managed build-growth estimate')
    parser.add_argument('--isolated', help='optional managed validation label')
    parser.add_argument('--inside', action='store_true', help=argparse.SUPPRESS)
    parser.add_argument('--run-dir', type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args(argv)
    need(1 <= len(args.model) + len(args.record) <= 8, 'provide one to eight --model/--record inputs')
    need(args.assets.is_absolute(), 'assets must be absolute')
    args.assets = args.assets.resolve(strict=True)
    need(args.assets.is_dir(), 'assets must be a directory')
    if args.catalog:
        need(args.catalog.is_absolute(), 'catalog must be absolute')
        args.catalog = args.catalog.resolve(strict=True)
        need(args.catalog.is_file(), 'catalog must be an existing file')
    return args


def load_workspace(repo):
    common = Path(query(['git', '-C', str(repo), 'rev-parse', '--path-format=absolute', '--git-common-dir']).strip())
    root = Path(os.environ.get('MUDCRAB_WORKSPACE_ROOT', common.parent / '.mudcrab-local')).resolve()
    path = repo / 'scripts/workspace.py'
    if not path.is_file():
        path = root / 'bin/workspace.py'
    need(path.is_file(), 'managed workspace.py is required; see docs/tools/model-inspector.md')
    spec = importlib.util.spec_from_file_location('_model_inspector_workspace', path)
    module = importlib.util.module_from_spec(spec)
    source = path.read_bytes()
    exec(compile(source, str(path), 'exec', dont_inherit=True), module.__dict__)
    module.inspector_source_sha256 = hashlib.sha256(source).hexdigest()
    return module, root, path


def held_build_leases(root):
    held = []
    for path in sorted((root / 'locks').glob('*.lock')):
        if not re.fullmatch(r'(?:slot-[01]|isolated-.+)\.lock', path.name):
            continue
        with path.open('rb') as stream:
            try:
                fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                held.append(path)
            else:
                fcntl.flock(stream, fcntl.LOCK_UN)
    return held


def quiet_native(root, own_cache):
    peers = [path for path in held_build_leases(root) if path.stem != own_cache.name]
    need(not peers, 'native quiet lane occupied; leave peer work untouched: ' + repr(peers))
    rows = processes.processes()
    need(not any(PROTECTED.fullmatch(Path(row['command']).name) for row in rows.values()),
         'another game/inspector is running; leave it untouched')
    # Existing local department reservations remain authoritative when present. The tool
    # neither creates this optional ledger nor clears anyone else's reservation.
    ledger = root / 'evidence/department-review-20261101/ledger.json'
    if ledger.is_file():
        policy = json.loads(ledger.read_text())['resourcePolicy']
        need(not policy.get('gpuOwner') and not policy.get('gpuGrant'), 'another native reservation exists')


def sample_host(initial_swap=None, profile='native', admission=False):
    sample = host.collect(query)
    result = host.enforce(sample, sample['swap'] if initial_swap is None else initial_swap,
                          profile, admission=admission)
    return dict(sample=sample, policy=result)


def monitor(command, repo, output, log_name, profile, root, reserve_gib, seconds):
    """Keep ownership through cleanup, including partial spawn and signal failures."""
    owned = lifecycle.OwnedChild(processes)
    child = None
    code = None
    failure = None
    ready = False
    sampler = memory.Darwin()
    pre = sample_host(profile=profile, admission=True)
    started = time.monotonic()
    owned.install()
    peak_rss = peak_foot = 0
    try:
        with (output / log_name).open('xb') as log, (output / (log_name + '.samples.jsonl')).open('x') as samples:
            child = owned.spawn(command, cwd=repo, stdout=log, stderr=subprocess.STDOUT,
                                start_new_session=True)
            while True:
                rows, family = owned.observe()
                rss = sum(rows[pid]['rss_kib'] * 1024 for pid in family)
                footprint = 0
                for pid in family:
                    try:
                        footprint += sampler.memory(pid)['footprint']
                    except memory.Refused:
                        newer = processes.processes()
                        need(pid not in newer or newer[pid]['birth'] != rows[pid]['birth'],
                             'cannot sample a live owned process')
                peak_rss, peak_foot = max(peak_rss, rss), max(peak_foot, footprint)
                elapsed = time.monotonic() - started
                current = sample_host(pre['sample']['swap'], profile)
                need(rss < (4 if profile == 'native' else 8) * GIB, 'owned RSS limit')
                need(footprint < (6 if profile == 'native' else 8) * GIB, 'owned footprint limit')
                need(shutil.disk_usage(root).free >= reserve_gib * GIB, 'disk reserve violated')
                need(elapsed < seconds, 'owned command deadline')
                need((output / log_name).stat().st_size < 32 * 1024 ** 2, 'owned log limit')
                if profile == 'native':
                    text = (output / log_name).read_text(errors='replace')
                    need(not FAULT.search(text), 'native reported a load/render failure')
                    if not ready and 'INSPECTOR_READY' in text:
                        ready = True
                        save(output / 'ready.json', dict(pid=child.pid, elapsed=elapsed))
                        print('INSPECTOR_READY ' + str(output), flush=True)
                    need(ready or elapsed < 100, 'inspector did not become ready')
                samples.write(json.dumps(dict(elapsed=elapsed, family=sorted(family), rss=rss,
                                              footprint=footprint, host=current)) + '\n')
                samples.flush()
                code = child.poll()
                if code is not None:
                    break
                time.sleep(2)
            need(code == 0 and (profile != 'native' or ready), 'owned command failed')
    except BaseException as error:
        failure = str(error) or type(error).__name__
        raise
    finally:
        try:
            owned.cleanup()
        except BaseException as error:
            owned.hold(lambda: save(output / 'cleanup-held.json', dict(reason=str(error))))
        owned.restore()
        _, remaining = owned.observe()
        receipt = dict(exit_code=code, failure=failure, remaining_owned=sorted(remaining),
                       known=owned.known, peak_rss=peak_rss, peak_footprint=peak_foot)
        save(output / (log_name + '.release.json'), receipt)
        try:
            receipt['host_postflight'] = sample_host(pre['sample']['swap'], profile)
        except BaseException as error:
            receipt['postflight_failure'] = str(error) or type(error).__name__
            save(output / (log_name + '.release.json'), receipt)
            if failure is None:
                raise
        else:
            save(output / (log_name + '.release.json'), receipt)
    return code


def run_inside(args, repo, root, workspace):
    cache = Path(os.environ.get('MUDCRAB_BUILD_LEASE', ''))
    need(cache.is_absolute() and cache.parent == root / 'builds', 'managed foreground lease required')
    need(Path(os.environ.get('CARGO_TARGET_DIR', '')) == cache / 'target', 'wrapper target mismatch')
    marker = json.loads((cache / 'build.json').read_text())
    need(marker['checkout'] == str(repo), 'wrapper checkout mismatch')
    output = args.run_dir.resolve(strict=True)
    need(output.parent == root / 'evidence/model-inspector', 'output must be a fresh managed run')
    row = processes.processes()[os.getpid()]
    save(output / 'worker.json', dict(pid=os.getpid(), birth=row['birth']))
    need(not (output / 'stop-request.json').exists(), 'stopped before worker startup')
    reserve = float(os.environ.get('MUDCRAB_RESERVE_GIB', workspace.RESERVE_GIB))
    command = ['cargo', 'build', '--locked', '--profile', 'quick', '-p', 'engine',
               '--example', 'model_inspector', '-j2', '--message-format=json']
    monitor(command, repo, output, 'build.log', 'cpu', root, reserve, 3600)
    binaries = []
    for line in (output / 'build.log').read_text().splitlines():
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if value.get('reason') == 'compiler-artifact' and value.get('target', {}).get('name') == 'model_inspector' and value.get('executable'):
            binaries.append(Path(value['executable']))
    need(len(set(binaries)) == 1, 'one inspector binary required')
    binary = binaries[-1].resolve(strict=True)
    need(binary.is_relative_to(cache / 'target'), 'artifact escaped the leased target')
    save(output / 'binary.json', dict(path=str(binary), sha256=digest(binary),
         head=query(['git', '-C', str(repo), 'rev-parse', 'HEAD']).strip(),
         tracked_diff_sha256=hashlib.sha256(subprocess.check_output(['git', '-C', str(repo), 'diff', '--binary', 'HEAD'])).hexdigest()))
    if args.build_only:
        return 0
    need(not (output / 'stop-request.json').exists(), 'stopped before native startup')
    with workspace.lease(root / 'locks/native-admission.lock'):
        quiet_native(root, cache)
        command = [str(binary), '--assets', str(args.assets), '--output', str(output / 'captures')]
        for model in args.model:
            command += ['--model', model]
        for record in args.record:
            command += ['--record', record]
        if args.catalog:
            command += ['--catalog', str(args.catalog)]
        # Remove unrelated engine/adapter overrides only for the native child.
        env_before = os.environ.copy()
        try:
            for key in list(os.environ):
                if key.startswith(('MUDCRAB_', 'WGPU_')):
                    del os.environ[key]
            os.environ['WGPU_BACKEND'] = 'metal'
            return monitor(command, repo, output, 'native.log', 'native', root, reserve, 1830)
        finally:
            os.environ.clear()
            os.environ.update(env_before)


def create_output(root):
    """Create a unique run beneath the canonical managed evidence directory."""
    evidence = root / 'evidence'
    need(evidence.is_dir() and not evidence.is_symlink(), 'managed evidence root missing or aliased')
    parent = evidence / 'model-inspector'
    need(not parent.is_symlink(), 'inspector evidence parent cannot be a symlink')
    parent.mkdir(exist_ok=True)
    need(parent.resolve(strict=True) == parent, 'inspector evidence parent escaped managed storage')
    output = parent / ('run-' + time.strftime('%Y%m%dT%H%M%S') + '-' + uuid.uuid4().hex[:8])
    output.mkdir()
    return output


def main(argv=None):
    args = options(argv)
    need(sys.platform == 'darwin', 'this launcher currently supports macOS/Metal only')
    repo = Path(__file__).resolve().parents[2]
    workspace, root, tool = load_workspace(repo)
    if args.inside:
        return run_inside(args, repo, root, workspace)
    workspace.Workspace(repo).setup()
    need(len(held_build_leases(root)) < 2, 'two managed commands are already active')
    if not args.build_only:
        # Do not build an interactive package while another native session is protected.
        with workspace.lease(root / 'locks/native-admission.lock'):
            quiet_native(root, Path('/no-owned-cache'))
    sample_host(profile='cpu', admission=True)
    output = create_output(root)
    save(output / 'launch.json', dict(workspace_tool=str(tool), workspace_sha256=workspace.inspector_source_sha256,
         assets=str(args.assets), models=args.model, records=args.record,
         scope='standalone model inspection; no world or feature acceptance'))
    command = [sys.executable, '-B', str(repo / 'scripts/inspect-model.py'), '--inside',
               '--run-dir', str(output), '--assets', str(args.assets)]
    for model in args.model:
        command += ['--model', model]
    for record in args.record:
        command += ['--record', record]
    if args.catalog:
        command += ['--catalog', str(args.catalog)]
    if args.build_only:
        command += ['--build-only']
    class CapacityBound(workspace.Workspace):
        def preflight(self, needed_gib=8, output=None):
            # Workspace.run_task calls this while holding allocation.lock. Keep that
            # implementation unchanged and make the global two-command check atomic.
            need(len(held_build_leases(self.root)) < 2, 'two managed commands are active')
            return super().preflight(needed_gib, output)
    with forwarding.ForwardSignals(output):
        code = CapacityBound(repo).run_task(command, args.isolated, args.needed_gib)
    save(output / 'wrapper-release.json', dict(exit_code=code, foreground_returned=True,
         own_cache_lease_released=True))
    print('Inspector evidence: ' + str(output), flush=True)
    return code
