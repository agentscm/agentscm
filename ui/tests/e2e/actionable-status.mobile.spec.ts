import { expect, test } from '@playwright/test'
import { readFileSync } from 'node:fs'
const fixture = JSON.parse(readFileSync(new URL('../../../src/hub/fixtures/actionable-status.json', import.meta.url), 'utf8')) as { work: { title: string; recipient: { login: string } }[] }

// No live Hub/forge: the existing mock serves other pages; this exact frozen response drives
// personal status on both shipped layouts. Chromium device emulation is not native-app proof.
test('shared status stays readable on desktop, phone and 200 percent zoom', async ({ page }, testInfo) => {
  const status = structuredClone(fixture)
  status.work[2]!.title = 'Review a long synthetic handover title with a very long repository description'
  status.work[2]!.recipient.login = 'synthetic-reviewer-with-a-long-display-name'
  await page.route('**/v1/hub/standup/status', (route) => route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(status) }))
  await page.goto('/#token=read-token')
  await expect(page.locator('#main')).toBeVisible()
  for (const layout of ['classic', 'g']) {
    await page.evaluate((layout) => {
      localStorage.setItem('agentscm.layout', layout)
    }, layout)
    // Use the explicit layout route through the shipped layout preference.
    if (layout === 'g') await page.goto('/#g/home')
    else await page.goto('/#home')
    for (const width of [1360, 390]) {
      await page.setViewportSize({ width, height: 900 })
      await expect(page.locator('#box-needs')).toBeVisible()
      await expect(page.locator('.answer-text')).toContainText('3 actions need you')
      const evidence = page.locator('#box-needs').getByText(/All shared status evidence/)
      await evidence.click()
      for (const zoom of ['1', '2']) {
        await page.evaluate((zoom) => { document.documentElement.style.zoom = zoom }, zoom)
        await expect(page.locator('#box-needs')).toContainText('Awaiting a named party')
        const overflow = await page.evaluate(() => ({ width: window.innerWidth, scroll: document.documentElement.scrollWidth }))
        if (width === 390 && zoom === '1') {
          const column = await page.locator('#box-needs .attn-main').first().boundingBox()
          expect(column?.width).toBeGreaterThan(150)
          await page.evaluate(() => window.scrollTo(0, 0))
          await page.screenshot({ path: testInfo.outputPath(`${layout}-phone.png`), fullPage: false, animations: 'disabled' })
        }
        expect(overflow.scroll, `${layout} ${width}px zoom ${zoom}`).toBeLessThanOrEqual(overflow.width + 1)
      }
      await page.evaluate(() => { document.documentElement.style.zoom = '1' })
      await evidence.click()
    }
  }
})
