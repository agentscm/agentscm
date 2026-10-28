// Box anchors against the synthetic mock hub (MOCK=1 npx vite): `#box-…` opens the page that holds the
// box and scrolls to it, whether it is opened directly or followed from another page.
import { expect, test } from '@playwright/test'

test.beforeEach(async ({ page, baseURL }) => {
  const mock = await fetch(`${baseURL}/v1/hub/skills`, { headers: { Authorization: 'Bearer read-token' } }).catch(() => null)
  test.skip(!mock?.ok, 'Start the mock hub: MOCK=1 npx vite')
  await page.goto('/#token=read-token')
  await expect(page.locator('#main')).toBeVisible()
})

const tab = (page: import('@playwright/test').Page) => page.locator('.htabs [aria-current="page"]')

test('opening #box-contested directly lands on Where with the box in view', async ({ page }) => {
  await page.evaluate(() => (window.location.hash = 'box-contested'))
  await page.reload()
  await expect(tab(page)).toContainText('Where')
  await expect(page.locator('#box-contested')).toBeInViewport()
})

test('see the contested file, from Now, opens Where at the box', async ({ page }) => {
  await page.locator('#box-needs').getByRole('link', { name: 'see the contested file' }).first().click()
  await expect(tab(page)).toContainText('Where')
  await expect(page.locator('#box-contested')).toBeInViewport()
})
