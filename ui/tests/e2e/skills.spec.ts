// Runs against the synthetic mock hub, not a live one:
//   MOCK=1 AGENTSCM_UI_PORT=5197 npx playwright test skills.spec.ts skills.mobile.spec.ts --workers=1
import { expect, test, type Page } from '@playwright/test'

test.beforeEach(async ({ page, baseURL }) => {
  const mock = await fetch(`${baseURL}/v1/hub/skills`, { headers: { Authorization: 'Bearer read-token' } }).catch(() => null)
  test.skip(!mock?.ok, 'Start the mock hub: MOCK=1 npx vite (see the header of this file).')
  await page.goto('/#token=read-token')
  await expect(page.getByRole('complementary', { name: 'Hub sections' })).toBeVisible()
})

const go = async (page: Page, hash: string) => {
  await page.evaluate((h) => (window.location.hash = h), hash)
}
const noSideScroll = (page: Page) => page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1)

test('library shows adoption source, applicability, commands and what needs attention', async ({ page }) => {
  await go(page, 'skills')
  const main = page.locator('#main')
  await expect(main).toContainText('Which workflows do agents in this repository have to follow?')
  await expect(main).toContainText('Adoption: read from main on the forge')
  await expect(main.getByText('When to use').first()).toBeVisible()
  await expect(main.getByText('Prerequisites').first()).toBeVisible()
  await expect(main.getByText('Not for').first()).toBeVisible()
  await expect(main.getByText('How it works').first()).toBeVisible()
  await expect(main.getByText('Full procedure').first()).toBeVisible()
  await expect(main.getByText(/The author runs the tests on the exact head commit\./).first()).toBeVisible()
  await expect(main.getByText('Planned: merge check').first()).toBeVisible()
  const enforced = main.getByText(/^Enforced:/)
  await expect(enforced.first()).toHaveText(/^Enforced: Stop hook$/)
  for (const el of await enforced.all()) await expect(el).not.toContainText(/forge status/i)
  await expect(main).toContainText('agentscm skill add post-merge-check')
  await expect(main).toContainText('agentscm skill remove pr-gate')
  await expect(main).toContainText('committed')
  await expect(main.getByText('Unknown version')).toBeVisible()
  await main.getByRole('link', { name: /View runs/ }).click()
  await expect(page).toHaveURL(/#skills\/runs$/)
})

test('runs list leads with the next action and labels self-reported gates', async ({ page }) => {
  await go(page, 'skills/runs')
  const main = page.locator('#main')
  await expect(main.getByText('At review')).toBeVisible()
  await expect(main.getByText('Needs an independent review of a1b2c3d by another session.')).toBeVisible()
  await expect(main.getByText('Self-reported').first()).toBeVisible()
  await expect(main.getByText('Waiting on ci')).toBeVisible()
  await main.getByRole('tab', { name: /Completed/ }).click()
  await expect(main.getByText('Completed', { exact: true }).last()).toBeVisible()
  await expect(main.getByText('chore/tighten-upload-limits')).toBeVisible()
  await main.getByRole('tab', { name: /Open/ }).click()
  await main.getByRole('textbox', { name: 'Search runs on this page' }).fill('cache')
  await expect(main.getByText('fix/cache-parsed-manifests')).toBeVisible()
  await expect(main.getByText('feat/sync-retry-budget')).toHaveCount(0)
})

test('run detail shows the gate table with a visible Self-reported assurance', async ({ page }) => {
  await go(page, 'skills/runs')
  await page.locator('#main').getByRole('link', { name: /feat\/sync-retry-budget/ }).click()
  const main = page.locator('#main')
  await expect(main.getByRole('heading', { name: 'Blocking finish?' })).toBeVisible()
  await expect(main).toContainText('Yes')
  await expect(main).toContainText('it blocks the author at tests, review and fix')
  const table = main.getByRole('table')
  await expect(table.getByRole('columnheader')).toHaveText(['Gate', 'State', 'Evidence', 'Assurance'])
  await expect(table.getByText('Self-reported').first()).toBeVisible()
  await expect(table.getByText('Observed')).toBeVisible()
  await expect(main.getByText('Stale evidence')).toBeVisible()
  await main.getByText('Technical details').click()
  await expect(main).toContainText('run_41')
  expect(await noSideScroll(page)).toBe(true)
})
