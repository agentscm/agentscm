// Adds <wbr> break opportunities after `_ / . - ?` in inline code. Long identifiers such as
// test names and paths contain no spaces, so a table would otherwise size a column to the
// whole identifier and overflow the content area. A <wbr> lets the line break only between
// segments, never inside a word, and leaves copied text unchanged.
const BREAK_AFTER = /([_/.\-?])(?=[^_/.\-?])/;

function splitText(value) {
  const parts = value.split(BREAK_AFTER);
  if (parts.length === 1) return null;
  const nodes = [];
  for (const [index, part] of parts.entries()) {
    if (part === '') continue;
    nodes.push({ type: 'text', value: part });
    // split() with a capture group puts each separator at an odd index.
    if (index % 2 === 1) nodes.push({ type: 'element', tagName: 'wbr', properties: {}, children: [] });
  }
  return nodes;
}

function visit(node, inPre) {
  if (node.type !== 'element' && node.type !== 'root') return;
  const pre = inPre || (node.type === 'element' && node.tagName === 'pre');
  if (node.type === 'element' && node.tagName === 'code' && !pre) {
    node.children = node.children.flatMap((child) =>
      (child.type === 'text' && splitText(child.value)) || [child]);
    return;
  }
  for (const child of node.children ?? []) visit(child, pre);
}

export default function rehypeCodeBreaks() {
  return (tree) => visit(tree, false);
}
