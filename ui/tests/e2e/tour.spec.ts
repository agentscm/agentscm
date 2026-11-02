// The feature walkthrough against the mock hub: MOCK=1 AGENTSCM_UI_PORT=<free port> npx playwright test tour
import { expect, test, type Page } from '@playwright/test'

// Start as a first-time viewer. The introduction is off by default; `introOn` is a viewer who switched it on in Settings.
test.use({ storageState: { cookies: [], origins: [] } })

test.beforeAll(async ({ request }) => {
  const up = await request.get('/__mock/up').catch(() => null)
  // Without MOCK=1, vite's SPA fallback answers this path with index.html, so check the mock's own reply.
  test.skip(!up?.ok() || !(await up.text()).includes('hub is now healthy'), 'Needs the mock hub: run with MOCK=1 so vite serves it.')
})

const dialog = (page: Page) => page.getByRole('dialog')
const title = (page: Page) => dialog(page).getByRole('heading', { level: 2 })
const count = (page: Page) => dialog(page).locator('.tour-count')
const trigger = (page: Page) => page.getByRole('button', { name: 'Tour this page' })
const signIn = async (page: Page, hash = '') => {
  await page.goto(`/#token=mock`)
  if (hash) await page.evaluate((h) => (window.location.hash = h), hash)
  await expect(page.locator('[data-tour="live"]')).toBeVisible()
}
/** The welcome tour no longer opens by itself; ⌘K replays it. */
const welcome = async (page: Page) => {
  await page.keyboard.press('ControlOrMeta+k')
  await page.getByRole('dialog', { name: 'Search the hub' }).locator('input').fill('replay the welcome')
  await page.keyboard.press('Enter')
  await expect(dialog(page)).toBeVisible()
}
/** Before the page loads: the Settings switch is on (kept if the page already saved something, so reloads keep their dismissals). */
const introOn = (page: Page) =>
  page.addInitScript(() => {
    if (!localStorage.getItem('agentscm.tour.v1')) localStorage.setItem('agentscm.tour.v1', JSON.stringify({ welcomeSeen: false, welcomeOff: false, autoplay: [], sectionsSeen: [], helpSeen: false }))
  })
const coach = (page: Page) => page.locator('[data-coach]')
const reload = async (page: Page) => {
  await page.reload()
  await expect(page.locator('[data-tour="live"]')).toBeVisible()
}
const stored = (page: Page) => page.evaluate(() => JSON.parse(localStorage.getItem('agentscm.tour.v1') ?? 'null'))
// Polled: the card pops in over ~140ms, and its box is only final once that animation ends.
const withinViewport = async (page: Page) => {
  const vp = page.viewportSize()!
  await expect
    .poll(async () => {
      const b = (await dialog(page).boundingBox())!
      return b.x >= 0 && b.y >= 0 && b.x + b.width <= vp.width + 0.5 && b.y + b.height <= vp.height + 0.5
    })
    .toBe(true)
}

test('first sign-in shows nothing tour-like by default, and the ? button invites a first look', async ({ page, baseURL }) => {
  await page.context().addCookies([{ name: 'mock_new', value: '1', url: baseURL! }]) // the mock's account is an hour old
  await signIn(page)
  await expect(coach(page)).toHaveCount(0)
  await expect(dialog(page)).toHaveCount(0)
  await expect(trigger(page)).toHaveClass(/is-pulsing/)
  await trigger(page).click()
  await expect(dialog(page)).toBeVisible()
  expect(await stored(page)).toMatchObject({ helpSeen: true })
  await page.keyboard.press('Escape')
  await reload(page)
  await expect(trigger(page)).not.toHaveClass(/is-pulsing/) // answered in this browser
})

test('an established account never sees the pulse', async ({ page }) => {
  await signIn(page)
  await expect(trigger(page)).not.toHaveClass(/is-pulsing/)
})

test('with the introduction switched on, first sign-in shows three coach marks in the page, and no modal', async ({ page }) => {
  await introOn(page)
  await signIn(page)
  await expect(coach(page)).toHaveCount(3)
  await expect(dialog(page)).toHaveCount(0)
  // Nothing is blocked: the page behind them still works.
  await page.locator('.need-chev').first().click()
  await expect(page.locator('.need-chev').first()).toHaveAttribute('aria-expanded', 'true')
})

test('a coach mark is dismissed on its own, and Skip tips ends the set; both stay gone after a reload', async ({ page }) => {
  await introOn(page)
  await signIn(page)
  await coach(page).first().getByRole('button', { name: 'Got it' }).click()
  await expect(coach(page)).toHaveCount(2)
  await reload(page)
  await expect(coach(page)).toHaveCount(2)
  await coach(page).first().getByRole('button', { name: 'Skip tips' }).click()
  await expect(coach(page)).toHaveCount(0)
  expect(await stored(page)).toMatchObject({ welcomeSeen: true, welcomeOff: true })
  await reload(page)
  await expect(coach(page)).toHaveCount(0)
})

