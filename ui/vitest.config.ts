import { defineConfig, mergeConfig } from 'vitest/config'
import viteConfig from './vite.config.ts'

export default mergeConfig(
  viteConfig,
  defineConfig({
    test: {
      environment: 'jsdom',
      environmentOptions: { jsdom: { url: 'http://127.0.0.1:5184/' } },
      include: ['tests/unit/**/*.test.{ts,tsx}', 'tests/integration/**/*.test.{ts,tsx}'],
      setupFiles: ['tests/setup.ts'],
      // Integration tests share one live hub; run files one at a time.
      fileParallelism: false,
      testTimeout: 30_000,
    },
  }),
)
