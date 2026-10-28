function text(node) {
  return node.type === 'text' || node.type === 'inlineCode'
    ? node.value
    : (node.children ?? []).map(text).join('');
}

export default function removePageHeading() {
  return (tree, file) => {
    const title = file.data.astro?.frontmatter?.title;
    const index = tree.children.findIndex((node) => node.type === 'heading' && node.depth === 1);
    if (title && index !== -1 && text(tree.children[index]).trim() === title.replaceAll('`', '').trim()) {
      tree.children.splice(index, 1);
    }
  };
}