test('the welcome tour: Next, Back and the counter work', async ({ page }) => {
  await signIn(page)
  await welcome(page)
  await expect(dialog(page)).toBeVisible()
  await expect(dialog(page)).toHaveAttribute('aria-modal', 'true')
  await expect(count(page)).toHaveText(/^1 \/ \d+$/)
  const first = await title(page).textContent()
  await dialog(page).getByRole('button', { name: /Next/ }).click()
  await expect(count(page)).toHaveText(/^2 \/ \d+$/)
  await expect(title(page)).not.toHaveText(first!)
  await dialog(page).getByRole('button', { name: /Back/ }).click()
  await expect(count(page)).toHaveText(/^1 \/ \d+$/)
  await expect(title(page)).toHaveText(first!)
  await page.keyboard.press('ArrowRight')
  await expect(count(page)).toHaveText(/^2 \/ \d+$/)
  await expect(page.locator('.tour-hole')).toBeVisible() // step 2 spotlights the answer sentence
})

test('every welcome step stays inside the viewport with reachable buttons', async ({ page }) => {
  await signIn(page)
  await welcome(page)
  const total = Number((await count(page).textContent())!.split('/')[1])
  for (let n = 1; n <= total; n++) {
    await expect(count(page)).toHaveText(`${n} / ${total}`)
    await withinViewport(page)
    const next = dialog(page).locator('.tour-primary')
    await expect(next).toBeInViewport({ ratio: 0.99 })
    if (n > 1) await expect(dialog(page).getByRole('button', { name: /Back/ })).toBeInViewport({ ratio: 0.99 })
    await next.click() // Next, then Done on the last step
  }
  await expect(dialog(page)).toBeHidden()
})

test('Esc closes the tour and focus returns to the page', async ({ page }) => {
  await signIn(page)
  await welcome(page)
  await page.keyboard.press('Escape')
  await expect(dialog(page)).toBeHidden()
  await expect(page.locator('.tour-layer')).toHaveCount(0)
  await expect(dialog(page)).toHaveCount(0)
  expect(await page.evaluate(() => !document.activeElement?.closest('.tour-layer'))).toBe(true)
})

test('"Don’t show again" on the welcome tour persists across a reload, and takes the coach marks with it', async ({ page }) => {
  await introOn(page)
  await signIn(page)
  await welcome(page)
  await dialog(page).getByRole('checkbox', { name: 'Don’t show again' }).check()
  await dialog(page).getByRole('button', { name: 'Skip' }).click()
  await expect(dialog(page)).toBeHidden()
  expect(await stored(page)).toMatchObject({ welcomeSeen: true, welcomeOff: true })
  await reload(page)
  await expect(dialog(page)).toHaveCount(0)
  await expect(coach(page)).toHaveCount(0)
})

test('Skip on the welcome tour without the checkbox leaves the coach marks for the next visit', async ({ page }) => {
  await introOn(page)
  await signIn(page)
  await welcome(page)
  await dialog(page).getByRole('button', { name: 'Skip' }).click()
  await expect(dialog(page)).toHaveCount(0)
  await reload(page)
  await expect(coach(page)).toHaveCount(3)
})

test('the Settings switch is off by default, and turning it on brings the coach marks after a reload', async ({ page }) => {
  await signIn(page)
  await expect(coach(page)).toHaveCount(0)
  await page.evaluate(() => (window.location.hash = 'settings'))
  const sw = page.getByRole('switch', { name: 'Show the welcome tour next time' })
  await expect(sw).not.toBeChecked() // skipping and the switch are the same setting
  await sw.check()
  expect(await stored(page)).toMatchObject({ welcomeSeen: false, welcomeOff: false })
  await reload(page)
  await expect(coach(page)).toHaveCount(3)
})

test('the Tour this page trigger runs the Now tour', async ({ page }) => {
  await signIn(page)
  await page.keyboard.press('Escape')
  await trigger(page).click()
  await expect(dialog(page)).toBeVisible()
  await expect(title(page)).toHaveText('The answer comes first')
  await expect(page.locator('.tour-hole')).toBeVisible()
  await expect(dialog(page).getByRole('checkbox', { name: 'Don’t show again' })).toHaveCount(0)
  await withinViewport(page)
  await dialog(page).getByRole('button', { name: /Next/ }).click()
  await expect(title(page)).toHaveText('Four tabs, one question each')
  await page.keyboard.press('Escape')
  await expect(dialog(page)).toHaveCount(0)
})

test('the Skills tour spotlights the live page and quotes its plain-English steps', async ({ page }) => {
  await signIn(page)
  await page.keyboard.press('Escape')
  await page.evaluate(() => (window.location.hash = 'skills'))
  await expect(page.locator('[data-tour="skills-how"]').first()).toBeVisible()
  await trigger(page).click()
  await expect(title(page)).toHaveText('Which workflows agents are expected to follow')
  for (const [name, live] of [['Adopted skills', /adopted here/], ['What can stop an agent, and what is planned', /Can stop the agent \(opt-in\)/], ['How it works, in plain English', /Step 1 says: “.+”/], ['Adopt or remove with a command', /Copy this: agentscm skill (add|remove) /]] as const) {
    await page.keyboard.press('ArrowRight')
    await expect(title(page)).toHaveText(name)
    await expect(page.locator('.tour-hole')).toBeVisible()
    // The tip quotes the page's own text (from the hub), so the tour never keeps a copy of it.
    await expect(dialog(page).locator('.tour-tip.is-live')).toContainText(live)
    await withinViewport(page)
  }
})

