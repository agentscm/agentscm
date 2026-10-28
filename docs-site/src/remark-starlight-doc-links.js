import path from 'node:path';
import { execFileSync } from 'node:child_process';

const docsRoot = path.resolve(import.meta.dirname, '../../docs');
const repoRoot = path.dirname(docsRoot);
const tracked = new Set(execFileSync('git', ['ls-files', '-z'], { cwd: repoRoot }).toString().split('\0'));
const trackedDirectories = new Set();
for (const file of tracked) {
  const segments = file.split('/');
  for (let index = 1; index < segments.length; index += 1) {
    trackedDirectories.add(segments.slice(0, index).join('/'));
  }
}

function encodeRepoPath(repoPath) {
  return repoPath.split('/').map(encodeURIComponent).join('/');
}

/** Convert GitHub-style relative Markdown links to the matching Starlight route. */
export default function remarkStarlightDocLinks() {
  return (tree, file) => {
    if (!file.path) return;
    const sourceDir = path.dirname(file.path);

    function visit(node) {
      if (node.type === 'link' && typeof node.url === 'string') {
        const match = node.url.match(/^([^?#]+)([?#].*)?$/);
        if (match && !match[1].startsWith('/') && !/^[a-z][a-z\d+.-]*:/i.test(match[1])) {
          const target = path.resolve(sourceDir, match[1]);
          const relative = path.relative(docsRoot, target);
          if (match[1].endsWith('.md') && relative && !relative.startsWith(`..${path.sep}`) && relative !== '..' && !path.isAbsolute(relative)) {
            const page = relative.replace(/\\/g, '/').replace(/\.md$/, '');
            const route = page === 'README' ? '/' : `/${page}/`;
            node.url = `${route}${match[2] ?? ''}`;
          } else {
            const repoPath = path.relative(repoRoot, target).replace(/\\/g, '/');
            if (tracked.has(repoPath)) {
              const host = /\.(?:png|jpe?g|gif|svg|webp)$/i.test(repoPath)
                ? 'https://raw.githubusercontent.com/agentscm/agentscm/main/'
                : 'https://github.com/agentscm/agentscm/blob/main/';
              node.url = `${host}${encodeRepoPath(repoPath)}${match[2] ?? ''}`;
            } else if (trackedDirectories.has(repoPath)) {
              const trailingSlash = match[1].endsWith('/') ? '/' : '';
              node.url = `https://github.com/agentscm/agentscm/tree/main/${encodeRepoPath(repoPath)}${trailingSlash}${match[2] ?? ''}`;
            }
          }
        }
      }
      if (Array.isArray(node.children)) for (const child of node.children) visit(child);
    }

    visit(tree);
  };
}
