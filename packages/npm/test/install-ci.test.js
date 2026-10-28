'use strict';
// Exercise the real wrapper and installer in child processes, without a release or gh credentials.
const test = require('node:test');
const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const target = {
  'linux/x64': 'x86_64-unknown-linux-musl',
  'linux/arm64': 'aarch64-unknown-linux-musl',
  'darwin/x64': 'x86_64-apple-darwin',
  'darwin/arm64': 'aarch64-apple-darwin',
}[`${process.platform}/${process.arch}`];
const skip = !target && 'reconcile full-process fixtures require a supported POSIX host';
const version = '0.0.0-ci-test';
const asset = `agentscm-${target}`;
const releaseUrl = `https://github.com/agentscm/agentscm/releases/download/v${version}`;
const reconcileArgs = [
  'local-validation', 'reconcile', '--github-repo', 'example/project', '--base', 'main',
  '--repo', 'checkout with spaces', '--verifier-workflow', '.github/workflows/local-validation.yml',
  '--target-url', 'https://github.com/example/project/actions/runs/123',
];
const digest = (bytes) => crypto.createHash('sha256').update(bytes).digest('hex');

const binarySource = `#!/usr/bin/env node
'use strict';
const fs = require('node:fs');
const event = {
  kind: 'launch', args: process.argv.slice(2),
  telemetry: process.env.AGENTSCM_TELEMETRY,
  doNotTrack: process.env.DO_NOT_TRACK,
  verified: process.env.AGENTSCM_VERIFIED_INSTALL,
  stdin: fs.readFileSync(0, 'utf8'),
  tty: [process.stdin, process.stdout, process.stderr].map((stream) => Boolean(stream.isTTY)),
};
fs.appendFileSync(process.env.FIXTURE_EVENTS, JSON.stringify(event) + '\\n');
console.log('synthetic reconcile stdout');
console.error('synthetic reconcile stderr');
process.exit(Number(process.env.FIXTURE_EXIT || 0));
`;

const ghSource = `#!/usr/bin/env node
'use strict';
const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const fs = require('node:fs');
const config = JSON.parse(fs.readFileSync(process.env.FIXTURE_CONFIG, 'utf8'));
const args = process.argv.slice(2);
const record = (event) => fs.appendFileSync(process.env.FIXTURE_EVENTS, JSON.stringify(event) + '\\n');
if (args[0] === '--version') {
  assert.deepEqual(args, ['--version']);
  record({ kind: 'gh-version' });
} else if (args[0] === 'auth') {
  assert.deepEqual(args, ['auth', 'status', '--hostname', 'github.com']);
  record({ kind: 'gh-auth' });
  process.exit(config.signedOut ? 1 : 0);
} else {
  assert.equal(process.env.GH_HOST, 'github.com');
  assert.deepEqual(args, [
    'attestation', 'verify', args[2], '--repo', config.repository,
    '--signer-workflow', config.repository + '/.github/workflows/release.yml',
    '--source-ref', 'refs/tags/v' + config.version, '--deny-self-hosted-runners',
  ]);
  const hash = crypto.createHash('sha256').update(fs.readFileSync(args[2])).digest('hex');
  record({ kind: 'verify', args, hash });
  if (config.provenanceMismatch || config.attestedRepository !== config.repository ||
      config.attestedWorkflow !== config.repository + '/.github/workflows/release.yml' ||
      config.attestedRef !== 'refs/tags/v' + config.version) {
    console.error('synthetic attestation mismatch');
    process.exit(1);
  }
}
`;

// Loaded only into the wrapper child via --require. Every fetch is intercepted;
// unknown URLs fail instead of falling through to the real network.
const preloadSource = `
'use strict';
const fs = require('node:fs');
const { Readable } = require('node:stream');
const config = JSON.parse(fs.readFileSync(process.env.FIXTURE_CONFIG, 'utf8'));
require('node:net').Socket.prototype.connect = function () {
  throw new Error('network is disabled in installer CI fixtures');
};
globalThis.fetch = async (url) => {
  fs.appendFileSync(process.env.FIXTURE_EVENTS, JSON.stringify({ kind: 'fetch', url }) + '\\n');
  const file = config.downloads[url];
  if (!file) throw new Error('unexpected fixture URL: ' + url);
  const unavailable = config.unavailable === url;
  return {
    url, ok: !unavailable, status: unavailable ? 404 : 200,
    body: Readable.from(unavailable ? [] : [fs.readFileSync(file)]),
  };
};
`;

