import assert from 'node:assert/strict';
import path from 'node:path';
import test from 'node:test';
import removePageHeading from './remark-remove-page-heading.js';
import starlightDocLinks from './remark-starlight-doc-links.js';

test('matching Markdown title with inline code leaves one Starlight heading', () => {
  const tree = { children: [
    { type: 'heading', depth: 1, children: [
      { type: 'text', value: 'Guided setup: ' },
      { type: 'inlineCode', value: 'agentscm quickstart' },
    ] },
    { type: 'paragraph', children: [] },
  ] };
  removePageHeading()(tree, { data: { astro: { frontmatter: { title: 'Guided setup: `agentscm quickstart`' } } } });
  assert.equal(tree.children.length, 1);
});

test('docs routes, tracked source links, assets, and unknown links', () => {
  const urls = [
    'install.md#windows',
    '../examples/slack-manifest.json',
    'assets/agentscm-icon.png',
    '../missing-file.json',
    'https://example.com/guide.md',
    '#section',
  ];
  const tree = { children: urls.map((url) => ({ type: 'link', url })) };
  starlightDocLinks()(tree, { path: path.resolve(import.meta.dirname, '../../docs/slack.md') });
  assert.deepEqual(tree.children.map(({ url }) => url), [
    '/install/#windows',
    'https://github.com/agentscm/agentscm/blob/main/examples/slack-manifest.json',
    'https://raw.githubusercontent.com/agentscm/agentscm/main/docs/assets/agentscm-icon.png',
    '../missing-file.json',
    'https://example.com/guide.md',
    '#section',
  ]);
});

test('relative links to repository-root source files resolve to GitHub URLs', () => {
  const tree = { children: [
    { type: 'link', url: '../../README.md' },
    { type: 'link', url: '../../CHANGELOG.md' },
  ] };
  starlightDocLinks()(tree, { path: path.resolve(import.meta.dirname, '../../docs/advanced/README.md') });
  assert.deepEqual(tree.children.map(({ url }) => url), [
    'https://github.com/agentscm/agentscm/blob/main/README.md',
    'https://github.com/agentscm/agentscm/blob/main/CHANGELOG.md',
  ]);
});

test('relative links to tracked repository directories resolve to GitHub tree URLs', () => {
  const tree = { children: [
    { type: 'link', url: '../../tests/fixtures/intent-review/' },
  ] };
  starlightDocLinks()(tree, { path: path.resolve(import.meta.dirname, '../../docs/advanced/intent-review.md') });
  assert.deepEqual(tree.children.map(({ url }) => url), [
    'https://github.com/agentscm/agentscm/tree/main/tests/fixtures/intent-review/',
  ]);
});
