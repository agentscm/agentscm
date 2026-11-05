#!/usr/bin/env bash
# Synthetic orchestration checks; no Docker daemon, network or Rust build required.
set -euo pipefail
cd "$(dirname "$0")/.."
python3 - <<'PY'
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import time

script = Path('.github/hub-e2e.sh').resolve()
with tempfile.TemporaryDirectory(prefix='hub-e2e-test-') as temporary:
    root = Path(temporary)
    repo = root / 'repo'
    repo.mkdir()
    (repo / '.github').mkdir()
    shutil.copy(script, repo / '.github/hub-e2e.sh')
    for name in ['compose.hub.yaml', '.github/compose.e2e.yaml', 'Dockerfile']:
        (repo / name).write_text('# synthetic fixture\n')
    def git(*args):
        return subprocess.check_output(['git', '-C', str(repo), *args], text=True).strip()
    git('init', '-q')
    git('add', '.')
    git('-c', 'user.name=Synthetic', '-c', 'user.email=synthetic@example.invalid',
        '-c', 'commit.gpgsign=false', 'commit', '-qm', 'synthetic fixture')
    binaries = root / 'bin'
    binaries.mkdir()
    docker = binaries / 'docker'
    docker.write_text('''#!/usr/bin/env python3
import json, os, pathlib, sys, time
root = pathlib.Path(__file__).resolve().parent.parent
args = sys.argv[1:]
mode = (root / 'mode').read_text()
with (root / 'calls').open('a') as f: f.write(json.dumps(args) + '\\n')
if args[0] == 'build':
    if mode == 'interrupt':
        (root / 'waiting').touch()
        time.sleep(30)
    if mode == 'build-fail': sys.exit(1)
    if mode == 'source-drift': (root / 'repo' / 'drift').touch()
    role = args[args.index('--target') + 1]
    if mode != 'missing-id':
        pathlib.Path(args[args.index('--iidfile') + 1]).write_text('sha256:' + ('a' if role == 'runtime' else 'b') * 64)
elif args[:2] == ['image', 'inspect']:
    if args[3] == '{{.Id}}': print(args[4])
    elif mode != 'missing-platform': print('linux/arm64')
elif args[0] == 'compose':
    profile = args[args.index('--profile') + 1]
    if mode != 'legacy':
        assert '--env-file' in args and args[args.index('--env-file') + 1] == '/dev/null'
        assert not any(k.startswith('AGENTSCM_GITHUB') or k.startswith('AGENTSCM_GITLAB') for k in os.environ)
        if 'build' in args: raise AssertionError('profile rebuilt image')
    if 'up' in args and mode == profile + '-fail': sys.exit(1)
    if 'down' in args and mode == profile + '-cleanup-fail': sys.exit(1)
    if 'port' in args:
        portfile = root / 'port'
        port = int(portfile.read_text()) + 1 if portfile.exists() else 49999
        portfile.write_text(str(port))
        print('127.0.0.1:' + str(port))
    if 'run' in args and 'AGENTSCM_MACHINE=laptop-b' in args:
        if mode != 'incomplete':
            print(''' + repr('alice\'s agent (claude-a) on laptop-a is already working on src/billing.rs\nFix invoice rounding\n{"hookEventName":"PreToolUse"}') + ''')
''')
    docker.chmod(0o755)
    curl = binaries / 'curl'
    curl.write_text('''#!/usr/bin/env python3
import pathlib, sys
root = pathlib.Path(__file__).resolve().parent.parent
port = '7332' if (root / 'mode').read_text() == 'legacy' else (root / 'port').read_text()
assert 'http://127.0.0.1:' + port + '/v1/hub/health' in sys.argv
print('{"status":"ok"}')
''')
    curl.chmod(0o755)
    environment = dict(os.environ, PATH=str(binaries) + os.pathsep + os.environ['PATH'],
                       AGENTSCM_GITHUB_PAT='synthetic-must-not-reach-compose')
    def prepare(mode):
        (root / 'mode').write_text(mode)
        (root / 'calls').write_text('')
        return root / ('evidence-' + mode)
    def run(mode, expected):
        evidence = prepare(mode)
        result = subprocess.run(['bash', str(repo / '.github/hub-e2e.sh'), 'local-release', str(evidence)],
                                env=environment, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        assert (result.returncode == 0) == expected, (mode, result.stdout)
        calls = [json.loads(line) for line in (root / 'calls').read_text().splitlines()]
        return evidence, calls
    for profile in ['default', 'postgres']:
        prepare('legacy')
        legacy = subprocess.run(['bash', str(repo / '.github/hub-e2e.sh'), profile],
                                env=environment, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        assert legacy.returncode == 0, legacy.stdout
        calls = [json.loads(line) for line in (root / 'calls').read_text().splitlines()]
        assert len([c for c in calls if c[0] == 'compose' and 'build' in c]) == 1
        assert not any(c[0] == 'compose' and 'port' in c for c in calls)
    evidence, calls = run('success', True)
    assert (evidence / 'result').read_text() == 'pass\n'
    assert (evidence / 'source').read_text() == f'qualification=locally-built-only\ncommit={git("rev-parse", "HEAD")}\ntree={git("rev-parse", "HEAD^{tree}")}\n'
    assert len([c for c in calls if c[0] == 'build']) == 2
    for stage in ['build', 'default', 'postgres']:
        assert (evidence / (stage + '.status')).read_text() == 'pass\n'
        assert (evidence / (stage + '.log')).is_file()
    compose = (evidence / 'compose.yaml').read_text()
    assert compose.count('sha256:' + 'a' * 64) == 4
    assert compose.count('sha256:' + 'b' * 64) == 2
    profiles = [c[c.index('--profile') + 1] for c in calls if c[0] == 'compose']
    assert profiles == sorted(profiles)  # default completes before postgres starts
    assert len([c for c in calls if c[0] == 'compose' and 'restart' in c]) == 2
    assert len([c for c in calls if c[0] == 'volume' and c[1] == 'rm']) == 1
    for mode in ['build-fail', 'missing-id', 'missing-platform']:
        evidence, calls = run(mode, False)
        assert (evidence / 'build.status').read_text() == 'fail\n'
        assert (evidence / 'result').read_text() != 'pass\n'
        assert not any(c[0] == 'compose' for c in calls)
    for mode in ['default-fail', 'postgres-fail', 'default-cleanup-fail', 'incomplete']:
        evidence, calls = run(mode, False)
        assert (evidence / 'result').read_text() == 'fail\n'
        assert any(c[0] == 'compose' and c[c.index('--profile') + 1] == 'postgres' for c in calls)
    (repo / 'dirty').write_text('synthetic change\n')
    evidence, calls = run('dirty', False)
    assert not calls and not evidence.exists()
    (repo / 'dirty').unlink()
    evidence, calls = run('source-drift', False)
    assert (evidence / 'result').read_text() == 'fail\n'
    (repo / 'drift').unlink()
    # Existing evidence must remain untouched, even when it records an older pass.
    previous = (root / 'evidence-success' / 'result').read_text()
    evidence, calls = run('success', False)
    assert not calls and (evidence / 'result').read_text() == previous
    evidence = prepare('interrupt')
    process = subprocess.Popen(['bash', str(repo / '.github/hub-e2e.sh'), 'local-release', str(evidence)],
                               env=environment, start_new_session=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 10
        while not (root / 'waiting').exists() and time.monotonic() < deadline:
            time.sleep(.02)
        assert (root / 'waiting').exists()
        os.killpg(process.pid, signal.SIGTERM)
        assert process.wait(timeout=10) != 0
        assert (evidence / 'result').read_text() == 'incomplete\n'
        assert (evidence / 'build.status').read_text() == 'running\n'
    finally:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
    print('PASS synthetic Hub release orchestration (14 scenarios)')
PY
