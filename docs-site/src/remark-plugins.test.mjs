import assert from 'node:assert/strict';
import path from 'node:path';
import test from 'node:test';
import rehypeCodeBreaks from './rehype-code-breaks.js';
import remarkMermaid from './remark-mermaid.js';
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

test('mermaid code blocks become escaped diagram containers and other code stays', () => {
  const tree = { children: [
    { type: 'code', lang: 'mermaid', value: 'flowchart LR\n  A[a & <b>] -->|"x"| B' },
    { type: 'code', lang: 'sh', value: 'cargo test' },
    { type: 'blockquote', children: [{ type: 'code', lang: 'mermaid', value: 'flowchart TD' }] },
  ] };
  remarkMermaid()(tree);
  assert.deepEqual(tree.children[0], {
    type: 'html',
    value: '<pre class="mermaid" data-mermaid>flowchart LR\n  A[a &amp; &lt;b&gt;] --&gt;|&quot;x&quot;| B</pre>',
  });
  assert.deepEqual(tree.children[1], { type: 'code', lang: 'sh', value: 'cargo test' });
  assert.equal(tree.children[2].children[0].value, '<pre class="mermaid" data-mermaid>flowchart TD</pre>');
});

test('inline code gets break opportunities after separators; code blocks are untouched', () => {
  const code = (value) => ({ type: 'element', tagName: 'code', properties: {}, children: [{ type: 'text', value }] });
  const wbr = { type: 'element', tagName: 'wbr', properties: {}, children: [] };
  const text = (value) => ({ type: 'text', value });
  const tree = { type: 'root', children: [
    { type: 'element', tagName: 'td', properties: {}, children: [code('tests/hub_x.rs'), code('agentscm'), code('--apply'), code('a__b_')] },
    { type: 'element', tagName: 'pre', properties: {}, children: [code('keep_this/as-is')] },
  ] };
  rehypeCodeBreaks()(tree);
  const [path, word, flag, edges] = tree.children[0].children.map((node) => node.children);
  assert.deepEqual(path, [text('tests'), text('/'), wbr, text('hub'), text('_'), wbr, text('x'), text('.'), wbr, text('rs')]);
  assert.deepEqual(word, [text('agentscm')]);
  assert.deepEqual(flag, [text('-'), text('-'), wbr, text('apply')]);
  assert.deepEqual(edges, [text('a_'), text('_'), wbr, text('b_')]);
  assert.deepEqual(tree.children[1].children[0].children, [text('keep_this/as-is')]);
});
