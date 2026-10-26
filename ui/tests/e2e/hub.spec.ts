import { expect, test, type Page } from '@playwright/test'
import { AGENT_TOKEN, READ_TOKEN, checkpoint, freshHandoff, hubReachable, marker, readWork, seededPublication, startWork } from '../hub.ts'

test.beforeAll(async () => {
  test.skip(!(await hubReachable()), 'Start the dev hub first: ui/dev-seed/start.sh (127.0.0.1:7332).')
})

const rail = (page: Page) => page.getByRole('complementary', { name: 'Hub sections' })
const crumbs = (page: Page) => page.getByRole('navigation', { name: 'Breadcrumb' })
const main = (page: Page) => page.locator('#main')
/** The open tab of the current nav item: a label over its live status. */
const tab = (page: Page) => page.locator('.htabs [aria-current="page"]')

async function signIn(page: Page, hash = '') {
  await page.goto(`/#token=${READ_TOKEN}`)
  await expect(page.getByRole('button', { name: /Account and settings for maya-chen/ })).toBeVisible()
  if (hash) await page.evaluate((h) => (window.location.hash = h), hash)
}

test('signs in from #token= and strips the token from the address bar', async ({ page }) => {
  await signIn(page)
  expect(page.url()).not.toContain('token=')
  await expect(page.getByRole('button', { name: 'counterbranch/agentscm' })).toBeVisible()
  for (const item of ['Dashboard', 'Inbox', 'History', 'Timeline', 'Skills', 'Setup']) await expect(rail(page).getByRole('link', { name: new RegExp(`^${item}`) })).toBeVisible()
  await expect(tab(page)).toContainText('Now')
  await expect(main(page)).toContainText(/agents? (is|are) working on/)
})

