'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { NOTICE_TEXT, announces, targetFor, assetNameFor, installNames, urlFor, launchArgs, verifiedInstallEnv, parseSums, matches, localSums, ghVerifyArgs, provenanceGap, checkProvenance, provenanceLine } = require('../lib/install.js');

const fixtures = path.join(__dirname, 'fixtures');
const sums = fs.readFileSync(path.join(fixtures, 'SHA256SUMS'), 'utf8');
const sample = path.join(fixtures, 'sample.bin');
const hex = (c) => c.repeat(64);

test('platform mapping', () => {
  assert.equal(targetFor('linux', 'x64'), 'x86_64-unknown-linux-musl');
  assert.equal(targetFor('linux', 'arm64'), 'aarch64-unknown-linux-musl');
  assert.equal(targetFor('darwin', 'x64'), 'x86_64-apple-darwin');
  assert.equal(targetFor('darwin', 'arm64'), 'aarch64-apple-darwin');
  assert.equal(targetFor('win32', 'x64'), 'x86_64-pc-windows-msvc');
  for (const [p, a] of [['win32', 'arm64'], ['win32', 'ia32'], ['linux', 'ia32'], ['freebsd', 'x64'], ['darwin', 'arm'], ['constructor', 'x']]) {
    assert.equal(targetFor(p, a), null);
  }
});

test('SHA256SUMS parsing', () => {
  const t = 'x86_64-unknown-linux-musl';
  assert.equal(parseSums(sums, t), '2e872263e12593bf0c92feeb8cad3a84609a638af7515696ed3356631249a6d8');
  assert.throws(() => parseSums(sums + sums.split('\n')[0] + '\n', t), /exactly once/);
  assert.throws(() => parseSums(`${hex('a')} agentscm-${t}\n`, t), /malformed/); // one space
  assert.throws(() => parseSums(`${hex('A')}  agentscm-${t}\n`, t), /malformed/);
  assert.throws(() => parseSums(`${sums}junk\n`, t), /malformed/);
  assert.throws(() => parseSums(sums, 'aarch64-unknown-linux-musl'), /exactly once/);
});

test('asset names', () => {
  assert.equal(assetNameFor('x86_64-pc-windows-msvc'), 'agentscm-x86_64-pc-windows-msvc.exe');
  assert.equal(assetNameFor('aarch64-apple-darwin'), 'agentscm-aarch64-apple-darwin');
});

test('SHA256SUMS parsing with an .exe line', () => {
  const t = 'x86_64-pc-windows-msvc';
  assert.equal(parseSums(sums, t), 'ab'.repeat(32));
  const line = sums.split('\n').find((l) => l.endsWith('.exe'));
  assert.throws(() => parseSums(`${sums}${line}\n`, t), /exactly once/);
  assert.throws(() => parseSums(sums.replace('.exe', ''), t), /exactly once/); // the bare name is not the Windows asset
  assert.throws(() => parseSums(`${sums}${hex('a')}  agentscm-${t}.exe.bak\n`, t), /malformed/);
});

test('install names per platform', () => {
  assert.deepEqual(installNames('1.2.3', 'win32'), { versioned: 'agentscm-1.2.3.exe', latest: 'agentscm.exe', symlink: false });
  for (const p of ['linux', 'darwin']) {
    assert.deepEqual(installNames('1.2.3', p), { versioned: 'agentscm-1.2.3', latest: 'agentscm', symlink: true });
  }
});

test('checksum verification', () => {
  assert.equal(matches(sample, parseSums(sums, 'x86_64-unknown-linux-musl')), true);
  assert.equal(matches(sample, parseSums(sums, 'aarch64-apple-darwin')), false);
});

test('release URL', () => {
  assert.equal(
    urlFor('1.2.3', 'agentscm-aarch64-apple-darwin'),
    'https://github.com/counterbranch/agentscm/releases/download/v1.2.3/agentscm-aarch64-apple-darwin',
  );
});

test('release URL for the Windows asset', () => {
  assert.equal(
    urlFor('1.2.3', assetNameFor('x86_64-pc-windows-msvc')),
    'https://github.com/counterbranch/agentscm/releases/download/v1.2.3/agentscm-x86_64-pc-windows-msvc.exe',
  );
});

test('launch arguments', () => {
  assert.deepEqual(launchArgs([]), ['quickstart']);
  assert.deepEqual(launchArgs(['--yes']), ['quickstart', '--yes']);
  assert.deepEqual(launchArgs(['--version']), ['--version']);
  assert.deepEqual(launchArgs(['-V']), ['-V']);
  assert.deepEqual(launchArgs(['--help']), ['--help']);
  assert.deepEqual(launchArgs(['-h']), ['-h']);
  assert.deepEqual(launchArgs(['uninstall']), ['uninstall']);
  assert.deepEqual(launchArgs(['hub', 'serve', '--x']), ['hub', 'serve', '--x']);
  assert.deepEqual(launchArgs(['tray']), ['tray']);
  assert.deepEqual(launchArgs(['tray', '--foreground']), ['tray', '--foreground']);
});

test('verified installer marks only the launched product process', () => {
  const original = { PATH: '/bin', AGENTSCM_VERIFIED_INSTALL: 'stale' };
  assert.deepEqual(verifiedInstallEnv(original), {
    PATH: '/bin',
    AGENTSCM_VERIFIED_INSTALL: '1',
  });
  assert.equal(original.AGENTSCM_VERIFIED_INSTALL, 'stale');
});