function fixture(t, options = {}) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'agentscm-install-ci-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const home = path.join(root, 'home');
  const pkg = path.join(root, 'package');
  const bin = path.join(root, 'tools');
  const installDir = path.join(home, '.agentscm', 'bin');
  const binary = path.join(installDir, `agentscm-${version}`);
  const latest = path.join(installDir, 'agentscm');
  const eventsFile = path.join(root, 'events.jsonl');
  const configFile = path.join(root, 'config.json');
  const preload = path.join(root, 'preload.cjs');
  for (const dir of [home, bin, path.join(pkg, 'bin'), path.join(pkg, 'lib')]) {
    fs.mkdirSync(dir, { recursive: true });
  }
  for (const file of ['lib/install.js', 'bin/agentscm.js']) {
    fs.copyFileSync(path.join(__dirname, '..', file), path.join(pkg, file));
  }
  const repository = options.repository ?? 'agentscm/agentscm';
  const fixtureReleaseUrl = `https://github.com/${repository}/releases/download/v${version}`;
  const wrapper = path.join(pkg, 'lib/install.js');
  fs.writeFileSync(wrapper, fs.readFileSync(wrapper, 'utf8').replace(/^const REPO = '[^']+';$/m, `const REPO = '${repository}';`));
  fs.writeFileSync(path.join(pkg, 'package.json'), JSON.stringify({ version }));
  fs.symlinkSync(process.execPath, path.join(bin, 'node'));
  if (!options.missingGh) fs.writeFileSync(path.join(bin, 'gh'), ghSource, { mode: 0o755 });
  fs.writeFileSync(preload, preloadSource);
  const releaseBinary = path.join(root, 'release-binary');
  const releaseSums = path.join(root, 'release-sums');
  const sums = `${digest(binarySource)}  ${asset}\n`;
  fs.writeFileSync(releaseBinary, options.corruptDownload ? `${binarySource}\n// altered\n` : binarySource);
  fs.writeFileSync(releaseSums, sums);
  if (!options.downloadSums) fs.writeFileSync(path.join(pkg, 'SHA256SUMS'), options.sums ?? sums);
  const config = {
    version, repository, attestedRepository: options.attestedRepository ?? repository,
    attestedWorkflow: options.attestedWorkflow ?? `${repository}/.github/workflows/release.yml`,
    attestedRef: options.attestedRef ?? `refs/tags/v${version}`, signedOut: Boolean(options.signedOut), provenanceMismatch: Boolean(options.provenanceMismatch),
    unavailable: options.unavailable && `${fixtureReleaseUrl}/${options.unavailable}`,
    downloads: { [`${fixtureReleaseUrl}/${asset}`]: releaseBinary, [`${fixtureReleaseUrl}/SHA256SUMS`]: releaseSums },
  };
  const saveConfig = () => fs.writeFileSync(configFile, JSON.stringify(config));
  saveConfig();
  return {
    root, home, pkg, binary, latest, installDir, config, saveConfig,
    run(args = reconcileArgs, extraEnv = {}) {
      fs.writeFileSync(eventsFile, '');
      const result = spawnSync(process.execPath, ['--require', preload, path.join(pkg, 'bin', 'agentscm.js'), ...args], {
        cwd: root, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'], timeout: 10_000,
        // Never inherit ambient credentials, NODE_OPTIONS, configuration, or executable search paths.
        env: {
          HOME: home, XDG_CONFIG_HOME: path.join(home, '.config'), GH_CONFIG_DIR: path.join(home, '.gh'),
          TMPDIR: root, PATH: bin, GH_HOST: 'untrusted.invalid', CI: 'true', AGENTSCM_REQUIRE_PROVENANCE: '1',
          AGENTSCM_TELEMETRY: 'off', DO_NOT_TRACK: '1', FIXTURE_CONFIG: configFile,
          FIXTURE_EVENTS: eventsFile, ...extraEnv,
        },
      });
      assert.ifError(result.error);
      assert.equal(result.signal, null, result.stderr);
      const events = fs.readFileSync(eventsFile, 'utf8').trim().split('\n').filter(Boolean).map((line) => JSON.parse(line));
      return { ...result, events };
    },
  };
}

