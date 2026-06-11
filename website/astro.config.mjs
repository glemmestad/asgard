// @ts-check
import { defineConfig } from 'astro/config';
import remarkDirective from 'remark-directive';
import rehypeSlug from 'rehype-slug';
import { visit } from 'unist-util-visit';

// Render Docusaurus-style admonitions (:::tip[Title] ... :::) as <aside> blocks.
function remarkAdmonitions() {
  const known = ['note', 'tip', 'info', 'caution', 'warning', 'danger'];
  return (tree) => {
    visit(tree, (node) => {
      if (node.type !== 'containerDirective') return;
      const type = known.includes(node.name) ? node.name : 'note';
      node.data = node.data || {};
      node.data.hName = 'aside';
      node.data.hProperties = { className: ['admo', `admo-${type}`] };

      let title = type.charAt(0).toUpperCase() + type.slice(1);
      const first = node.children[0];
      if (first && first.data && first.data.directiveLabel) {
        const text = first.children?.map((c) => c.value).join('') ?? '';
        if (text.trim()) title = text;
        node.children.shift();
      }
      node.children.unshift({
        type: 'paragraph',
        data: { hName: 'p', hProperties: { className: ['admo-title'] } },
        children: [{ type: 'text', value: title }],
      });
    });
  };
}

export default defineConfig({
  site: 'https://frontkeep.build',
  output: 'static',
  devToolbar: { enabled: false },
  markdown: {
    remarkPlugins: [remarkDirective, remarkAdmonitions],
    rehypePlugins: [rehypeSlug],
    shikiConfig: { theme: 'github-dark', wrap: false },
  },
});
