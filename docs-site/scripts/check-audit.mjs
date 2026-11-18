import { spawnSync } from 'node:child_process';

const exception = {
  package: 'picomatch',
  url: 'https://github.com/advisories/GHSA-fc5q-39xc-vjpf',
  expiresAt: Date.parse('2026-12-02T00:00:00Z'),
};

const audit = spawnSync('npm', ['audit', '--audit-level=low', '--json'], {
  encoding: 'utf8',
  maxBuffer: 8 * 1024 * 1024,
});
if (audit.stderr) process.stderr.write(audit.stderr);
if (audit.stdout) process.stdout.write(audit.stdout);

function matchesException(findings) {
  const advisoryPackages = new Set();
  for (const [name, finding] of findings) {
    if (finding?.name !== name || !Array.isArray(finding.via) || !finding.via.length) return false;
    for (const cause of finding.via) {
      if (typeof cause === 'string') {
        if (!findings.has(cause)) return false;
        continue;
      }
      if (name !== exception.package || cause?.name !== exception.package ||
          cause.url !== exception.url) return false;
      advisoryPackages.add(name);
    }
  }

  // npm also reports dependents as findings whose causes reference another
  // package by name. Every finding must trace back to the exempt advisory.
  if (!advisoryPackages.size) return false;
  const covered = new Set(advisoryPackages);
  for (let previousSize = 0; covered.size !== previousSize;) {
    previousSize = covered.size;
    for (const [name, finding] of findings) {
      if (finding.via.some(cause => typeof cause === 'string' && covered.has(cause))) {
        covered.add(name);
      }
    }
  }
  return covered.size === findings.size;
}

try {
  if (audit.error || audit.signal) throw audit.error ?? new Error(audit.signal);
  const report = JSON.parse(audit.stdout);
  const vulnerabilities = report?.vulnerabilities;
  if (report?.error || report?.auditReportVersion !== 2 || !vulnerabilities ||
      typeof vulnerabilities !== 'object' || Array.isArray(vulnerabilities)) {
    throw new Error('Unexpected npm audit report format');
  }
  const findings = new Map(Object.entries(vulnerabilities));
  if (audit.status === 0 && findings.size === 0) {
    process.exitCode = 0;
  } else if (audit.status === 1 && Date.now() < exception.expiresAt && matchesException(findings)) {
    console.warn('Allowing GHSA-fc5q-39xc-vjpf only (engine issue #633, expires 2026-12-02 UTC).');
  } else {
    process.exitCode = 1;
  }
} catch (error) {
  console.error('Could not verify the npm audit report:', error.message);
  process.exitCode = 1;
}
