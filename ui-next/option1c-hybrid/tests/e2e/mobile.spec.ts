// Phone-sized flows: the rail is a drawer, the header collapses, and nothing scrolls sideways.
import { expect, test, type Page } from '@playwright/test'
import { READ_TOKEN, hubReachable } from '../hub.ts'

test.beforeAll(async () => {
  test.skip(!(await hubReachable()), 'Start the dev hub first: ui-next/dev-seed/start.sh (127.0.0.1:7332).')
})

const rail = (page: Page) => page.getByRole('complementary', { name: 'Hub sections' })
const crumbs = (page: Page) => page.getByRole('navigation', { name: 'Breadcrumb' })
const noSideScroll = (page: Page) => page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1)

async function signIn(page: Page) {
  await page.goto(`/#token=${READ_TOKEN}`)
  await expect(page.getByRole('button', { name: /Account and settings for maya-chen/ })).toBeVisible()
}

test('the drawer opens from the menu button, navigates, and closes', async ({ page }) => {
  await signIn(page)
  await expect(rail(page)).toBeHidden()
  await page.getByRole('button', { name: 'Open navigation' }).tap()
  await expect(rail(page)).toBeVisible()
  await rail(page).getByRole('link', { name: /^Work(?= )/ }).tap()
  await expect(rail(page)).toBeHidden()
  await expect(crumbs(page).locator('[aria-current="page"]')).toContainText('Working now')
  await expect(page.locator('#main')).toContainText('Rebuild the hub status page shell')
  expect(await noSideScroll(page)).toBe(true)
})

test('tapping the dimmed page closes the drawer', async ({ page }) => {
  await signIn(page)
  await page.getByRole('button', { name: 'Open navigation' }).tap()
  await expect(rail(page)).toBeVisible()
  const box = page.viewportSize()!
  await page.touchscreen.tap(box.width - 10, box.height / 2)
  await expect(rail(page)).toBeHidden()
})

test('every page fits the phone width', async ({ page }) => {
  await signIn(page)
  for (const hash of ['#now', '#work', '#work/coordination', '#activity', '#workflows', '#workspace', '#setup', '#health', '#settings']) {
    await page.evaluate((h) => (window.location.hash = h), hash)
    await expect(page.locator('#main')).not.toBeEmpty()
    expect(await noSideScroll(page), hash).toBe(true)
  }
})

test('the narrow header keeps "Degraded" in words', async ({ page }) => {
  await signIn(page)
  await expect(page.locator('.health-chip .health-text')).toBeHidden()
  await page.route('**/v1/hub/health', (r) => r.fulfill({ status: 503, contentType: 'application/json', body: '{"status":"unavailable"}' }))
  await page.reload()
  await expect(page.locator('.health-chip')).toContainText('Degraded', { timeout: 20_000 })
  await expect(page.locator('.health-chip .health-text')).toBeVisible()
})

test('search opens from the header button and finds a work item', async ({ page }) => {
  await signIn(page)
  await page.getByRole('button', { name: 'Search the hub' }).tap()
  const input = page.getByRole('combobox', { name: 'Search' })
  await expect(input).toBeFocused()
  await input.fill('Retention migration')
  await page.getByRole('option', { name: /^Retention migration for PostgreSQL\s*gemini-pax/ }).tap()
  await expect(crumbs(page).locator('[aria-current="page"]')).toHaveText(/Retention migration for PostgreSQL/)
})
