'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');
const source = path.resolve(__dirname, '../../..');
const version = require('../package.json').version;

function release(t, repository, extraEnv = {}) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'agentscm-release-identity-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  for (const dir of ['dist', 'packages/npm/lib']) fs.mkdirSync(path.join(root, dir), { recursive: true });
  const files = ['install.sh', 'packages/npm/package.json', 'packages/npm/lib/install.js'];
  for (const file of files) fs.copyFileSync(path.join(source, file), path.join(root, file));
  const result = spawnSync(process.execPath, [path.join(source, '.github/pin-release-installers.js')], {
    cwd: root, encoding: 'utf8',
    env: { GITHUB_REPOSITORY: repository, GITHUB_REPOSITORY_ID: '1423506417', GITHUB_REPOSITORY_OWNER_ID: '326639785', VERSION: version, ...extraEnv },
  });
  return { root, files, result };
}

for (const repository of ['counterbranch/agentscm', 'agentscm/agentscm']) {
  test(`release pins downloads, npm metadata and the sole signer to ${repository}`, (t) => {
    const { root, result } = release(t, repository);
    assert.equal(result.status, 0, result.stderr);
    const wrapper = require(path.join(root, 'packages/npm/lib/install.js'));
    assert.equal(wrapper.urlFor(version, 'binary'), `https://github.com/${repository}/releases/download/v${version}/binary`);
    assert.deepEqual(wrapper.ghVerifyArgs('binary', version), [
      'attestation', 'verify', 'binary', '--repo', repository,
      '--signer-workflow', `${repository}/.github/workflows/release.yml`,
      '--source-ref', `refs/tags/v${version}`, '--deny-self-hosted-runners',
    ]);
    const pkg = JSON.parse(fs.readFileSync(path.join(root, 'packages/npm/package.json')));
    assert.equal(pkg.repository.url, `git+https://github.com/${repository}.git`);
    assert.equal(pkg.homepage, `https://github.com/${repository}`);
    const shell = fs.readFileSync(path.join(root, 'dist/install.sh'), 'utf8');
    assert.ok(shell.includes(`AGENTSCM_DEFAULT_VERSION="${version}"`));
    assert.match(shell, new RegExp(`^REPO="${repository}"$`, 'm'));
    assert.equal(fs.readFileSync(path.join(root, 'install.sh'), 'utf8'), fs.readFileSync(path.join(source, 'install.sh'), 'utf8'));
    if (process.platform !== 'win32') {
      const override = spawnSync('sh', [path.join(root, 'dist/install.sh')], {
        encoding: 'utf8', env: { PATH: process.env.PATH, AGENTSCM_VERSION: '9.9.9' },
      });
      assert.notEqual(override.status, 0);
      assert.match(override.stderr, /this script is pinned to .*download install.sh from the requested release/);
    }
  });
}

for (const [name, repository, environment] of [
  ['fork with copied source', 'example/agentscm', {}],
  ['recycled owner/repository name', 'counterbranch/agentscm', { GITHUB_REPOSITORY_ID: '1' }],
  ['repository transferred to the placeholder', 'agentscm/agentscm', { GITHUB_REPOSITORY_OWNER_ID: '345972930' }],
  ['unrelated renamed repository', 'example/agentscm', { GITHUB_REPOSITORY_ID: '1423506417' }],
  ['wrong release version', 'agentscm/agentscm', { VERSION: '9.9.9' }],
  ['malformed release version', 'agentscm/agentscm', { VERSION: '../release' }],
]) {
  test(`release preparation rejects ${name} before changing files`, (t) => {
    const { root, files, result } = release(t, repository, environment);
    assert.notEqual(result.status, 0);
    assert.equal(fs.existsSync(path.join(root, 'dist/install.sh')), false);
    for (const file of files) assert.deepEqual(fs.readFileSync(path.join(root, file)), fs.readFileSync(path.join(source, file)));
  });
}

test('all release jobs require the immutable public repository identity', () => {
  const workflow = fs.readFileSync(path.join(source, '.github/workflows/release.yml'), 'utf8');
  assert.equal(workflow.match(/if: \$\{\{ github.repository_id == '1423506417' && github.repository_owner_id == '326639785' && !github.event.repository.private \}\}/g)?.length, 7);
  assert.equal(workflow.match(/node \.github\/pin-release-installers.js/g)?.length, 2);
  assert.doesNotMatch(workflow, /github.repository ==/);
});
