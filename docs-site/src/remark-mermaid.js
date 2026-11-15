// Turns ```mermaid code blocks into diagram containers before Expressive Code highlights them.
// MarkdownContent.astro renders each container in the browser. The escaped source stays in the
// HTML, so a reader without JavaScript, and the search index, still get the diagram text.
function escapeHtml(value) {
  return value
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;');
}

function visit(node) {
  for (const [index, child] of (node.children ?? []).entries()) {
    if (child.type === 'code' && child.lang === 'mermaid') {
      node.children[index] = {
        type: 'html',
        value: `<pre class="mermaid" data-mermaid>${escapeHtml(child.value)}</pre>`,
      };
    } else {
      visit(child);
    }
  }
}

export default function remarkMermaid() {
  return (tree) => visit(tree);
}