test('README carries the notice text verbatim', () => {
  assert.ok(fs.readFileSync(path.join(__dirname, '..', 'README.md'), 'utf8').replace(/\r\n/g, '\n').includes(NOTICE_TEXT)); // Windows checkouts may use CRLF
});

test('checksum source selection', () => {
  assert.equal(localSums(fixtures), sums); // a SHA256SUMS next to the package wins
  const empty = fs.mkdtempSync(path.join(os.tmpdir(), 'agentscm-test-'));
  try {
    assert.equal(localSums(empty), null); // dev checkout: caller downloads it
  } finally {
    fs.rmSync(empty, { recursive: true });
  }
});

test('attestation verification is pinned to the release workflow and tag', () => {
  assert.deepEqual(ghVerifyArgs('/x/agentscm-1', '1.2.3'), [
    'attestation', 'verify', '/x/agentscm-1', '--repo', 'counterbranch/agentscm',
    '--signer-workflow', 'counterbranch/agentscm/.github/workflows/release.yml',
    '--source-ref', 'refs/tags/v1.2.3', '--deny-self-hosted-runners',
  ]);
});

test('provenance needs gh installed and signed in', () => {
  const ran = (map) => (_cmd, args) => map[args[0]];
  assert.match(provenanceGap(ran({ '--version': { error: new Error('ENOENT') } })), /not installed/);
  assert.match(provenanceGap(ran({ '--version': { status: 0 }, auth: { status: 1 } })), /not signed in/);
  assert.equal(provenanceGap(ran({ '--version': { status: 0 }, auth: { status: 0 } })), null);
});

test('without gh, provenance is reported to the caller unless required', () => {
  const empty = fs.mkdtempSync(path.join(os.tmpdir(), 'agentscm-test-'));
  const saved = { PATH: process.env.PATH, req: process.env.AGENTSCM_REQUIRE_PROVENANCE };
  try {
    process.env.PATH = empty;
    delete process.env.AGENTSCM_REQUIRE_PROVENANCE;
    assert.equal(checkProvenance(sample, '1.2.3'), false);
    process.env.AGENTSCM_REQUIRE_PROVENANCE = '1';
    assert.throws(() => checkProvenance(sample, '1.2.3'), /AGENTSCM_REQUIRE_PROVENANCE=1 but .*not installed/);
  } finally {
    process.env.PATH = saved.PATH;
    if (saved.req === undefined) delete process.env.AGENTSCM_REQUIRE_PROVENANCE; else process.env.AGENTSCM_REQUIRE_PROVENANCE = saved.req;
    fs.rmSync(empty, { recursive: true });
  }
});

test('successful provenance verification is quiet unless installer verbose mode is enabled', () => {
  const saved = process.env.AGENTSCM_INSTALL_VERBOSE;
  const error = console.error;
  const lines = [];
  const run = () => ({ status: 0, stdout: 'verified attestation\n', stderr: 'upgrade gh now\n' });
  try {
    console.error = (line) => lines.push(line);
    delete process.env.AGENTSCM_INSTALL_VERBOSE;
    assert.equal(checkProvenance(sample, '1.2.3', null, run), true);
    assert.deepEqual(lines, []);

    process.env.AGENTSCM_INSTALL_VERBOSE = '1';
    assert.equal(checkProvenance(sample, '1.2.3', null, run), true);
    assert.deepEqual(lines, ['verified attestation', 'upgrade gh now']);
  } finally {
    console.error = error;
    if (saved === undefined) delete process.env.AGENTSCM_INSTALL_VERBOSE; else process.env.AGENTSCM_INSTALL_VERBOSE = saved;
  }
});

test('failed provenance verification exposes captured diagnostics', () => {
  const error = console.error;
  const lines = [];
  try {
    console.error = (line) => lines.push(line);
    assert.throws(
      () => checkProvenance(sample, '1.2.3', null, () => ({ status: 1, stdout: '', stderr: 'attestation mismatch\n' })),
      /build provenance verification failed/,
    );
    assert.deepEqual(lines, ['attestation mismatch']);
  } finally {
    console.error = error;
  }
});

test('--version answers from package.json without notice or download', async () => {
  const lines = [];
  const orig = console.log;
  console.log = (...a) => lines.push(a.join(' '));
  try {
    assert.equal(await require('../lib/install.js').main(['--version']), 0);
  } finally {
    console.log = orig;
  }
  assert.deepEqual(lines, [`agentscm ${require('../package.json').version}`]);
});

test('provenance line says exactly what was checked', () => {
  assert.equal(provenanceLine('the GitHub CLI (gh) is not installed'), 'Checked the SHA-256 checksum (provenance not checked: the GitHub CLI (gh) is not installed).');
  assert.match(provenanceLine(null), /^Verified provenance with GitHub attestations\./);
});

test('the notice and provenance line appear when installing, never before help, always when verbose', () => {
  assert.equal(announces([], true, false), true);
  assert.equal(announces(['--hub-url', 'https://hub.example.com'], true, false), true);
  assert.equal(announces([], false, false), false);
  assert.equal(announces(['hub', 'login'], false, false), false);
  assert.equal(announces(['--help'], true, false), false);
  assert.equal(announces(['-h'], true, false), false);
  assert.equal(announces(['hub'], false, true), true);
});
