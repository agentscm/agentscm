import { readFileSync } from 'node:fs'
import { join } from 'node:path'
import { defineConfig, type Plugin } from 'vite'
import { mockHub } from './dev/mock.ts'

/**
 * Writes dist/packages.json: every npm package that contributes code to app.js (read by `npm run notices`).
 * Modules that were imported but fully tree-shaken (rendered length 0) are left out.
 */
function packagesManifest(): Plugin {
  return {
    name: 'agentscm-packages-manifest',
    apply: 'build',
    generateBundle(_options, bundle) {
      const entry = bundle['app.js']
      if (entry?.type !== 'chunk') throw new Error('app.js chunk not found')
      const rendered = new Set(Object.entries(entry.modules).filter(([, m]) => m.renderedLength > 0).map(([id]) => id))
      // Keyed by resolved directory, so nested copies of one package at different versions stay separate.
      const found = new Map<string, { name: string; version: string; dir: string }>()
      const NM = '/node_modules/'
      for (const id of this.getModuleIds()) {
        if (!rendered.has(id)) continue
        const clean = id.split('?')[0]!.replaceAll('\\', '/')
        const first = clean.indexOf(NM)
        const last = clean.lastIndexOf(NM)
        if (first < 0) continue
        const parts = clean.slice(last + NM.length).split('/')
        const name = parts[0]!.startsWith('@') ? `${parts[0]}/${parts[1]}` : parts[0]!
        const end = last + NM.length + name.length
        const dir = clean.slice(first + 1, end) // relative to the project, e.g. node_modules/a/node_modules/b
        if (found.has(dir)) continue
        const version = (JSON.parse(readFileSync(join(clean.slice(0, end), 'package.json'), 'utf8')) as { version: string }).version
        found.set(dir, { name, version, dir })
      }
      const list = [...found.values()].sort((a, b) => a.name.localeCompare(b.name) || a.version.localeCompare(b.version) || a.dir.localeCompare(b.dir))
      this.emitFile({ type: 'asset', fileName: 'packages.json', source: `${JSON.stringify(list, null, 2)}\n` })
    },
  }
}

const MOCK = process.env['MOCK']
const mock = MOCK === '1' || MOCK === 'empty' || MOCK === 'down' || MOCK === 'demo'

export default defineConfig({
  base: './',
  plugins: [packagesManifest(), ...(mock ? [mockHub(MOCK === 'empty' || MOCK === 'down' || MOCK === 'demo' ? MOCK : 'full')] : [])],
  // One React instance for the app and its tests.
  resolve: { dedupe: ['react', 'react-dom'] },
  server: {
    host: '127.0.0.1',
    port: 5184,
    strictPort: true,
    // `agentscm serve` on this machine answers /v1/local (Templates, Timeline); it must come before /v1 (the hub).
    proxy: mock
      ? undefined
      : {
          // No changeOrigin: serve checks for a loopback Host, which the proxy passes through unchanged.
          '/v1/local': { target: process.env['AGENTSCM_SERVE'] ?? 'http://127.0.0.1:7331' },
          '/v1': { target: process.env['AGENTSCM_HUB'] ?? 'http://127.0.0.1:7332', changeOrigin: true },
        },
  },
  // The hub embeds exactly dist/index.html, app.js and app.css. Fonts and images inline as data: URIs
  // because the hub's CSP is font-src 'self' data:.
  build: {
    assetsDir: '',
    assetsInlineLimit: 100_000_000,
    modulePreload: false,
    cssCodeSplit: false,
    chunkSizeWarningLimit: 2000,
    rollupOptions: {
      output: {
        entryFileNames: 'app.js',
        assetFileNames: 'app[extname]',
        inlineDynamicImports: true,
      },
    },
  },
})
