// The new-teammate walkthrough: set up a computer, land signed in, see and revoke computers, and
// what an admin sees. Runs against the synthetic mock hub, not a live one:
//   MOCK=1 AGENTSCM_UI_PORT=5197 npx playwright test onboarding.spec.ts --project=chromium
// The real run on a clean Mac is docs/onboarding-checklist.md in ui/.
import { expect, test } from '@playwright/test'

test.beforeEach(async ({ baseURL }) => {
  // Reset first: an earlier step may have signed the mock browser out.
  await fetch(`${baseURL}/__mock/reset`).catch(() => null)
  const mock = await fetch(`${baseURL}/v1/hub/session`).catch(() => null)
  test.skip(!mock?.ok, 'Start the mock hub: MOCK=1 npx vite (see the header of this file).')
})

test('1. the Hub’s sign-in page offers one command to set up this computer', async ({ page, baseURL }) => {
  // Without a web sign-in: with one, the page opens the Hub instead (step 7).
  await fetch(`${baseURL}/__mock/signout`)
  await page.goto('/#home')
  const setup = page.getByRole('region', { name: 'Set up this computer' })
  await expect(setup).toBeVisible()
  await expect(setup.locator('code')).toHaveText(`npx agentscm --hub-url '${baseURL}'`)
  await expect(setup.getByRole('button', { name: /copy/i })).toBeVisible()
})

