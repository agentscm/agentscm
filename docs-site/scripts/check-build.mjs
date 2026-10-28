import assert from 'node:assert/strict';
import { readdir, readFile, stat } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('../dist/', import.meta.url));
const htmlFiles = [];
const outputFiles = [];
const privateSentinels = [
  '/Users/PRIVATE_USER/private-project/',
  '/home/PRIVATE_USER/private-project/',
  'C:\\Users\\PRIVATE_USER\\private-project\\',
  '/private-evidence-fixture/',
];

async function walk(directory) {
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const file = path.join(directory, entry.name);
    if (entry.isDirectory()) await walk(file);
    else {
      outputFiles.push(file);
      if (entry.name.endsWith('.html')) htmlFiles.push(file);
    }
  }
}

await walk(root);
assert.ok(htmlFiles.includes(path.join(root, 'index.html')), 'documentation home route exists');
assert.ok(await stat(path.join(root, 'pagefind', 'pagefind.js')).catch(() => false), 'Pagefind search index exists');
assert.equal((await readFile(path.join(root, 'CNAME'), 'utf8')).trim(), 'docs.agentscm.dev', 'custom domain file is emitted');

for (const file of outputFiles) {
  const contents = await readFile(file, 'utf8');
  for (const value of privateSentinels) {
    assert.ok(!contents.includes(value), `${path.relative(root, file)} excludes a private path sentinel`);
  }
}

const structuralIssues = [];
const linkIssues = [];
for (const html of htmlFiles) {
  const source = await readFile(html, 'utf8');
  const headingCount = (source.match(/<h1\b/g) ?? []).length;
  if (headingCount !== 1) structuralIssues.push(`${path.relative(root, html)} has ${headingCount} page headings`);
  const ids = new Set([...source.matchAll(/\b(?:id|name)="([^"]+)"/g)].map((match) => match[1]));
  for (const [, rawUrl] of source.matchAll(/\b(?:href|src)="([^"]+)"/g)) {
    if (/^(?:[a-z][a-z\d+.-]*:|\/\/)/i.test(rawUrl)) continue;
    const [rawPathname, rawFragment] = rawUrl.split('#', 2);
    const pathname = decodeURIComponent(rawPathname.split('?', 1)[0]);
    const fragment = rawFragment ? decodeURIComponent(rawFragment) : '';
    if (!pathname) {
      if (fragment && !ids.has(fragment)) linkIssues.push(`fragment resolves in ${path.relative(root, html)}: ${rawUrl}`);
      continue;
    }
    const target = pathname.startsWith('/')
      ? path.join(root, pathname.slice(1))
      : path.resolve(path.dirname(html), pathname);
    const candidates = [target, `${target}.html`, path.join(target, 'index.html')];
    const resolved = await Promise.any(candidates.map(async (file) => {
      const info = await stat(file);
      return info.isDirectory() ? path.join(file, 'index.html') : file;
    })).catch(() => null);
    if (!resolved) {
      linkIssues.push(`local link or asset resolves from ${path.relative(root, html)}: ${rawUrl}`);
      continue;
    }
    if (fragment && resolved.endsWith('.html')) {
      const targetIds = new Set([...(await readFile(resolved, 'utf8')).matchAll(/\b(?:id|name)="([^"]+)"/g)].map((match) => match[1]));
      if (!targetIds.has(fragment)) linkIssues.push(`fragment resolves in ${path.relative(root, resolved)}: ${rawUrl}`);
    }
  }
}

assert.deepEqual([...structuralIssues, ...linkIssues], [],
  `site checks failed:\n${[...structuralIssues, ...linkIssues].join('\n')}`);
console.log(`Checked ${htmlFiles.length} pages, Pagefind search output, local links, assets, and CNAME.`);
