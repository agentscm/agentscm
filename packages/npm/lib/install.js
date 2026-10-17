'use strict';
const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { execFileSync, spawnSync } = require('node:child_process');

const REPO = 'counterbranch/agentscm';
const SUMS_MAX = 64 * 1024;
const BINARY_MAX = 200 * 1024 * 1024;
const BUDGET_MS = 600_000; // overall, for both downloads

const NOTICE_TEXT = `npx agentscm downloads the AgentSCM release binary for your platform from
GitHub Releases, verifies its SHA-256 checksum and build provenance, and then
starts the setup guide. It does not run install scripts, send telemetry or
change anything without asking. Everything it writes is listed before it
writes it, and can be undone with \`agentscm uninstall\`.`;

const TARGETS = {
  'linux/x64': 'x86_64-unknown-linux-musl',
  'linux/arm64': 'aarch64-unknown-linux-musl',
  'darwin/x64': 'x86_64-apple-darwin',
  'darwin/arm64': 'aarch64-apple-darwin',
  'win32/x64': 'x86_64-pc-windows-msvc',
};

const targetFor = (platform, arch) => Object.hasOwn(TARGETS, `${platform}/${arch}`) ? TARGETS[`${platform}/${arch}`] : null;

// The release asset for a target; the Windows binary carries an .exe suffix.
const assetNameFor = (target) => `agentscm-${target}${target.endsWith('-windows-msvc') ? '.exe' : ''}`;

// File names in the install dir. Windows has no unprivileged symlinks, so "latest" is a copy there.
const installNames = (version, platform) => platform === 'win32'
  ? { versioned: `agentscm-${version}.exe`, latest: 'agentscm.exe', symlink: false }
  : { versioned: `agentscm-${version}`, latest: 'agentscm', symlink: true };

// The download host is fixed on purpose: no env var or flag may change it.
const urlFor = (version, asset) => `https://github.com/${REPO}/releases/download/v${version}/${asset}`;

// A bare `npx agentscm` or a leading flag starts the setup guide; any other first argument is a subcommand.
// The binary's own top-level flags pass through; any other leading flag goes to the setup guide.
const TOP_LEVEL_FLAGS = new Set(['--version', '-V', '--help', '-h']);
const launchArgs = (args) =>
  args.length === 0 || (args[0].startsWith('-') && !TOP_LEVEL_FLAGS.has(args[0])) ? ['quickstart', ...args] : args;

// Strict: every line must be `<64 lowercase hex>  agentscm-<target>[.exe]`, and the asset appears exactly once.
function parseSums(text, target) {
  const hashes = [];
  for (const line of text.replace(/\n$/, '').split('\n')) {
    const m = /^([0-9a-f]{64}) {2}(agentscm-[a-z0-9_-]+(?:\.exe)?)$/.exec(line);
    if (!m) throw new Error('SHA256SUMS has a malformed line');
    if (m[2] === assetNameFor(target)) hashes.push(m[1]);
  }
  if (hashes.length !== 1) throw new Error(`SHA256SUMS must list ${assetNameFor(target)} exactly once`);
  return hashes[0];
}

const sha256File = (file) => crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
const matches = (file, expected) => sha256File(file) === expected;

async function download(url, max, onChunk, signal) {
  const res = await fetch(url, { signal });
  if (!res.url.startsWith('https://')) throw new Error('download redirected to a non-https URL');
  if (!res.ok) throw new Error(`download failed (HTTP ${res.status}): ${url}`);
  let total = 0;
  for await (const chunk of res.body) {
    total += chunk.length;
    if (total > max) throw new Error(`download exceeds the ${max} byte limit: ${url}`);
    onChunk(chunk);
  }
}

// Checksums shipped inside the npm package come from the attested build artifact, not the release assets (see docs/install.md).
function localSums(pkgDir) {
  try {
    return fs.readFileSync(path.join(pkgDir, 'SHA256SUMS'), 'utf8');
  } catch (err) {
    if (err.code === 'ENOENT') return null;
    throw err;
  }
}

// Pinned to this repository's release workflow and tag, so a binary attested by any other workflow fails.
const ghVerifyArgs = (file, version) => [
  'attestation', 'verify', file, '--repo', REPO,
  '--signer-workflow', `${REPO}/.github/workflows/release.yml`,
  '--source-ref', `refs/tags/v${version}`,
  '--deny-self-hosted-runners',
];

function unsupported(version) {
  return `no AgentSCM release binary for ${process.platform}/${process.arch}.
Use Docker instead:
  docker run --rm -it ghcr.io/${REPO}:${version} --help
See https://github.com/${REPO}/blob/v${version}/docs/install.md`;
}