test('1b. no set-up card at all where there is no command to show', async ({ page, baseURL }) => {
  // localhost isn't a Hub address the CLI accepts (only https or 127.0.0.1/[::1]), so no command renders; neither may its card.
  await fetch(`${baseURL}/__mock/signout`)
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
  await main.getByRole('button', { name: 'Sign out of Hub pages' }).click()
  // #160: the web sign-in starts only with ?web=1 (without it the Hub expects the CLI's port).
  await expect(main.getByRole('link', { name: /Sign in with GitHub/ })).toHaveAttribute('href', '/v1/hub/login/github?web=1')
  await expect(tabs.getByRole('link', { name: /Admin/ })).toHaveCount(0)
  await main.getByRole('link', { name: /Sign in with GitHub/ }).click()
  await expect(page).toHaveURL(/#home$/)
  await page.evaluate(() => (window.location.hash = 'computers'))
  await expect(main.getByText('maria-desktop')).toBeVisible()
})

test('7. a web sign-in opens the Hub with no pasted token, switches repositories, and stores no token', async ({ page, baseURL }) => {
  await page.goto('/#home')
  const repoButton = page.locator('.repo-btn')
  // The first repository the sign-in may open, sorted: acme/billing-docs.
  await expect(repoButton).toHaveText(/acme\/billing-docs/)
  await expect(page.locator('#main')).toContainText(/working on|Nobody is working/)
  await expect(page.getByRole('button', { name: /Open status/ })).toHaveCount(0)

  await repoButton.click()
  await page.getByRole('menuitem', { name: 'acme/widgets' }).click()
  await expect(repoButton).toHaveText(/acme\/widgets/)

  // A repository the person's role is too low for: the Hub's sentence, and it leaves the list.
  await repoButton.click()
  await page.getByRole('menuitem', { name: 'acme/secret-plans' }).click()
  await repoButton.click()
  await expect(page.getByRole('status').filter({ hasText: 'ask a repository owner for Triage or higher' })).toBeVisible()
  await expect(page.getByRole('menuitem', { name: 'acme/secret-plans' })).toHaveCount(0)
  await expect(repoButton).toHaveText(/acme\/widgets/)

  // One mint to open and one per switch that succeeded; nothing written to browser storage but the repository name.
  expect((await (await fetch(`${baseURL}/v1/hub/__count/read-tokens`)).json()).minted).toBe(2)
  const stored = await page.evaluate(() => JSON.stringify({ ...localStorage }) + JSON.stringify({ ...sessionStorage }))
  expect(stored).not.toContain('webread:')
  expect(stored).toContain('acme/widgets')

  // A reload asks once more, and opens the repository picked last.
  await page.reload()
  await expect(repoButton).toHaveText(/acme\/widgets/)
  expect((await (await fetch(`${baseURL}/v1/hub/__count/read-tokens`)).json()).minted).toBe(3)

  // Signing out of the browser goes back to the token page.
  await page.getByRole('button', { name: /Account and settings/ }).click()
  await page.getByRole('menuitem', { name: /Sign out of Hub pages/ }).click()
  await expect(page.getByRole('button', { name: /Open status/ })).toBeVisible()
})

test('8. Promises stays reachable after Hub sign-out, with Workspace access disclosed', async ({ page }) => {
  await page.goto('/#promises')
  await expect(page.getByRole('link', { name: /Promises/ })).toBeVisible()
  await expect(page.locator('#main').getByRole('heading', { name: 'What must remain true?' })).toBeVisible()
  await expect(page.getByText('You are signed in as synthetic-maintainer.')).toBeVisible()
  await page.getByRole('button', { name: /Account and settings/ }).click()
  const signOut = page.getByRole('menuitem', { name: /Sign out of Hub pages/ })
  await expect(signOut).toContainText('Workspace access and computers stay signed in')
  await signOut.click()
  expect((await page.request.get('/v1/hub/session')).status()).toBe(401)
  expect((await page.request.get('/v1/hub/workspace/session')).status()).toBe(200)
  await expect(page.getByRole('heading', { name: 'What must remain true?' })).toBeVisible()
  await expect(page.getByRole('link', { name: 'Back to Hub' })).toBeVisible()
  await expect(page.locator('nav.htabs')).toHaveCount(0)
  await expect(page.getByText('You are signed in as synthetic-maintainer.')).toBeVisible()
})

test('8b. Computers sign-out clears Hub pages with one DELETE while Workspace stays signed in', async ({ page }) => {
  await page.goto('/#computers')
  const signOut = page.locator('#main').getByRole('button', { name: 'Sign out of Hub pages' })
  await expect(signOut).toBeVisible()
  let deletes = 0
  page.on('request', (request) => {
    if (request.method() === 'DELETE' && new URL(request.url()).pathname === '/v1/hub/session') deletes++
  })
  await signOut.click()
  await expect(page.getByRole('button', { name: /Open status/ })).toBeVisible()
  expect(deletes).toBe(1)
  expect((await page.request.get('/v1/hub/session')).status()).toBe(401)
  expect((await page.request.get('/v1/hub/workspace/session')).status()).toBe(200)
  await page.evaluate(() => { window.location.hash = 'home' })
  await expect(page.getByRole('button', { name: /Open status/ })).toBeVisible()
  await page.evaluate(() => { window.location.hash = 'promises' })
  await expect(page.getByRole('heading', { name: 'What must remain true?' })).toBeVisible()
  await expect(page.getByText('You are signed in as synthetic-maintainer.')).toBeVisible()
})

test('9. Hub repository switch preserves a Workspace Promise draft', async ({ page }) => {
  await page.goto('/#promises')
  const draft = page.getByLabel('Original request').first()
  await expect(draft).toBeVisible()
  await draft.fill('Retain the unfinished owner request.')
  const repo = page.locator('.repo-btn')
  await expect(repo).toHaveText(/acme\/billing-docs/)
  await repo.click()
  await page.getByRole('menuitem', { name: 'acme/widgets' }).click()
  await expect(repo).toHaveText(/acme\/widgets/)
  await expect(draft).toHaveValue('Retain the unfinished owner request.')
  await expect(page.getByText('You are signed in as synthetic-maintainer.')).toBeVisible()
  await page.getByRole('button', { name: /Account and settings/ }).click()
  await page.getByRole('menuitem', { name: /Sign out of Hub pages/ }).click()
  await expect(page.getByRole('link', { name: 'Back to Hub' })).toBeVisible()
  await expect(draft).toHaveValue('Retain the unfinished owner request.')
})

test('10. Hub repository switch preserves a Workspace settings draft and refreshes Hub data', async ({ page }) => {
  await page.goto('/#home')
  await expect(page.locator('#main')).toContainText('Nobody is working')
  await page.evaluate(() => { window.location.hash = 'workspace/settings' })
  const name = page.getByLabel('Workspace name')
  await expect(name).toBeVisible()
  await name.fill('Unfinished workspace name')
  const repo = page.locator('.repo-btn')
  await repo.click()
  await page.getByRole('menuitem', { name: 'acme/widgets' }).click()
  await expect(repo).toHaveText(/acme\/widgets/)
  await expect(name).toHaveValue('Unfinished workspace name')
  await page.evaluate(() => { window.location.hash = 'home' })
  await expect(page.locator('#main')).toContainText('Refactor the billing module')
  await expect(page.locator('#main')).not.toContainText('Nobody is working')
})

test('11. a Hub token 401 renewal preserves a Workspace Promise draft', async ({ page }) => {
  await page.goto('/#promises')
  const draft = page.getByLabel('Original request').first()
  await draft.fill('Retain the request after Hub renewal.')
  await page.request.get('/__mock/reject-next-read')
  await expect.poll(async () => (await (await page.request.get('/v1/hub/__count/read-tokens')).json()).minted).toBe(2)
  await expect(draft).toHaveValue('Retain the request after Hub renewal.')
  await expect(page.getByText('You are signed in as synthetic-maintainer.')).toBeVisible()
})

test('12. failed Hub sign-out keeps its session and a Workspace draft until retry succeeds', async ({ page }) => {
  await page.goto('/#workspace/settings')
  const name = page.getByLabel('Workspace name')
  await expect(name).toBeVisible()
  await name.fill('Keep this draft if Hub sign-out fails.')
  await page.request.get('/__mock/reject-next-signout')
  await page.getByRole('button', { name: /Account and settings/ }).click()
  await page.getByRole('menuitem', { name: /Sign out of Hub pages/ }).click()
  await expect(page.getByRole('alert')).toContainText('This browser may still be signed in to Hub pages')
  expect((await page.request.get('/v1/hub/session')).status()).toBe(200)
  await expect(name).toHaveValue('Keep this draft if Hub sign-out fails.')
  await expect(page.getByRole('button', { name: /Account and settings/ })).toBeVisible()
  await page.getByRole('menuitem', { name: /Sign out of Hub pages/ }).click()
  expect((await page.request.get('/v1/hub/session')).status()).toBe(401)
  await expect(page.getByRole('link', { name: 'Back to Hub' })).toBeVisible()
  await expect(page.locator('nav.htabs')).toHaveCount(0)
  await expect(name).toHaveValue('Keep this draft if Hub sign-out fails.')
})

test('12b. failed Computers sign-out reports the shared Hub warning and permits retry', async ({ page }) => {
  await page.goto('/#computers')
  const signOut = page.locator('#main').getByRole('button', { name: 'Sign out of Hub pages' })
  await expect(signOut).toBeVisible()
  await page.request.get('/__mock/reject-next-signout')
  await signOut.click()
  await expect(page.getByRole('alert').filter({ hasText: 'Hub sign-out failed' })).toBeVisible()
  expect((await page.request.get('/v1/hub/session')).status()).toBe(200)
  await signOut.click()
  await expect(page.getByRole('button', { name: /Open status/ })).toBeVisible()
  expect((await page.request.get('/v1/hub/session')).status()).toBe(401)
})

test('12c. an identified session can retry Hub sign-out while read-token minting is unavailable', async ({ page }) => {
  await page.goto('/#workspace/settings')
  const name = page.getByLabel('Workspace name')
  await expect(name).toBeVisible()
  await name.fill('Keep this draft while Hub is unavailable.')
  await page.request.get('/__mock/reject-next-signout')
  await page.getByRole('button', { name: /Account and settings/ }).click()
  await page.getByRole('menuitem', { name: /Sign out of Hub pages/ }).click()
  await expect(page.getByRole('alert').filter({ hasText: 'Hub sign-out failed' })).toBeVisible()
  await page.route('**/v1/hub/session/read-token', (route) => route.fulfill({ status: 503, body: '{}' }))
  await page.request.get('/__mock/reject-next-read')
  await expect(page.getByRole('button', { name: 'Retry Hub connection' })).toBeVisible()
  const retry = page.getByRole('button', { name: 'Retry Hub sign-out' })
  await expect(retry).toBeVisible()
  await expect(name).toHaveValue('Keep this draft while Hub is unavailable.')
  await retry.click()
  expect((await page.request.get('/v1/hub/session')).status()).toBe(401)
  await expect(name).toHaveValue('Keep this draft while Hub is unavailable.')
})

test('12d. Hub pages offer sign-out retry separately from connection retry', async ({ page }) => {
  await page.goto('/#home')
  await expect(page.locator('.repo-btn')).toBeVisible()
  await page.request.get('/__mock/reject-next-signout')
  await page.getByRole('button', { name: /Account and settings/ }).click()
  await page.getByRole('menuitem', { name: /Sign out of Hub pages/ }).click()
  await expect(page.getByRole('alert').filter({ hasText: 'Hub sign-out failed' })).toBeVisible()
  await page.route('**/v1/hub/session/read-token', (route) => route.fulfill({ status: 503, body: '{}' }))
  await page.request.get('/__mock/reject-next-read')
  await expect(page.getByRole('button', { name: 'Retry Hub connection' })).toBeVisible()
  await page.getByRole('button', { name: 'Retry Hub sign-out' }).click()
  await expect(page.getByRole('button', { name: /Open status/ })).toBeVisible()
  expect((await page.request.get('/v1/hub/session')).status()).toBe(401)
})

test('12e. a live Hub session with no repository keeps sign-out and the token form', async ({ page }) => {
  await page.route('**/v1/hub/tokens', (route) => route.fulfill({ status: 200, contentType: 'application/json', body: '{"tokens":[]}' }))
  await page.route('**/v1/hub/devices', (route) => route.fulfill({ status: 200, contentType: 'application/json', body: '{"devices":[]}' }))
  await page.route('**/v1/hub/admin/repositories', (route) => route.fulfill({ status: 200, contentType: 'application/json', body: '{"repositories":[]}' }))
  await page.goto('/#home')
  await expect(page.getByText('No repository available')).toBeVisible()
  await expect(page.getByText(/has no repository to open yet/)).toBeVisible()
  await expect(page.getByRole('button', { name: 'Open status' })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Open status' })).toBeInViewport()
  await page.getByRole('button', { name: 'Sign out of Hub pages' }).click()
  expect((await page.request.get('/v1/hub/session')).status()).toBe(401)
})

test('12f. an identified refused Hub session keeps its access explanation and token form', async ({ page }) => {
  await page.route('**/v1/hub/tokens', (route) => route.fulfill({ status: 200, contentType: 'application/json', body: '{"tokens":[]}' }))
  await page.route('**/v1/hub/devices', (route) => route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({ devices: [{ id: `dev_${'a'.repeat(32)}`, name: 'synthetic-computer', created_at: 1, last_used_at: null, revoked_at: null, current: false, repositories: ['acme/secret-plans'] }] }) }))
  await page.route('**/v1/hub/session/read-token', (route) => route.fulfill({ status: 403, contentType: 'application/json', body: JSON.stringify({ error: 'insufficient_role', login: 'maria', repository: 'acme/secret-plans', visibility: 'private', has: 'read', needs: 'triage', message: 'maria has Read on private repository acme/secret-plans; ask a repository owner for Triage or higher' }) }))
  await page.goto('/#home')
  await expect(page.getByText('Hub access refused')).toBeVisible()
  await expect(page.getByText(/ask a repository owner for Triage or higher/).first()).toBeVisible()
  await expect(page.getByRole('button', { name: 'Open status' })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Sign out of Hub pages' })).toBeVisible()
})

test('13. forgetting a pasted token preserves an independent Promise draft', async ({ page }) => {
  await page.goto('/#token=read-token')
  await page.evaluate(() => { window.location.hash = 'promises' })
  const draft = page.getByLabel('Original request').first()
  await expect(draft).toBeVisible()
  await draft.fill('Keep this draft after forgetting the pasted token.')
  await page.getByRole('button', { name: /Account and settings/ }).click()
  await page.getByRole('menuitem', { name: /Forget token/ }).click()
  await expect(draft).toHaveValue('Keep this draft after forgetting the pasted token.')
  await expect(page.getByText('You are signed in as synthetic-maintainer.')).toBeVisible()
})

test('14. another tab replacing a refused pasted token preserves a Workspace draft', async ({ page }) => {
  await page.goto('/#token=read-token')
  await page.evaluate(() => { window.location.hash = 'workspace/settings' })
  const name = page.getByLabel('Workspace name')
  await expect(name).toBeVisible()
  await name.fill('Keep this draft after token replacement.')
  await page.evaluate(() => window.localStorage.setItem('agentscm.token', 'replacement-token'))
  const replacement = page.waitForRequest((request) => request.url().includes('/v1/hub/') && request.headers()['authorization'] === 'Bearer replacement-token')
  await page.request.get('/__mock/reject-next-agent-read')
  await replacement
  await expect(name).toHaveValue('Keep this draft after token replacement.')
})
