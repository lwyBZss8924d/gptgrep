"""Exercise the standalone runtime stager using an explicitly owned output root.

The race subprocess executes the exact embedded Python bytes while interposing
one os.open call at the destination-create boundary. No product test hook exists.
The historical shell probe inserts a symlink in its cp subprocess after preflight.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

SEED = b'caller-owned unrelated content; preserve exactly\n'


def sha(p):
    return hashlib.sha256(Path(p).read_bytes()).hexdigest()


def write_json(path, value):
    Path(path).write_text(json.dumps(value, indent=2, sort_keys=True) + '\n')


def race_child(args):
    script = Path(args.script)
    job = Path(args.output)
    mode = args.race_child
    outside = job.parent / (job.name + '-protected')
    outside.mkdir()
    sentinel = outside / 'sentinel'
    sentinel.write_bytes(SEED)
    protected = [sentinel]
    if mode == 'parent_symlink':
        for name in ['gptgrep', 'gptgrep-document', 'gptgrep-ocr', 'gptgrep-retrieval']:
            p = outside / 'skills' / name / 'SKILL.md'
            p.parent.mkdir(parents=True)
            p.write_bytes(SEED)
            protected.append(p)
    before = {str(p): sha(p) for p in protected}
    original_open = os.open
    injected = []

    def raced_open(path, flags, mode_bits=0o777, *, dir_fd=None):
        selected = (path == 'SKILL.md') if mode == 'parent_symlink' else (path == 'AGENTS.md')
        if not injected and selected and flags & os.O_CREAT:
            require_absent = job / 'AGENTS.md'
            if mode == 'leaf_regular':
                require_absent.write_bytes(SEED)
                protected.append(require_absent)
                before[str(require_absent)] = sha(require_absent)
            elif mode == 'leaf_symlink':
                require_absent.symlink_to(sentinel)
            else:
                (job / '.agents').rename(job / '.agents-detached')
                (job / '.agents').symlink_to(outside, target_is_directory=True)
            injected.append({'operation': 'open', 'leaf': path, 'flags': flags, 'dir_fd_present': dir_fd is not None, 'after_preflight': True})
        return original_open(path, flags, mode_bits, dir_fd=dir_fd)

    os.open = raced_open
    os.supports_dir_fd.add(raced_open)
    data = script.read_text()
    prefix = "exec python3 -I -B - \"$@\" <<'PY'\n"
    if data.count(prefix) != 1 or not data.endswith('\nPY\n'):
        raise RuntimeError('unexpected embedded Python boundary')
    body = data.split(prefix, 1)[1][:-3]
    sys.argv = [str(script), args.package, str(job)]
    exit_code = 0
    try:
        exec(compile(body, str(script) + ':embedded-python', 'exec'), {'__name__': '__main__'})
    except SystemExit as exc:
        exit_code = exc.code
    after = {str(p): sha(p) for p in protected}
    result = {'scenario': mode, 'injection': injected, 'stager_exit_code': exit_code, 'before': before, 'after': after, 'protected_bytes_unchanged': before == after, 'embedded_source_sha256': hashlib.sha256(body.encode()).hexdigest()}
    write_json(job.parent / (job.name + '.json'), result)
    if len(injected) != 1 or exit_code == 0 or before != after:
        raise RuntimeError('stager failed race-preservation assertion')


def parent(args):
    root = Path(args.output)
    root.mkdir(exist_ok=False)
    script = Path(args.script).resolve(strict=True)
    package = Path(args.package).resolve(strict=True)
    manifest = json.loads((package / 'runtime/codex/manifest.json').read_bytes())
    shim = root / 'bin'
    shim.mkdir()
    (shim / 'python3').symlink_to(Path(sys.executable).resolve(strict=True))
    env = os.environ.copy()
    env['PATH'] = str(shim) + ':' + str(Path(sys.executable).parent) + ':/usr/bin:/bin:/usr/sbin:/sbin'
    calls = []

    def run(name, command, expected):
        proc = subprocess.run([str(x) for x in command], cwd=root, env=env, stdin=subprocess.DEVNULL, capture_output=True, timeout=25)
        if len(proc.stdout) + len(proc.stderr) > 1048576:
            raise RuntimeError('fixture output limit exceeded')
        value = {'name': name, 'argv': [str(x) for x in command], 'exit_code': proc.returncode, 'stdout': proc.stdout.decode('utf-8', 'replace'), 'stderr': proc.stderr.decode('utf-8', 'replace')}
        write_json(root / (name + '.command.json'), value)
        calls.append(value)
        if (proc.returncode == 0) != (expected == 0):
            raise RuntimeError('unexpected exit: ' + name)
        return value

    fresh = root / 'fresh cwd with spaces'
    fresh.mkdir()
    run('fresh', ['/bin/sh', script, package, fresh], 0)
    expected_files = sorted(row['target_path'] for row in manifest['files'])
    actual_files = sorted(str(p.relative_to(fresh)) for p in fresh.rglob('*') if p.is_file())
    assert actual_files == expected_files
    for row in manifest['files']:
        assert sha(fresh / row['target_path']) == row['sha256']
    existing = root / 'existing'
    existing.mkdir()
    (existing / 'AGENTS.md').write_bytes(SEED)
    run('existing', ['/bin/sh', script, package, existing], 2)
    assert (existing / 'AGENTS.md').read_bytes() == SEED
    assert sorted(p.name for p in existing.iterdir()) == ['AGENTS.md']
    for mode in ['leaf_regular', 'leaf_symlink', 'parent_symlink']:
        job = root / mode
        job.mkdir()
        run(mode, [sys.executable, '-I', '-B', Path(__file__).resolve(), '--script', script, '--package', package, '--output', job, '--race-child', mode], 0)

    historical = None
    if args.historical_script:
        job = root / 'historical-r2'
        job.mkdir()
        sentinel = root / 'historical-r2-protected'
        sentinel.write_bytes(SEED)
        wrapper = shim / 'cp'
        wrapper.write_text('#!/bin/sh\nset -eu\nif [ "$2" = "$GPTGREP_STAGE_RACE_JOB/AGENTS.md" ]; then\n  /bin/ln -s "$GPTGREP_STAGE_RACE_SENTINEL" "$2"\nfi\nexec /bin/cp "$@"\n')
        wrapper.chmod(0o700)
        env.update({'GPTGREP_STAGE_RACE_JOB': str(job), 'GPTGREP_STAGE_RACE_SENTINEL': str(sentinel)})
        run('historical-r2', ['/bin/sh', Path(args.historical_script), package, job], 0)
        historical = {'script': str(args.historical_script), 'script_sha256': sha(args.historical_script), 'successful_exit_with_raced_symlink': (job / 'AGENTS.md').is_symlink(), 'unrelated_fixture_bytes_overwritten': sentinel.read_bytes() != SEED, 'sentinel_matches_asset': sha(sentinel) == sha(package / 'runtime/codex/AGENTS.md')}
        assert all(historical[k] for k in ['successful_exit_with_raced_symlink', 'unrelated_fixture_bytes_overwritten', 'sentinel_matches_asset'])
    races = [json.loads((root / (name + '.json')).read_bytes()) for name in ['leaf_regular', 'leaf_symlink', 'parent_symlink']]
    result = {'schema': 'gptgrep.runtime-stage-regression.v1', 'status': 'passed', 'stage_script': {'path': str(script), 'bytes': script.stat().st_size, 'sha256': sha(script)}, 'manifest_sha256': sha(package / 'runtime/codex/manifest.json'), 'fresh_staged_exact_eight': True, 'preexisting_file_refused_without_changes': True, 'race_cases': races, 'historical_r2_reproduction': historical, 'fixture_limit': 'Deterministic OS-call interposition executes exact embedded source; not a scheduler-probability concurrency stress test.', 'command_count': len(calls)}
    write_json(root / 'result.json', result)
    print(json.dumps({'status': 'passed', 'result': str(root / 'result.json'), 'race_cases': len(races), 'historical_failure_reproduced': historical is not None}))


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--script', required=True)
    parser.add_argument('--package', required=True)
    parser.add_argument('--output', required=True)
    parser.add_argument('--historical-script')
    parser.add_argument('--race-child', choices=['leaf_regular', 'leaf_symlink', 'parent_symlink'])
    args = parser.parse_args()
    if args.race_child:
        race_child(args)
    else:
        parent(args)
