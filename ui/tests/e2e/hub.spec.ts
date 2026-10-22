import { expect, test, type Page } from '@playwright/test'
import { AGENT_TOKEN, READ_TOKEN, checkpoint, freshHandoff, hubReachable, marker, readWork, startWork } from '../hub.ts'

test.beforeAll(async () => {
  test.skip(!(await hubReachable()), 'Start the dev hub first: ui/dev-seed/start.sh (127.0.0.1:7332).')
})

const rail = (page: Page) => page.getByRole('complementary', { name: 'Hub sections' })
const crumbs = (page: Page) => page.getByRole('navigation', { name: 'Breadcrumb' })
const main = (page: Page) => page.locator('#main')

async function signIn(page: Page, hash = '') {
  await page.goto(`/#token=${READ_TOKEN}`)
  await expect(page.getByRole('button', { name: /Account and settings for maya-chen/ })).toBeVisible()
  if (hash) await page.evaluate((h) => (window.location.hash = h), hash)
}

test('signs in from #token= and strips the token from the address bar', async ({ page }) => {
  await signIn(page)
  expect(page.url()).not.toContain('token=')
  await expect(page.getByRole('button', { name: 'counterbranch/agentscm' })).toBeVisible()
  await expect(rail(page).getByText('Coordinate', { exact: true })).toBeVisible()
  await expect(rail(page).getByText('Verify', { exact: true })).toBeVisible()
  await expect(rail(page).getByText('Manage', { exact: true })).toBeVisible()
  await expect(crumbs(page)).toContainText('counterbranch/agentscm')
  await expect(crumbs(page)).toContainText('Right now')
  await expect(main(page)).toContainText(/agents? (is|are) holding/)
})

test('every section is reachable from the rail with a matching breadcrumb and page', async ({ page }) => {
  await signIn(page)
  const pages: [string, RegExp][] = [
    ['Work', /Who owns which task\?/],
    ['Activity', /What just happened\?|happened in this repository/],
    ['Workflows', /checks|workflow/i],
    ['Workspace', /workspace/i],
    ['Connect agents', /agent/i],
    ['Hub health', /Is the hub OK\?/],
    ['Settings', /How is this page set up for me\?/],
    ['Right now', /Who is editing which files/],
  ]
  for (const [label, heading] of pages) {
    await rail(page).getByRole('link', { name: new RegExp(`^${label}(?= )`) }).click()
    await expect(crumbs(page).locator('[aria-current="page"]')).toContainText(label === 'Work' ? 'Working now' : label === 'Workspace' ? 'Repositories & runs' : label)
    await expect(main(page)).toContainText(heading)
    await expect(rail(page).getByRole('link', { name: new RegExp(`^${label}(?= )`) })).toHaveAttribute('aria-current', 'page')
    // A new section starts at the top with focus on the page.
    expect(await page.evaluate(() => [window.scrollY, document.activeElement?.id])).toEqual([0, 'main'])
  }
})