function assertRejected(f, result, diagnostic, { cached = false } = {}) {
  assert.equal(result.status, 1, result.stderr);
  assert.match(result.stderr, diagnostic);
  assert.equal(result.events.some((event) => event.kind === 'launch'), false);
  assert.doesNotMatch(result.stdout + result.stderr, /synthetic reconcile/);
  if (!cached) {
    assert.equal(fs.existsSync(f.binary), false);
    assert.equal(fs.lstatSync(f.latest, { throwIfNoEntry: false }), undefined);
  }
  if (fs.existsSync(f.installDir)) {
    assert.equal(fs.readdirSync(f.installDir).some((name) => name.startsWith('.agentscm-')), false);
  }
}

test('CI reconcile forwards literal arguments without quickstart or a TTY and preserves telemetry off', { skip }, (t) => {
  const f = fixture(t);
  // The launcher must preserve even opaque arguments; the synthetic binary does not parse them.
  const args = [...reconcileArgs, '--config-dir', 'config with spaces', '--',
    '$(touch injected-dollar)', '`touch injected-backtick`', '; touch injected-semicolon',
    '$HOME', '*?[abc]', 'a|b > c & d', '"double" and \'single\'', '', 'line one\nline two'];
  const r = f.run(args);
  assert.equal(r.status, 0, r.stderr);
  assert.deepEqual(r.events.map((event) => event.kind), ['gh-version', 'gh-auth', 'fetch', 'verify', 'launch']);
  assert.equal(r.events[2].url, `${releaseUrl}/${asset}`);
  assert.equal(r.events[3].hash, digest(binarySource));
  assert.deepEqual(r.events[4], {
    kind: 'launch', args, telemetry: 'off', doNotTrack: '1', verified: '1', stdin: '', tty: [false, false, false],
  });
  assert.match(r.stdout, /synthetic reconcile stdout/);
  assert.match(r.stderr, /synthetic reconcile stderr/);
  for (const file of ['injected-dollar', 'injected-backtick', 'injected-semicolon']) {
    assert.equal(fs.existsSync(path.join(f.root, file)), false);
  }
  assert.equal(fs.readlinkSync(f.latest), path.basename(f.binary));
});

test('CI reconcile verifies a downloaded checksum manifest before launching', { skip }, (t) => {
  const f = fixture(t, { downloadSums: true });
  const r = f.run();
  assert.equal(r.status, 0, r.stderr);
  assert.deepEqual(r.events.map((event) => event.kind), ['gh-version', 'gh-auth', 'fetch', 'fetch', 'verify', 'launch']);
  assert.deepEqual(r.events.filter((event) => event.kind === 'fetch').map((event) => event.url),
    [`${releaseUrl}/SHA256SUMS`, `${releaseUrl}/${asset}`]);
});

test('CI reconcile rejects a bad checksum before provenance or binary execution', { skip }, (t) => {
  const f = fixture(t, { corruptDownload: true });
  const r = f.run();
  assertRejected(f, r, /checksum mismatch/);
  assert.deepEqual(r.events.map((event) => event.kind), ['gh-version', 'gh-auth', 'fetch']);
});

test('CI reconcile rejects a malformed packaged checksum manifest before download', { skip }, (t) => {
  const f = fixture(t, { sums: 'malformed\n' });
  const r = f.run();
  assertRejected(f, r, /SHA256SUMS has a malformed line/);
  assert.deepEqual(r.events.map((event) => event.kind), ['gh-version', 'gh-auth']);
});

