# AgentSCM documentation site

The site loads every Markdown file under the repository's `docs/` directory at build time. The sidebar in `astro.config.mjs` controls the guided navigation; it does not control which pages are published. Keep `docs/` as the source of truth, and update the sidebar when a page should appear in that navigation.

```sh
# Requires Node.js 24.22.1.
npm ci
npm run build
npm run check
```

GitHub Pages deploys only after a push to `main`. Pull requests build and check the site without publishing it. The repository's Pages source must be set to GitHub Actions, and DNS for `docs.agentscm.dev` must point to GitHub Pages before the first live deployment.
