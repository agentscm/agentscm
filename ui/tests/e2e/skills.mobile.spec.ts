// Phone-sized Skills pages against the synthetic mock hub (see skills.spec.ts for how to run).
import { expect, test } from '@playwright/test'

test.beforeEach(async ({ page, baseURL }) => {
  const mock = await fetch(`${baseURL}/v1/hub/skills`, { headers: { Authorization: 'Bearer read-token' } }).catch(() => null)
  test.skip(!mock?.ok, 'Start the mock hub: MOCK=1 npx vite')
  await page.goto('/#token=read-token')
  await expect(page.locator('#main')).toBeVisible()
})

for (const hash of ['skills', 'skills/runs', 'skills/runs/run_41']) {
  test(`#${hash} fits a phone without sideways scrolling`, async ({ page }) => {
    await page.evaluate((h) => (window.location.hash = h), hash)
    await expect(page.locator('#main').getByText(/Self-reported|pr-gate/).first()).toBeVisible()
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1)).toBe(true)
  })
}
