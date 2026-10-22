import js from '@eslint/js'
import globals from 'globals'
import reactHooks from 'eslint-plugin-react-hooks'
import tseslint from 'typescript-eslint'

export default tseslint.config(
  { ignores: ['dist', 'node_modules', 'test-results', 'playwright-report'] },
  js.configs.recommended,
  tseslint.configs.recommended,
  {
    files: ['src/**/*.{ts,tsx}'],
    languageOptions: { globals: globals.browser },
    extends: [reactHooks.configs.flat.recommended],
  },
  {
    files: ['tests/**/*.{ts,tsx}'],
    languageOptions: { globals: { ...globals.browser, ...globals.node } },
    extends: [reactHooks.configs.flat.recommended],
  },
  {
    files: ['scripts/**/*.mjs', 'dev/**/*.ts', 'vite.config.ts', 'vitest.config.ts', 'playwright.config.ts'],
    languageOptions: { globals: globals.node },
  },
)