test('⌘K search opens a work item and the breadcrumb names it', async ({ page }) => {
  await signIn(page)
  await page.keyboard.press('ControlOrMeta+k')
  const dialog = page.getByRole('dialog', { name: 'Search the hub' })
  await expect(dialog).toBeVisible()
  await page.keyboard.type('Rebuild the hub status')
  // The agent's task matches too; pick the Work result.
  await dialog.getByRole('option', { name: /^Rebuild the hub status page shell\s*claude-ivy · active/ }).click()
  await expect(page).toHaveURL(/#work\/wrk_[0-9a-f]{32}$/)
  await expect(crumbs(page).locator('[aria-current="page"]')).toHaveText(/Rebuild the hub status page shell/)
  await expect(main(page)).toContainText('Rebuild the hub status page shell')
})

test('a directed request expands to show targets, receipts and paths', async ({ page }) => {
  await signIn(page, '#work/coordination')
  await expect(crumbs(page)).toContainText('Needs coordination')
  const request = main(page).locator('details.request').first()
  await expect(request).toBeVisible()
  await request.locator('summary').click()
  await expect(request).toHaveAttribute('open', '')
  await expect(request).toContainText('Target lead')
  await expect(request).toContainText(/Awaiting receipt|Received/)
})

test('rail collapse persists across reload, with tooltips naming each icon', async ({ page }) => {
  await signIn(page)
  await page.getByRole('button', { name: 'Collapse sidebar' }).click()
  await expect(rail(page)).toHaveClass(/is-compact/)
  await page.reload()
  await expect(rail(page)).toHaveClass(/is-compact/)
  await expect(rail(page).getByRole('link', { name: 'Work', exact: true })).toBeVisible()
  await page.keyboard.press('[')
  await expect(rail(page)).not.toHaveClass(/is-compact/)
  await page.reload()
  await expect(rail(page)).not.toHaveClass(/is-compact/)
})

test('theme choice in Settings applies and persists', async ({ page }) => {
  await signIn(page, '#settings')
  await main(page).getByRole('radio', { name: /Dark/ }).click()
  await expect(page.locator('html')).toHaveAttribute('data-bs-theme', 'dark')
  await page.reload()
  await expect(page.locator('html')).toHaveAttribute('data-bs-theme', 'dark')
  await main(page).getByRole('radio', { name: /Light/ }).click()
  await expect(page.locator('html')).toHaveAttribute('data-bs-theme', 'light')
  await expect(main(page)).toContainText('status-page')
})

test.describe('keyboard', () => {
  test('skip link jumps to the page', async ({ page }) => {
    await signIn(page)
    await page.locator('.skip-link').focus()
    await expect(page.locator('.skip-link')).toBeInViewport()
    await page.keyboard.press('Enter')
    expect(await page.evaluate(() => document.activeElement?.id)).toBe('main')
    await expect(crumbs(page)).toContainText('Right now') // the hash was not changed
  })

  test('header menus: first item focused, arrows move, Escape returns focus', async ({ page }) => {
    await signIn(page)
    const trigger = page.getByRole('button', { name: 'counterbranch/agentscm' })
    await trigger.focus()
    await page.keyboard.press('Enter')
    const menu = page.getByRole('menu')
    await expect(menu).toBeVisible()
    await expect(menu.getByRole('menuitem', { name: /Open the workspace/ })).toBeFocused()
    await page.keyboard.press('ArrowDown')
    await expect(menu.getByRole('menuitem', { name: /Use a different token/ })).toBeFocused()
    await page.keyboard.press('Escape')
    await expect(menu).toBeHidden()
    await expect(trigger).toBeFocused()

    const account = page.getByRole('button', { name: /Account and settings/ })
    await account.focus()
    await page.keyboard.press('Enter')
    await expect(page.getByRole('menu')).toBeVisible()
    await page.keyboard.press('End')
    await expect(page.getByRole('menuitem', { name: /Forget token/ })).toBeFocused()
    await page.keyboard.press('Escape')
    await expect(account).toBeFocused()
  })

  test('search palette keeps Tab on its input and Escape restores focus', async ({ page }) => {
    await signIn(page)
    const pill = page.getByRole('button', { name: 'Search the hub' })
    await pill.focus()
    await page.keyboard.press('Enter')
    const input = page.getByRole('dialog', { name: 'Search the hub' }).getByRole('combobox', { name: 'Search' })
    await expect(input).toBeFocused()
    await page.keyboard.press('Tab')
    await page.keyboard.press('Shift+Tab')
    await expect(input).toBeFocused()
    await page.keyboard.press('Escape')
    await expect(page.getByRole('dialog')).toBeHidden()
    await expect(pill).toBeFocused()
  })
})

test.describe('tablet width', () => {
  test.use({ viewport: { width: 768, height: 1024 } })
  test('the rail becomes a drawer that is hidden from Tab while closed', async ({ page }) => {
    await signIn(page)
    await expect(rail(page)).toBeHidden()
    const burger = page.getByRole('button', { name: 'Open navigation' })
    await burger.focus()
    await page.keyboard.press('Enter')
    await expect(rail(page)).toBeVisible()
    await expect(rail(page).getByRole('link').first()).toBeFocused()
    await page.keyboard.press('Escape')
    await expect(rail(page)).toBeHidden()
    await expect(burger).toBeFocused()
    await burger.click()
    await rail(page).getByRole('link', { name: /^Activity/ }).click()
    await expect(rail(page)).toBeHidden()
    await expect(crumbs(page)).toContainText('Activity')
  })
})

test('live: a refusal and handoff made by agents appear in Activity without reload', async ({ page }) => {
  await signIn(page, '#activity')
  await expect(main(page)).toContainText('What just happened?')
  const handoff = await freshHandoff(marker())
  try {
    // Activity polls every 5 s.
    await expect(main(page).getByText(handoff.file).first()).toBeVisible({ timeout: 20_000 })
    await expect(main(page)).toContainText(/was refused an edit to/)
  } finally {
    await handoff.release()
  }
})

test('live: work started and checkpointed over MCP shows up in the UI, then moves to finished', async ({ page }) => {
  test.skip(!AGENT_TOKEN, 'No agent token in ui/dev-seed/.data/tokens.env')
  test.setTimeout(120_000)
  await signIn(page, '#work')
  await expect(main(page)).toContainText('Rebuild the hub status page shell')

  const m = marker()
  const { item, session } = await startWork(m)
  try {
    // The overview refreshes every 15 s; the new card appears in the In progress lane.
    const card = main(page).locator('.lane').filter({ hasText: 'In progress' }).getByText(`E2E probe ${m}`)
    await expect(card).toBeVisible({ timeout: 30_000 })

    // Cards show the reported next action; the checkpoint summary is on the work detail page.
    expect((await checkpoint(item, session, 'progress', `Checkpoint ${m} via MCP`, `Next step ${m}`)).status).toBe(200)
    await expect(main(page).locator('.lane').filter({ hasText: 'In progress' })).toContainText(`Next step ${m}`, { timeout: 30_000 })
    await page.evaluate((id) => (window.location.hash = `#work/${id}`), item.id)
    await expect(crumbs(page).locator('[aria-current="page"]')).toHaveText(`E2E probe ${m}`)
    await expect(main(page)).toContainText(`Checkpoint ${m} via MCP`)
  } finally {
    const latest = await readWork(item.id)
    expect((await checkpoint(latest, session, 'finish', `Finished ${m}`)).status).toBe(200)
  }
  await page.evaluate(() => (window.location.hash = '#work/finished'))
  await expect(main(page)).toContainText(`E2E probe ${m}`, { timeout: 30_000 })
})

test('forgetting the token returns to sign-in and clears it from this browser', async ({ page }) => {
  await signIn(page)
  await page.getByRole('button', { name: /Account and settings/ }).click()
  await page.getByRole('menuitem', { name: /Forget token/ }).click()
  await expect(page.getByLabel(/token/i).first()).toBeVisible()
  await expect(page.getByText(/Token forgotten on this browser/)).toBeVisible()
  expect(await page.evaluate(() => [Object.keys(localStorage), Object.keys(sessionStorage)].flat().filter((k) => /token/i.test(k)))).toEqual([])
})

test('sign-in refuses an agent token, by paste and by #token=', async ({ page }) => {
  test.skip(!AGENT_TOKEN, 'No agent token in ui/dev-seed/.data/tokens.env')
  await page.goto('/')
  await page.locator('#token').fill(AGENT_TOKEN)
  await page.getByRole('button', { name: /Open status/ }).click()
  await expect(page.getByText('This is an agent token, which can change coordination. The status page needs a read-only token.')).toBeVisible()
  await expect(page.getByRole('button', { name: /Account and settings/ })).toHaveCount(0)

  // Same tab, hash-only navigation: the token is still stripped and still refused.
  await page.goto(`/#token=${AGENT_TOKEN}`)
  await expect(page).not.toHaveURL(/token=/)
  await expect(page.locator('#token')).toBeVisible()
  await expect(page.getByRole('button', { name: /Account and settings/ })).toHaveCount(0)
})

test.describe('narrow header keeps problems in words', () => {
  test.use({ viewport: { width: 768, height: 1024 } })

  test('"Live" collapses to a dot, but "Degraded" stays readable', async ({ page }) => {
    await signIn(page)
    const chip = page.locator('.health-chip')
    await expect(chip).toHaveAttribute('aria-label', /answering/)
    await expect(chip.locator('.health-text')).toBeHidden()

    await page.route('**/v1/hub/health', (r) => r.fulfill({ status: 503, contentType: 'application/json', body: '{"status":"unavailable"}' }))
    await page.reload()
    await expect(chip).toHaveClass(/is-bad/, { timeout: 20_000 })
    await expect(chip.locator('.health-text')).toBeVisible()
    await expect(chip).toContainText('Degraded')
  })

  test('a failing refresh shows "Reconnecting" as text, not just colour', async ({ page }) => {
    await signIn(page)
    await page.route('**/v1/hub/repos/**', (r) => r.abort())
    const chip = page.locator('.health-chip')
    await expect(chip).toContainText('Reconnecting', { timeout: 20_000 })
    await expect(chip.locator('.health-text')).toBeVisible()
    await expect(page.locator('.crumb-fresh')).toBeVisible()
    await expect(page.locator('.crumb-fresh')).toContainText('Reconnecting')
  })
})

test('a malformed workflow address shows the run list instead of a blank page, even after reload', async ({ page }) => {
  await signIn(page)
  const errors: string[] = []
  page.on('pageerror', (e) => errors.push(e.message))
  await page.evaluate(() => (window.location.hash = '#workflows/runs/%E0%A4%A'))
  await expect(crumbs(page).locator('[aria-current="page"]')).toHaveText(/Workflows/)
  await expect(main(page)).toBeVisible()
  await page.reload()
  await expect(crumbs(page).locator('[aria-current="page"]')).toHaveText(/Workflows/)
  await expect(page.getByRole('alert').filter({ hasText: 'This page hit a problem' })).toHaveCount(0)
  expect(errors).toEqual([])
})