// Why provenance cannot be checked, or null when gh is installed and signed in.
function provenanceGap(run = spawnSync) {
  if (run('gh', ['--version'], { stdio: 'ignore' }).error) return 'the GitHub CLI (gh) is not installed';
  if (run('gh', ['auth', 'status', '--hostname', 'github.com'], { stdio: 'ignore', timeout: 30_000 }).status !== 0) {
    return 'the GitHub CLI (gh) is not signed in to github.com or cannot reach it (see `gh auth status`)';
  }
  return null;
}

// Throws if the attestation does not verify. With no usable gh it only warns, unless AGENTSCM_REQUIRE_PROVENANCE=1.
function checkProvenance(file, version, gap = provenanceGap()) {
  if (gap) {
    if (process.env.AGENTSCM_REQUIRE_PROVENANCE === '1') throw new Error(`AGENTSCM_REQUIRE_PROVENANCE=1 but ${gap}`);
    console.log(`Checksum-verified only: provenance was not checked (${gap}).`);
    return;
  }
  try {
    execFileSync('gh', ghVerifyArgs(file, version), { stdio: 'inherit' });
  } catch {
    throw new Error('build provenance verification failed; not running the binary');
  }
}

async function main(args) {
  const version = require('../package.json').version;
  console.log(`${NOTICE_TEXT}\n`);
  const target = targetFor(process.platform, process.arch);
  if (!target) {
    console.error(`agentscm: ${unsupported(version)}`);
    return 1;
  }
  const asset = assetNameFor(target);
  const names = installNames(version, process.platform);
  const signal = AbortSignal.timeout(BUDGET_MS);
  const gap = provenanceGap(); // checked once: `gh auth status` goes over the network
  if (gap && process.env.AGENTSCM_REQUIRE_PROVENANCE === '1') throw new Error(`AGENTSCM_REQUIRE_PROVENANCE=1 but ${gap}`);

  let sums = localSums(path.join(__dirname, '..'));
  if (sums === null) {
    const parts = [];
    await download(urlFor(version, 'SHA256SUMS'), SUMS_MAX, (c) => parts.push(c), signal);
    sums = Buffer.concat(parts).toString('utf8');
    console.log('Checksums from the GitHub release.');
  } else {
    console.log('Checksums from the npm package.');
  }
  const expected = parseSums(sums, target);

  const dir = path.join(os.homedir(), '.agentscm', 'bin');
  fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
  fs.chmodSync(dir, 0o700);
  const link = path.join(dir, names.latest);
  const lst = fs.lstatSync(link, { throwIfNoEntry: false });
  if (names.symlink && lst && !lst.isSymbolicLink()) throw new Error(`${link} exists and is not a symlink; remove it`);

  // Order: checksum, provenance, chmod, rename, link, exec. Nothing is installed or linked before both checks pass.
  const binary = path.join(dir, names.versioned);
  const rand = crypto.randomBytes(6).toString('hex');
  if (fs.existsSync(binary)) {
    if (!matches(binary, expected)) throw new Error(`${binary} does not match the release checksum; delete it and retry`);
    checkProvenance(binary, version, gap);
    console.log(`Using the verified ${binary}`);
  } else {
    const tmp = path.join(dir, `.agentscm-${version}-${rand}.tmp`);
    try {
      const fd = fs.openSync(tmp, 'wx', 0o700);
      try {
        await download(urlFor(version, asset), BINARY_MAX, (c) => fs.writeSync(fd, c), signal);
      } finally {
        fs.closeSync(fd);
      }
      if (!matches(tmp, expected)) throw new Error(`checksum mismatch for ${asset}; nothing was installed`);
      checkProvenance(tmp, version, gap);
      fs.chmodSync(tmp, 0o755);
      fs.renameSync(tmp, binary);
    } finally {
      fs.rmSync(tmp, { force: true });
    }
  }

  const tmpLink = path.join(dir, `.agentscm-link-${rand}`);
  if (names.symlink) {
    fs.symlinkSync(path.basename(binary), tmpLink);
    fs.renameSync(tmpLink, link);
  } else {
    try {
      fs.copyFileSync(binary, tmpLink);
      fs.renameSync(tmpLink, link);
    } catch (err) {
      // A running agentscm.exe is locked; the versioned binary is what runs below.
      if (!['EPERM', 'EBUSY', 'EACCES'].includes(err.code)) throw err;
      console.warn(`agentscm: could not replace ${link} (in use?); it keeps the previous version`);
    } finally {
      fs.rmSync(tmpLink, { force: true });
    }
  }

  const r = spawnSync(binary, launchArgs(args), { stdio: 'inherit' });
  if (r.error) throw r.error;
  return r.status ?? 128 + (os.constants.signals[r.signal] ?? 0);
}

module.exports = { NOTICE_TEXT, targetFor, assetNameFor, installNames, urlFor, launchArgs, parseSums, sha256File, matches, localSums, ghVerifyArgs, provenanceGap, checkProvenance, main };