test('every page is reachable from the rail and its tabs, each with its question', async ({ page }) => {
  await signIn(page)
  const pages: [string, string, string][] = [
    ['Dashboard', 'Where', 'Who is editing which files?'],
    ['Dashboard', 'Catch up', 'What happened since I last looked?'],
    ['Dashboard', 'Team', 'Who is working right now?'],
    ['Inbox', '', 'Which overlaps need a conversation?'],
    ['History', 'Finished', 'What did agents report finished?'],
    ['History', 'Changes', 'What changed in Work, and when?'],
    ['History', 'Change records', 'What is ready to hand off?'],
    ['Skills', 'Checks', 'Did the checks pass?'],
    ['Setup', 'Access', 'Who can run what?'],
    ['Setup', 'Agents', 'Is my agent set up?'],
    ['Setup', 'Hub', 'Is the hub OK?'],
    ['Setup', 'Preferences', 'How is this page set up for me?'],
    ['Dashboard', 'Now', 'What needs me, and where is every task?'],
  ]
  for (const [item, label, question] of pages) {
    await rail(page).getByRole('link', { name: new RegExp(`^${item}`) }).click()
    if (label) await page.locator('.htabs').getByRole('link', { name: new RegExp(`^${label}`) }).click()
    if (label) await expect(tab(page)).toContainText(label)
    await expect(main(page).getByRole('heading', { level: 1 }).first()).toHaveText(question)
    await expect(rail(page).getByRole('link', { name: new RegExp(`^${item}`) })).toHaveAttribute('aria-current', 'page')
    // A new page starts at the top with focus on it; a tab inside the same page keeps focus on the tab picked.
    expect(await page.evaluate(() => [window.scrollY, document.activeElement?.id === 'main' || !!document.activeElement?.closest('.htabs')])).toEqual([0, true])
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

test('a work item opens its authenticated change-record projection', async ({ page }) => {
  await signIn(page, '#home')
  await main(page).getByRole('link', { name: 'Rebuild the hub status page shell' }).first().click()
  await main(page).getByRole('link', { name: 'Change record' }).click()
  await expect(page).toHaveURL(/#change-record\/wrk_[0-9a-f]{32}$/)
  await expect(crumbs(page).locator('[aria-current="page"]')).toContainText('Work wrk_')
  await expect(main(page).getByRole('heading', { level: 1, name: 'Rebuild the hub status page shell' })).toBeVisible()
  await expect(main(page)).toContainText('No source-bound action has been captured.')
  await expect(main(page)).toContainText('No action captured')
})

test('a directed request expands to show targets, receipts and paths', async ({ page }) => {
  await signIn(page, '#work/coordination')
  await expect(page).toHaveURL(/#work\/coordination$/)
  await expect(rail(page).getByRole('link', { name: /^Inbox/ })).toHaveAttribute('aria-current', 'page')
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
  await expect(rail(page).getByRole('link', { name: 'Inbox', exact: true })).toBeVisible()
  await page.keyboard.press('[')
  await expect(rail(page)).not.toHaveClass(/is-compact/)
  await page.reload()
  await expect(rail(page)).not.toHaveClass(/is-compact/)
})

test('the top bar view menu switches theme and density, and both persist', async ({ page }) => {
  await signIn(page)
  await page.getByRole('button', { name: 'Theme and density' }).click()
  await page.getByRole('menuitemradio', { name: /^Dark/ }).click()
  await page.getByRole('menuitemradio', { name: 'Condensed' }).click()
  await expect(page.locator('html')).toHaveAttribute('data-bs-theme', 'dark')
  await expect(page.locator('html')).toHaveAttribute('data-density', 'condensed')
  await page.reload()
  await expect(page.locator('html')).toHaveAttribute('data-bs-theme', 'dark')
  await expect(page.locator('html')).toHaveAttribute('data-density', 'condensed')
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
    await expect(tab(page)).toContainText('Now') // the hash was not changed
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
    await rail(page).getByRole('link', { name: /^History/ }).click()
    await expect(rail(page)).toBeHidden()
    await expect(tab(page)).toContainText('Finished')
  })
})

test('live: a refusal and handoff made by agents appear in Activity without reload', async ({ page }) => {
  await signIn(page, '#activity')
  await expect(main(page)).toContainText('The full log')
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
  await signIn(page, '#home')
  await expect(main(page)).toContainText('Rebuild the hub status page shell')

  const m = marker()
  const { item, session } = await startWork(m)
  try {
    // The overview refreshes every 15 s; the new task appears on the track.
    const lane = main(page).locator('#box-track .rl-row').filter({ hasText: `E2E probe ${m}` })
    await expect(lane).toBeVisible({ timeout: 30_000 })

    // Lanes show the reported next action; the checkpoint summary is on the task page too.
    expect((await checkpoint(item, session, 'progress', `Checkpoint ${m} via MCP`, `Next step ${m}`)).status).toBe(200)
    await expect(lane).toContainText(`Next step ${m}`, { timeout: 30_000 })
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
  })
})

test('a malformed workflow address shows the run list instead of a blank page, even after reload', async ({ page }) => {
  await signIn(page)
  const errors: string[] = []
  page.on('pageerror', (e) => errors.push(e.message))
  await page.evaluate(() => (window.location.hash = '#workflows/runs/%E0%A4%A'))
  await expect(tab(page)).toContainText('Checks')
  await expect(main(page)).toBeVisible()
  await page.reload()
  await expect(tab(page)).toContainText('Checks')
  await expect(page.getByRole('alert').filter({ hasText: 'This page hit a problem' })).toHaveCount(0)
  expect(errors).toEqual([])
})

// ---------- New features: Landed changes (#62), Templates (#64), Timeline (#67)

test('the local pages and After merge each answer their question', async ({ page }) => {
  await signIn(page)
  for (const [hash, question] of [
    ['#landed', 'Did our changes land, and did checks pass after?'],
    ['#templates', 'Which reusable workflows can agents configure here?'],
    ['#timeline', 'What did this workspace look like earlier?'],
  ] as const) {
    await page.evaluate((h) => (window.location.hash = h), hash)
    await expect(main(page).getByRole('heading', { level: 1 })).toHaveText(question)
  }
})

test('Landed changes shows live changes to a read-only token, or says plainly the hub won’t', async ({ page }) => {
  // Hubs before the read-only publication routes refuse the status page's token; newer ones list
  // changes. The page must handle both, and must never ask for an agent token.
  const probe = await page.request.get(`/v1/hub/repos/${encodeURIComponent('counterbranch/agentscm')}/publications?limit=1`, { headers: { Authorization: `Bearer ${READ_TOKEN}` } })
  const readable = probe.ok()
  const failedCi = seededPublication('agent/rune/schema-migration-lock')
  test.skip(!failedCi, 'No seeded publications in ui/dev-seed/.data/publications.txt')
  await signIn(page, '#landed')
  if (readable) {
    await expect(main(page).getByRole('heading', { level: 1 })).toHaveText('Did our changes land, and did checks pass after?')
    await expect(main(page)).toContainText('3 of 7 recent changes landed')
    await expect(main(page)).toContainText('agent/rune/schema-migration-lock')
    await expect(page.locator('[data-tour="pager"]')).toContainText('That’s everything.')
    // The picker searches the hub: type part of a branch, pick it, land on that change.
    const picker = page.getByRole('combobox', { name: 'Find a change' })
    await picker.fill('schema-migration')
    await expect(page.getByRole('option', { name: /schema-migration-lock/ })).toBeVisible()
    await picker.press('ArrowDown')
    await picker.press('Enter')
    await expect(page).toHaveURL(new RegExp(`#landed/${failedCi}$`))
    await page.evaluate((h) => (window.location.hash = h), '#landed')
    await page.evaluate((h) => (window.location.hash = h), `#landed/${failedCi}`)
    const track = main(page).locator('.box').filter({ hasText: 'How far it got' })
    await expect(track).toContainText('Opened #483')
    await expect(track).toContainText('Failed')
    await expect(main(page).getByText('Partly checked', { exact: false }).first()).toBeVisible()
  } else {
    expect(probe.status()).toBe(403)
    await expect(main(page)).toContainText(/needs a newer hub/)
    await page.evaluate((h) => (window.location.hash = h), `#landed/${failedCi}`)
    await expect(main(page).getByText('Landed changes aren’t readable here yet')).toBeVisible()
  }
  await expect(page.getByLabel(/token/i)).toHaveCount(0)
})

test('Templates and Timeline read this computer through the dev proxy: live when serve answers, labelled preview when not', async ({ page }) => {
  // The local read API may or may not be running; the page must say which, and work either way.
  const probe = await page.request.get('/v1/local/workspace/timeline?limit=1').catch(() => null)
  const live = probe !== null && probe.ok() && (probe.headers()['content-type'] ?? '').includes('json')
  const banner = (scope: ReturnType<typeof main>) => scope.getByText(/Preview data/)
  await signIn(page, '#timeline')
  await expect(main(page).getByRole('heading', { name: 'Saved moments' })).toBeVisible()
  if (live) await expect(banner(main(page))).toHaveCount(0)
  else await expect(banner(main(page))).toBeVisible()
  await page.getByLabel('Pick #3 to compare').check()
  await page.getByLabel('Pick #2 to compare').check()
  await page.getByRole('link', { name: /Compare these two/ }).click()
  await expect(crumbs(page).locator('[aria-current="page"]')).toContainText('Compare #2 and #3')
  await expect(main(page).getByText(/What changed from #2 to #3/)).toBeVisible()
  // Inspecting one change's template is the Library's third view.
  await page.evaluate(() => (window.location.hash = '#templates?view=inspect'))
  const changePicker = page.getByRole('combobox', { name: 'Change ID' })
  await changePicker.fill('other-change')
  await changePicker.press('Enter')
  await page.getByLabel('Template file').fill('skills/checks/v2/workflow.json')
  await page.getByRole('button', { name: /Open/ }).first().click()
  await expect(main(page).getByText('Check this path is in the change: src/lib.rs')).toBeVisible()
  if (live) await expect(banner(main(page))).toHaveCount(0)
  else await expect(banner(main(page))).toBeVisible()
})

test('pickers: change records, workflow runs and activity paging', async ({ page }) => {
  await signIn(page, '#change-record')
  const work = page.getByRole('combobox', { name: 'Find a work item' })
  await expect(page.locator('[data-tour="change-record-picker"]')).toBeVisible()
  await work.focus()
  // Seeded work items (and any started by the MCP tests) are listed with human titles; the ID is secondary.
  await expect(page.getByRole('option').first()).toBeVisible()
  await work.press('ArrowDown')
  await work.press('Enter')
  await expect(page).toHaveURL(/#change-record\/wrk_[0-9a-f]{32}$/)

  await page.evaluate(() => (window.location.hash = '#workflows'))
  const run = page.getByRole('combobox', { name: 'Find a run' })
  await expect(page.locator('[data-tour="workflow-run-picker"]')).toBeVisible()
  await run.fill('retained-run-001')
  await expect(page.getByRole('option', { name: /Open retained-run-001/ })).toBeVisible()
  await run.press('Enter')
  await expect(page).toHaveURL(/#workflows\/runs\/retained-run-001$/)

  await page.evaluate(() => (window.location.hash = '#activity'))
  const older = page.locator('[data-tour="activity-load-older"]')
  await expect(older).toBeVisible()
  await older.getByRole('button', { name: /Load older/ }).click()
  await expect(older.getByRole('button', { name: /Load older/ }).or(older.getByText('That’s everything.'))).toBeVisible()
})

test('keyboard: the Landed picker reaches "Show more results" and Enter loads the next page', async ({ page }) => {
  const pubs = `/v1/hub/repos/${encodeURIComponent('counterbranch/agentscm')}/publications`
  const seen: string[] = []
  // A scan that hit its cap: no matches yet, but a cursor to keep looking.
  await page.route(`**${pubs}?*`, async (route) => {
    const url = new URL(route.request().url())
    if (!url.searchParams.has('q')) return route.continue()
    seen.push(url.search)
    const body = url.searchParams.has('before')
      ? { publications: [], next_cursor: null, complete: true }
      : { publications: [], next_cursor: `pub_${'a'.repeat(32)}`, complete: false, searched_to: 1 }
    await route.fulfill({ contentType: 'application/json', body: JSON.stringify(body) })
  })
  await signIn(page, '#landed')
  const picker = page.getByRole('combobox', { name: 'Find a change' })
  await picker.fill('no such branch')
  await expect(page.getByText('No matches in the results searched so far')).toBeVisible()
  // The action is an option of the listbox: arrow to it and press Enter.
  await picker.press('ArrowDown')
  await expect(page.getByRole('option', { name: 'Show more results' })).toHaveAttribute('aria-selected', 'true')
  await picker.press('Enter')
  await expect.poll(() => seen.some((q) => q.includes('before=pub_'))).toBe(true)
})