test('⌘K offers Walk me through this page and Replay the welcome tour', async ({ page }) => {
  await signIn(page)
  await page.keyboard.press('Escape')
  await page.keyboard.press('ControlOrMeta+k')
  const search = page.getByRole('dialog', { name: 'Search the hub' }).locator('input')
  await search.fill('walk me')
  await page.keyboard.press('Enter')
  await expect(title(page)).toHaveText('The answer comes first')
  await page.keyboard.press('Escape')
  await page.keyboard.press('ControlOrMeta+k')
  await search.fill('replay the welcome')
  await page.keyboard.press('Enter')
  await expect(dialog(page)).toBeVisible()
  await expect(dialog(page).locator('.tour-kicker')).toHaveText('Welcome to the AgentSCM Hub')
})

test('a Settings "Run now" starts that page’s tour', async ({ page }) => {
  await signIn(page)
  await page.keyboard.press('Escape')
  await page.evaluate(() => (window.location.hash = 'settings'))
  await page.getByText(/^Page tours ·/).click()
  await page.getByRole('button', { name: 'Run the Now tour now' }).click()
  await expect(title(page)).toHaveText('The answer comes first')
  await expect(page.locator('[data-tour="answer"]')).toBeVisible() // it took us to the page
})

test('a page tour switched on in Settings plays until finished or turned off', async ({ page }) => {
  await signIn(page)
  await page.evaluate(() => (window.location.hash = 'settings'))
  await page.getByText(/^Page tours ·/).click()
  await page.getByRole('switch', { name: 'Play the Now tour on my next visit' }).check()
  await page.evaluate(() => (window.location.hash = 'home'))
  await expect(title(page)).toHaveText('The answer comes first')
  await page.keyboard.press('Escape') // closed without the box: it plays again next visit
  await reload(page)
  await expect(title(page)).toHaveText('The answer comes first')
  await dialog(page).getByRole('checkbox', { name: 'Don’t show again' }).check()
  await page.keyboard.press('Escape')
  await reload(page)
  await expect(dialog(page)).toHaveCount(0)
  await page.evaluate(() => (window.location.hash = 'settings'))
  await page.getByText(/^Page tours ·/).click()
  await expect(page.getByRole('switch', { name: 'Play the Now tour on my next visit' })).not.toBeChecked()
})

test('leaving the page closes its tour, so old copy never spotlights a new page', async ({ page }) => {
  await signIn(page)
  await page.keyboard.press('Escape')
  await trigger(page).click()
  await expect(title(page)).toHaveText('The answer comes first')
  await page.evaluate(() => (window.location.hash = 'health'))
  await expect(dialog(page)).toHaveCount(0)
  await page.goBack()
  await expect(page.locator('[data-tour="needs"]')).toBeVisible()
  await expect(dialog(page)).toHaveCount(0)
})

test('after Settings “Run now” and closing, focus lands on the page', async ({ page }) => {
  await signIn(page)
  await page.keyboard.press('Escape')
  await page.evaluate(() => (window.location.hash = 'settings'))
  await page.getByText(/^Page tours ·/).click()
  await page.getByRole('button', { name: 'Run the Hub tour now' }).click()
  await expect(dialog(page)).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(dialog(page)).toHaveCount(0)
  // Polled: focus returns on the frame after the tour layer unmounts.
  await expect.poll(() => page.evaluate(() => document.activeElement?.id)).toBe('main')
})

test.describe('storage blocked', () => {
  test.afterEach(async ({ request }) => void (await request.get('/__mock/reset')))
  test('the page loads, no tour auto-opens, and the trigger still works', async ({ page }) => {
    const errors: string[] = []
    page.on('pageerror', (e) => errors.push(e.message))
    await page.addInitScript(() => {
      for (const k of ['localStorage', 'sessionStorage'] as const) {
        Object.defineProperty(window, k, { configurable: true, get() { throw new DOMException('blocked', 'SecurityError') } })
      }
    })
    // A #token= link cannot be adopted without storage, so sign in with the form; the token lives in memory.
    // Sign the mock's browser session out first, or the page opens signed in and never shows the form.
    await page.request.get('/__mock/signout')
    await page.goto('/')
    await page.getByLabel('Read-only link or token').fill('mock')
    await page.getByRole('button', { name: /Open repository/ }).click()
    await expect(page.locator('[data-tour="live"]')).toBeVisible()
    await page.waitForTimeout(1200)
    await expect(dialog(page)).toHaveCount(0)
    await trigger(page).click()
    await expect(dialog(page)).toBeVisible()
    await page.keyboard.press('Escape')
    await expect(dialog(page)).toHaveCount(0)
    expect(errors).toEqual([])
  })
})
