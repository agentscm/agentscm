// Phone-sized run rows against the synthetic mock hub (MOCK=1 npx vite). A row's text must keep most of
// the row: its pills and actions wrap below it instead of squeezing it to a column one word wide.
import { expect, test } from '@playwright/test'

test.beforeEach(async ({ page, baseURL }) => {
  const mock = await fetch(`${baseURL}/v1/hub/skills`, { headers: { Authorization: 'Bearer read-token' } }).catch(() => null)
  test.skip(!mock?.ok, 'Start the mock hub: MOCK=1 npx vite')
  await page.goto('/#token=read-token')
  await expect(page.locator('#main')).toBeVisible()
})

for (const [hash, row] of [['landed', '.landed-row'], ['workspace', '.ws-run']] as const) {
  test(`#${hash} run rows keep their text readable on a phone`, async ({ page }) => {
    await page.evaluate((h) => (window.location.hash = h), hash)
    await expect(page.locator(row).first()).toBeVisible()
    const ratios = await page.locator(row).evaluateAll((rows) =>
      rows.map((r) => r.querySelector('.run-main')!.getBoundingClientRect().width / r.getBoundingClientRect().width),
    )
    for (const ratio of ratios) expect(ratio).toBeGreaterThan(0.6)
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1)).toBe(true)
  })
}

for (const hash of ['computers', 'settings', 'templates']) {
  test(`#${hash} keeps its open tab in view in the sideways-scrolling strip`, async ({ page }) => {
    await page.evaluate((h) => (window.location.hash = h), hash)
    const open = page.locator('.htabs [aria-current="page"]')
    await expect(open).toBeVisible()
    const { tab, strip } = await open.evaluate((el) => ({ tab: el.getBoundingClientRect().toJSON() as DOMRect, strip: el.parentElement!.getBoundingClientRect().toJSON() as DOMRect }))
    expect(tab.left).toBeGreaterThanOrEqual(strip.left - 1)
    expect(tab.right).toBeLessThanOrEqual(strip.right + 1)
  })
}
