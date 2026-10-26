// Both densities on the parts most likely to break: tours, pickers and lanes.
// MOCK=1 AGENTSCM_UI_PORT=<free port> npx playwright test density.tour
import { expect, test, type Page } from '@playwright/test'

test.beforeAll(async ({ request }) => {
  const up = await request.get('/__mock/up').catch(() => null)
  test.skip(!up?.ok(), 'Needs the mock hub: run with MOCK=1 so vite serves it.')
})

const dialog = (page: Page) => page.getByRole('dialog')
const count = (page: Page) => dialog(page).locator('.tour-count')
const TOURS_OFF = JSON.stringify({ welcomeSeen: true, welcomeOff: true, autoplay: [], sectionsSeen: [] })

const signIn = async (page: Page, hash = '') => {
  await page.goto('/#token=mock')
  if (hash) await page.evaluate((h) => (window.location.hash = h), hash)
  await expect(page.locator('[data-tour="live"]')).toBeVisible()
}
const noSideScroll = (page: Page) => page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1)

/** The dialog sits inside the viewport once its pop-in has ended. */
const withinViewport = async (page: Page) => {
  const vp = page.viewportSize()!
  await expect
    .poll(async () => {
      const b = (await dialog(page).boundingBox())!
      return b.x >= 0 && b.y >= 0 && b.x + b.width <= vp.width + 0.5 && b.y + b.height <= vp.height + 0.5
    })
    .toBe(true)
}

/** The spotlight frame covers some tour target on the page (it is that target plus padding). */
const holeOverTarget = (page: Page) =>
  expect
    .poll(() =>
      page.evaluate(() => {
        const hole = document.querySelector('.tour-hole')?.getBoundingClientRect()
        if (!hole || hole.width === 0) return false
        return [...document.querySelectorAll('[data-tour]')].some((el) => {
          if (el.closest('.tour-layer')) return false
          const r = el.getBoundingClientRect()
          const w = Math.min(r.right, hole.right) - Math.max(r.left, hole.left)
          const h = Math.min(r.bottom, hole.bottom) - Math.max(r.top, hole.top)
          return w > 0 && h > 0 && (w * h) / Math.min(r.width * r.height, hole.width * hole.height) >= 0.5
        })
      }),
    )
    .toBe(true)

/** Every step: dialog inside the viewport, Next (and Back) reachable, spotlight on its target when there is one. */
async function walk(page: Page) {
  const total = Number((await count(page).textContent())!.split('/')[1])
  for (let n = 1; n <= total; n++) {
    await expect(count(page)).toHaveText(`${n} / ${total}`)
    await withinViewport(page)
    const next = dialog(page).locator('.tour-primary')
    await expect(next).toBeInViewport({ ratio: 0.99 })
    if (n > 1) await expect(dialog(page).getByRole('button', { name: /Back/ })).toBeInViewport({ ratio: 0.99 })
    if (await page.locator('.tour-hole').isVisible()) await holeOverTarget(page)
    await next.click()
  }
  await expect(dialog(page)).toBeHidden()
}

for (const density of ['spacious', 'condensed'] as const) {
  test.describe(`${density} density`, () => {
    test.beforeEach(async ({ page }) => {
      await page.addInitScript(
        ([d, off]) => {
          localStorage.setItem('agentscm.density', d!)
          if (!localStorage.getItem('agentscm.tour.v1') && !location.hash.includes('welcome')) localStorage.setItem('agentscm.tour.v1', off!)
        },
        [density, TOURS_OFF],
      )
    })

    test('the welcome tour stays on screen and reachable', async ({ page }) => {
      // The welcome tour only starts for a viewer who has not seen it.
      await page.addInitScript(() => localStorage.removeItem('agentscm.tour.v1'))
      await signIn(page)
      await expect(page.locator('html')).toHaveAttribute('data-density', density)
      await expect(dialog(page)).toBeVisible()
      await walk(page)
    })

    for (const [hash, name] of [['#home', 'Dashboard › Now'], ['#now', 'Dashboard › Where'], ['#work/finished', 'History']] as const) {
      test(`the ${name} page tour stays on screen with its spotlight on target`, async ({ page }) => {
        await signIn(page, hash)
        await page.getByRole('button', { name: 'Tour this page' }).click()
        await expect(dialog(page)).toBeVisible()
        await walk(page)
      })
    }

    test('picker popups stay within their combobox, clear of the nav', async ({ page }) => {
      await signIn(page)
      for (const [hash, label] of [['#templates', 'Change ID'], ['#workflows', 'Find a run']] as const) {
        await page.evaluate((h) => (window.location.hash = h), hash)
        const picker = page.getByRole('combobox', { name: label })
        await picker.focus()
        await picker.press('ArrowDown')
        await expect(picker).toHaveAttribute('aria-expanded', 'true')
        const pop = await page.locator('.combo-pop').first().boundingBox()
        const combo = await picker.evaluate((el) => {
          const r = el.parentElement!.getBoundingClientRect()
          return { left: r.left, right: r.right }
        })
        expect(pop).not.toBeNull()
        expect(pop!.x).toBeGreaterThanOrEqual(combo.left - 1)
        expect(pop!.x + pop!.width).toBeLessThanOrEqual(combo.right + 1)
        const nav = page.locator('[data-tour="nav"]')
        if ((await nav.isVisible()) && page.viewportSize()!.width > 960) {
          const n = (await nav.boundingBox())!
          expect(pop!.x).toBeGreaterThanOrEqual(n.x + n.width - 1)
        }
        expect(await noSideScroll(page)).toBe(true)
        await picker.press('Escape')
      }
    })

    test('the track shows without sideways scroll', async ({ page }) => {
      await signIn(page, '#home')
      await expect(page.locator('[data-tour="work-lanes"]')).toBeVisible()
      await expect(page.locator('.lb:visible').first()).toBeVisible()
      expect(await noSideScroll(page)).toBe(true)
    })

    test('after switching density in Settings, the page tour spotlight still matches its target', async ({ page }) => {
      const other = density === 'spacious' ? 'Condensed' : 'Spacious'
      await signIn(page, '#settings')
      await page.getByRole('radio', { name: new RegExp(`^${other}`) }).click()
      await expect(page.locator('html')).toHaveAttribute('data-density', other.toLowerCase())
      await page.getByRole('button', { name: 'Tour this page' }).click()
      await expect(dialog(page)).toBeVisible()
      await walk(page)
    })
  })
}
