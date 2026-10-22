import { defineConfig } from 'vite'
import { mockHub } from '../../ui/dev/mock.ts'

const MOCK = process.env['MOCK']
const mock = MOCK === '1' || MOCK === 'empty' || MOCK === 'down' || MOCK === 'demo'

export default defineConfig({
  base: './',
  plugins: mock ? [mockHub(MOCK === 'empty' || MOCK === 'down' || MOCK === 'demo' ? MOCK : 'full')] : [],
  // The shared data layer lives in ../../ui/src; keep one React instance.
  resolve: { dedupe: ['react', 'react-dom'] },
  server: {
    host: '127.0.0.1',
    port: 5184,
    strictPort: true,
    fs: { allow: ['../..'] },
    proxy: mock ? undefined : { '/v1': { target: process.env['AGENTSCM_HUB'] ?? 'http://127.0.0.1:7332', changeOrigin: true } },
  },
  build: { chunkSizeWarningLimit: 2000 },
})