for (const [name, options, diagnostic, kinds] of [
  ['missing gh', { missingGh: true }, /AGENTSCM_REQUIRE_PROVENANCE=1 but .*not installed/, []],
  ['signed-out gh', { signedOut: true }, /AGENTSCM_REQUIRE_PROVENANCE=1 but .*not signed in/, ['gh-version', 'gh-auth']],
  ['provenance mismatch', { provenanceMismatch: true }, /build provenance verification failed; not running the binary/, ['gh-version', 'gh-auth', 'fetch', 'verify']],
]) {
  test(`CI reconcile fails closed for ${name}`, { skip }, (t) => {
    const f = fixture(t, options);
    const r = f.run();
    assertRejected(f, r, diagnostic);
    assert.deepEqual(r.events.map((event) => event.kind), kinds);
    if (options.provenanceMismatch) assert.match(r.stderr, /synthetic attestation mismatch/);
  });
}

for (const unavailable of ['SHA256SUMS', asset]) {
  test(`CI reconcile does not launch when release ${unavailable} is unavailable`, { skip }, (t) => {
    const f = fixture(t, { downloadSums: true, unavailable });
    const r = f.run();
    assertRejected(f, r, /download failed \(HTTP 404\)/);
    assert.equal(r.events.at(-1).url, `${releaseUrl}/${unavailable}`);
    assert.equal(r.events.some((event) => event.kind === 'verify'), false);
  });
}

test('CI reconcile rechecks cached bytes and provenance on every invocation', { skip }, (t) => {
  const f = fixture(t);
  assert.equal(f.run().status, 0);
  const cached = f.run();
  assert.equal(cached.status, 0, cached.stderr);
  assert.deepEqual(cached.events.map((event) => event.kind), ['gh-version', 'gh-auth', 'verify', 'launch']);
  assert.equal(cached.events[2].args[2], f.binary);
  assert.equal(cached.events[2].hash, digest(binarySource));

  fs.appendFileSync(f.binary, '\n// altered cached bytes\n');
  const corrupt = f.run();
  assertRejected(f, corrupt, /does not match the release checksum/, { cached: true });
  assert.deepEqual(corrupt.events.map((event) => event.kind), ['gh-version', 'gh-auth']);

  fs.writeFileSync(f.binary, binarySource);
  f.config.provenanceMismatch = true;
  f.saveConfig();
  const revoked = f.run();
  assertRejected(f, revoked, /build provenance verification failed/, { cached: true });
  assert.deepEqual(revoked.events.map((event) => event.kind), ['gh-version', 'gh-auth', 'verify']);
});

test('CI reconcile propagates the launched binary exit code', { skip }, (t) => {
  const f = fixture(t);
  const r = f.run(reconcileArgs, { FIXTURE_EXIT: '37' });
  assert.equal(r.status, 37, r.stderr);
  assert.equal(r.events.at(-1).kind, 'launch');
  assert.match(r.stdout, /synthetic reconcile stdout/);
  assert.match(r.stderr, /synthetic reconcile stderr/);
});

for (const repository of ['counterbranch/agentscm', 'agentscm/agentscm']) {
  test(`CI reconcile installs and rechecks cached provenance for ${repository}`, { skip }, (t) => {
    const f = fixture(t, { repository });
    assert.equal(f.run().status, 0);
    const cached = f.run();
    assert.equal(cached.status, 0, cached.stderr);
    assert.deepEqual(cached.events.map((event) => event.kind), ['gh-version', 'gh-auth', 'verify', 'launch']);
  });
  for (const [name, mismatch] of [
    ['owner', { attestedRepository: 'untrusted/agentscm' }],
    ['workflow', { attestedWorkflow: `${repository}/.github/workflows/other.yml` }],
    ['tag', { attestedRef: 'refs/tags/v9.9.9' }],
    ['opposite migration identity', { attestedRepository: repository === 'counterbranch/agentscm' ? 'agentscm/agentscm' : 'counterbranch/agentscm' }],
  ]) {
    test(`CI reconcile rejects wrong ${name} for ${repository}`, { skip }, (t) => {
      const f = fixture(t, { repository, ...mismatch });
      const result = f.run();
      assertRejected(f, result, /build provenance verification failed/);
      assert.equal(result.events.at(-1).kind, 'verify');
    });
  }
}
