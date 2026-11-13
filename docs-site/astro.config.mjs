import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import { unified } from '@astrojs/markdown-remark';
import rehypeCodeBreaks from './src/rehype-code-breaks.js';
import removePageHeading from './src/remark-remove-page-heading.js';
import remarkMermaid from './src/remark-mermaid.js';
import starlightDocLinks from './src/remark-starlight-doc-links.js';

export default defineConfig({
  site: 'https://docs.agentscm.dev',
  integrations: [
    starlight({
      title: 'AgentSCM Documentation',
      favicon: '/favicon.png',
      social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/agentscm/agentscm' }],
      components: {
        MarkdownContent: './src/components/MarkdownContent.astro',
        SocialIcons: './src/components/SocialIcons.astro',
      },
      sidebar: [
        { label: 'Start', items: [
          { label: 'Documentation', link: '/' },
          { label: 'See it in action', link: '/scripted-demo/' },
          { label: 'Install', link: '/install/' },
          { label: 'How it works', link: '/concepts/' },
          { label: 'Guided setup', link: '/setup-guide/' },
          { label: 'Two-container Hub walkthrough', link: '/two-agent-demo/' },
        ] },
        { label: 'Coordinate', items: [
          { label: 'Supported agents', link: '/agents/' },
          { label: 'Work intents', link: '/intents/' },
          { label: 'Collision warnings', link: '/collisions/' },
          { label: 'Hub', link: '/hub/' },
          { label: 'Record a deployment', link: '/record-deployments/' },
          { label: 'Join a team Hub', link: '/team-member-onboarding/' },
          { label: 'Telemetry', link: '/telemetry/' },
        ] },
        { label: 'Native alpha', items: [
          { label: 'Claude Code workflow loop', link: '/advanced/claude-code-workflows/' },
          { label: 'Community workspace', link: '/advanced/community-workspace/' },
          { label: 'Architecture', link: '/architecture/' },
          { label: 'Roadmap', link: '/roadmap/' },
          { label: 'Deployment', link: '/deployment/' },
          { label: 'Hosted Hub', link: '/hosted/' },
        ] },
        { label: 'Reference and contributing', items: [
          { label: 'CLI reference', link: '/reference/cli/' },
          { label: 'HTTP API', link: '/api/' },
          { label: 'Contributor testing', link: '/contributor-testing/' },
          { label: 'Releasing', link: '/releasing/' },
        ] },
      ],
    }),
  ],
  markdown: { processor: unified({ remarkPlugins: [removePageHeading, starlightDocLinks, remarkMermaid],
    rehypePlugins: [rehypeCodeBreaks],
  }) },
});
