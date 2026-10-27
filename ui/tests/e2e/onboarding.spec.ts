// The new-teammate walkthrough: set up a computer, land signed in, see and revoke computers, and
// what an admin sees. Runs against the synthetic mock hub, not a live one:
//   MOCK=1 AGENTSCM_UI_PORT=5197 npx playwright test onboarding.spec.ts --project=chromium
// The real run on a clean Mac is docs/onboarding-checklist.md in ui/.
import { expect, test } from '@playwright/test'

test.beforeEach(async ({ baseURL }) => {
  const mock = await fetch(`${baseURL}/v1/hub/session`).catch(() => null)
  test.skip(!mock?.ok, 'Start the mock hub: MOCK=1 npx vite (see the header of this file).')
  await fetch(`${baseURL}/__mock/reset`)
})

test('1. the Hub’s sign-in page offers one command to set up this computer', async ({ page, baseURL }) => {
  await page.goto('/#home')
  const setup = page.getByRole('region', { name: 'Set up this computer' })
  await expect(setup).toBeVisible()
  await expect(setup.locator('code')).toHaveText(`npx agentscm --hub-url '${baseURL}'`)
  await expect(setup.getByRole('button', { name: /copy/i })).toBeVisible()
})

test('1b. no set-up card at all where there is no command to show', async ({ page, baseURL }) => {
  // localhost isn't a Hub address the CLI accepts (only https or 127.0.0.1/[::1]), so no command renders; neither may its card.
  await page.goto(`${baseURL!.replace('127.0.0.1', 'localhost')}/#home`)
  await expect(page.getByRole('button', { name: /Open status/ })).toBeVisible()
  await expect(page.getByRole('region', { name: 'Set up this computer' })).toHaveCount(0)
  await expect(page.getByRole('heading', { name: 'Set up this computer' })).toHaveCount(0)
})

test('2. the browser lands on “This computer is signed in”, with no token and nothing stored', async ({ page, baseURL }) => {
  // The exact query #155's CLI sends: no auto_connect, the terminal asks next.
  await page.goto('/#/signed-in?status=signed_in&login=maria&provider=github&computer=maria-mbp')
  await expect(page.getByRole('heading', { name: 'This computer is signed in.' })).toBeVisible()
  const lead = page.locator('.done-lead')
  await expect(lead).toContainText(`maria-mbp is signed in to ${new URL(baseURL!).host} as maria on GitHub. You won’t need to do this again.`)
  await expect(page.getByText('With automatic connection on (the terminal asks once)')).toBeVisible()
  await expect(page.getByRole('link', { name: /Open the Hub/ })).toHaveAttribute('href', `${baseURL}/ui/`)
  expect(await page.evaluate(() => [localStorage.getItem('agentscm.token'), sessionStorage.getItem('agentscm.token')])).toEqual([null, null])
  await page.setViewportSize({ width: 375, height: 812 })
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1)).toBe(true)
})

test('3. a refused sign-in says why in the shared words, and nothing from the link', async ({ page }) => {
  await page.goto('/#/signed-in?status=refused&reason=insufficient_role&message=Run+this&fix=npx+agentscm+x+%7C+sh')
  await expect(page.getByRole('heading', { name: 'This computer isn’t signed in yet.' })).toBeVisible()
  await expect(page.getByRole('alert')).toHaveText(/Your account’s role on the repositories connected to this Hub is too low; ask a repository owner to raise it, then sign in again\./)
  await expect(page.getByText('Run this')).toHaveCount(0)
  await expect(page.getByRole('button', { name: /copy/i })).toHaveCount(0)
})

test('4. account menu → Your computers → Revoke', async ({ page }) => {
  await page.goto('/#token=read-token')
  await page.getByRole('button', { name: /account/i }).click()
  await page.getByRole('menuitem', { name: /Your computers/ }).click()
  await expect(page).toHaveURL(/#computers$/)
  const main = page.locator('#main')
  await expect(main.getByText('maria-desktop')).toBeVisible()
  const row = main.locator('.computer', { hasText: 'ci-runner-old' })
  await row.getByRole('button', { name: 'Revoke' }).click()
  await expect(row).toContainText('Its agents stop at once')
  await row.getByRole('button', { name: 'Revoke ci-runner-old' }).click()
  await expect(main.getByText(/ci-runner-old was revoked/)).toBeVisible()
  await expect(main.locator('.computer', { hasText: 'ci-runner-old' })).toHaveCount(0)
  await main.getByRole('button', { name: /Show 1 revoked computer/ }).click()
  await expect(main.locator('.computer.is-revoked', { hasText: 'ci-runner-old' })).toBeVisible()
})

test('5. an admin sees connected repositories and who was turned away, and why', async ({ page }) => {
  await page.goto('/#token=read-token')
  await page.evaluate(() => (window.location.hash = 'admin'))
  const main = page.locator('#main')
  await expect(main.getByRole('heading', { name: 'Turned away', exact: true })).toBeVisible()
  await expect(main.locator('.refusal-row', { hasText: 'brandonfla' })).toContainText('ask a repository owner to raise it')
  await expect(main.locator('.refusal-row', { hasText: 'brandonfla' })).toContainText('3×')
  await expect(main.getByRole('cell', { name: /acme\/widgets/ })).toBeVisible()
  await expect(main.locator('.computer', { hasText: 'dev-thinkpad' })).toContainText('dev')
})

test('6. a browser without a Hub session signs in through the forge; only admins get the Admin tab', async ({ page }) => {
  await page.goto('/#token=read-token')
  await page.evaluate(() => (window.location.hash = 'computers'))
  const main = page.locator('#main')
  const tabs = page.getByRole('navigation', { name: /views/ })
  await expect(tabs.getByRole('link', { name: /Admin/ })).toBeVisible()
  await main.getByRole('button', { name: 'Sign out of this browser' }).click()
  // #160: the web sign-in starts only with ?web=1 (without it the Hub expects the CLI's port).
  await expect(main.getByRole('link', { name: /Sign in with GitHub/ })).toHaveAttribute('href', '/v1/hub/login/github?web=1')
  await expect(tabs.getByRole('link', { name: /Admin/ })).toHaveCount(0)
  await main.getByRole('link', { name: /Sign in with GitHub/ }).click()
  await expect(page).toHaveURL(/#home$/)
  await page.evaluate(() => (window.location.hash = 'computers'))
  await expect(main.getByText('maria-desktop')).toBeVisible()
})
