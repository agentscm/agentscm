'use strict';
// Runs install.sh against a stub curl that serves only the fixture SHA256SUMS, so no network is used.
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const installSh = path.join(__dirname, '..', '..', '..', 'install.sh');
const fixtureSums = path.join(__dirname, 'fixtures', 'SHA256SUMS');
const target = { 'linux/x64': 'x86_64-unknown-linux-musl', 'darwin/arm64': 'aarch64-apple-darwin' }[`${process.platform}/${process.arch}`]; // both are in the fixture
const skip = !target && 'install.sh needs Linux x86_64 or macOS arm64 here';

function run(sums, extraEnv = {}, stubs = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'agentscm-sh-'));
  try {
    const bin = path.join(dir, 'bin');
    fs.mkdirSync(bin);
    fs.writeFileSync(path.join(dir, 'SHA256SUMS'), sums);
    const curl = `#!/bin/sh
for a; do [ "$prev" = -o ] && out=$a; prev=$a; url=$a; done
case "$url" in */SHA256SUMS) cp "$SUMS" "$out" ;; *) exit 22 ;; esac
`;
    for (const [name, body] of Object.entries({ curl, ...stubs })) fs.writeFileSync(path.join(bin, name), body, { mode: 0o755 });
    return spawnSync('sh', [installSh], {
      encoding: 'utf8',
      env: { PATH: `${bin}:${process.env.PATH}`, HOME: dir, SUMS: path.join(dir, 'SHA256SUMS'), AGENTSCM_VERSION: '0.0.0-test', ...extraEnv },
    });
  } finally {
    fs.rmSync(dir, { recursive: true });
  }
}

test('install.sh accepts a SHA256SUMS that lists the Windows .exe', { skip }, () => {
  const r = run(fs.readFileSync(fixtureSums, 'utf8'));
  assert.match(r.stdout, /Checksums from the GitHub release/);
  assert.match(r.stderr, new RegExp(`could not download agentscm-${target}`)); // got past the checksum file checks
  assert.doesNotMatch(r.stderr, /malformed/);
});

test('install.sh rejects a malformed SHA256SUMS line', { skip }, () => {
  const r = run(`${fs.readFileSync(fixtureSums, 'utf8')}junk\n`);
  assert.notEqual(r.status, 0);
  assert.match(r.stderr, /SHA256SUMS has a malformed line/);
});

test('install.sh dies under AGENTSCM_REQUIRE_PROVENANCE=1 when gh is not signed in', { skip }, () => {
  const r = run(fs.readFileSync(fixtureSums, 'utf8'), { AGENTSCM_REQUIRE_PROVENANCE: '1' }, { gh: '#!/bin/sh\n[ "$1" = auth ] && exit 1\nexit 0\n' });
  assert.notEqual(r.status, 0);
  assert.match(r.stderr, /AGENTSCM_REQUIRE_PROVENANCE=1 but the GitHub CLI \(gh\) is not signed in/);
});
