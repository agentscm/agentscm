// Phone-sized flows: the rail is a drawer, the header collapses, and nothing scrolls sideways.
import { expect, test, type Page } from '@playwright/test'
import { READ_TOKEN, hubReachable } from '../hub.ts'

test.beforeAll(async () => {
  test.skip(!(await hubReachable()), 'Start the dev hub first: ui/dev-seed/start.sh (127.0.0.1:7332).')
})

const rail = (page: Page) => page.getByRole('complementary', { name: 'Hub sections' })
const crumbs = (page: Page) => page.getByRole('navigation', { name: 'Breadcrumb' })
const noSideScroll = (page: Page) => page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1)
const CHANGE_RECORD_WORK_ID = `wrk_${'a'.repeat(32)}`
const SOURCE_ID = 'b'.repeat(64)
const RECIPE_DIGEST = 'c'.repeat(64)
const SETUP_DIGEST = 'd'.repeat(64)

const longIdentityChangeRecord = {
  version: 1,
  availability: 'available',
  summary: {
    schema_version: 1,
    work_id: CHANGE_RECORD_WORK_ID,
    generation: 1,
    as_of: 1_798_000_300,
    intent: { title: 'Inspect a narrow change record', scope: ['ui/src/pages/ChangeRecord.tsx'], scope_omitted: 0 },
    subject: {
      source_id: SOURCE_ID,
      source_kind: 'git',
      source: {
        kind: 'git', repository: 'counterbranch/agentscm', worktree_id: 'synthetic-mobile-worktree',
        head: '1'.repeat(40), tree: '2'.repeat(40), base: '3'.repeat(40), dirty: false,
        untracked_count: 0, omissions: [], captured_snapshot: null, snapshot_coverage: 'complete',
      },
    },
    sessions: { count: 1, coverage: 'complete', gaps: 0 },
    latest_action: {
      action_id: `act:${'e'.repeat(64)}`, source_id: SOURCE_ID, kind: 'verification', outcome: 'succeeded',
      recipe_provenance: 'configured_exact_match', recipe_digest: RECIPE_DIGEST, setup_digest: SETUP_DIGEST,
      result: 'passed', applicability: 'current', assurance: 'caller_asserted', evidence_availability: 'local',
      coverage: 'complete', started_at: 1_798_000_000, finished_at: 1_798_000_120, drift: 'none_observed',
    },
    decision: {
      status: 'proposed', reference: 'decision-mobile', summary: 'Keep long identities readable on phones.',
      source_refs: [], source_refs_omitted: 0, author_kind: 'agent', author_agent: 'synthetic-agent',
      at: 1_798_000_060, generation: 1,
    },
    next_action: 'Review the bounded receipt.',
    completeness: { events: 'complete', source: 'complete', setup: 'partial', recipe: 'partial', result: 'complete', usage: 'partial' },
    unknowns: ['human_decision'],
  },
}

async function signIn(page: Page) {
  await page.goto(`/#token=${READ_TOKEN}`)
  await expect(page.getByRole('button', { name: /Account and settings for maya-chen/ })).toBeVisible()
}

test('the drawer opens from the menu button, navigates, and closes', async ({ page }) => {
  await signIn(page)
  await expect(rail(page)).toBeHidden()
  await page.getByRole('button', { name: 'Open navigation' }).tap()
  await expect(rail(page)).toBeVisible()
  await rail(page).getByRole('link', { name: /^Work(?= )/ }).tap()
  await expect(rail(page)).toBeHidden()
  await expect(crumbs(page).locator('[aria-current="page"]')).toContainText('Working now')
  await expect(page.locator('#main')).toContainText('Rebuild the hub status page shell')
  expect(await noSideScroll(page)).toBe(true)
})

test('tapping the dimmed page closes the drawer', async ({ page }) => {
  await signIn(page)
  await page.getByRole('button', { name: 'Open navigation' }).tap()
  await expect(rail(page)).toBeVisible()
  const box = page.viewportSize()!
  await page.touchscreen.tap(box.width - 10, box.height / 2)
  await expect(rail(page)).toBeHidden()
})

test('every page fits the phone width', async ({ page }) => {
  await signIn(page)
  for (const hash of ['#now', '#work', '#work/coordination', '#activity', '#workflows', '#change-record', '#landed', '#templates', '#timeline', '#workspace', '#setup', '#health', '#settings']) {
    await page.evaluate((h) => (window.location.hash = h), hash)
    await expect(page.locator('#main')).not.toBeEmpty()
    expect(await noSideScroll(page), hash).toBe(true)
  }
})

test('change record detail wraps long source, recipe and setup identities', async ({ page }) => {
  await page.route('**/v1/hub/health', (route) => route.fulfill({
    status: 200,
    contentType: 'application/json',
    body: JSON.stringify({ status: 'ok', storage: 'sqlite', coordination: {} }),
  }))
  await page.route('**/v1/hub/repos/**/work', (route) => route.fulfill({
    status: 200,
    contentType: 'application/json',
    body: JSON.stringify({ intents: [], intents_truncated: false, connected: true, pull_requests: [] }),
  }))
  await page.route('**/v1/hub/repos/**/activity?limit=50', (route) => route.fulfill({
    status: 200,
    contentType: 'application/json',
    body: JSON.stringify({ sessions: [], events: [] }),
  }))
  await page.route('**/v1/hub/whoami', (route) => route.fulfill({
    status: 200,
    contentType: 'application/json',
    body: JSON.stringify({
      token: { id: 'tkn_mobile', name: 'status-page', repository: 'counterbranch/agentscm', expires_at: null, read_only: true },
      owner: { provider: 'github', host: 'github.com', login: 'maya-chen' },
    }),
  }))
  await page.route('**/v1/hub/repos/**/change-records/**', (route) => route.fulfill({
    status: 200,
    contentType: 'application/json',
    body: JSON.stringify(longIdentityChangeRecord),
  }))
  await signIn(page)
  await page.evaluate((workId) => { window.location.hash = `change-record/${workId}` }, CHANGE_RECORD_WORK_ID)
  await expect(page.getByRole('heading', { level: 1, name: 'Inspect a narrow change record' })).toBeVisible()
  await expect(page.locator('code:visible').filter({ hasText: SOURCE_ID }).first()).toHaveText(SOURCE_ID)
  const receiptIdentities = page.locator('details').filter({ hasText: 'Receipt identities' })
  await receiptIdentities.getByText('Receipt identities', { exact: true }).click()
  await expect(receiptIdentities.getByText(RECIPE_DIGEST, { exact: true })).toBeVisible()
  await expect(receiptIdentities.getByText(SETUP_DIGEST, { exact: true })).toBeVisible()
  expect(await noSideScroll(page)).toBe(true)
})

test('the narrow header keeps "Degraded" in words', async ({ page }) => {
  await signIn(page)
  await expect(page.locator('.health-chip .health-text')).toBeHidden()
  await page.route('**/v1/hub/health', (r) => r.fulfill({ status: 503, contentType: 'application/json', body: '{"status":"unavailable"}' }))
  await page.reload()
  await expect(page.locator('.health-chip')).toContainText('Degraded', { timeout: 20_000 })
  await expect(page.locator('.health-chip .health-text')).toBeVisible()
})

test('search opens from the header button and finds a work item', async ({ page }) => {
  await signIn(page)
  await page.getByRole('button', { name: 'Search the hub' }).tap()
  const input = page.getByRole('combobox', { name: 'Search' })
  await expect(input).toBeFocused()
  await input.fill('Retention migration')
  await page.getByRole('option', { name: /^Retention migration for PostgreSQL\s*gemini-pax/ }).tap()
  await expect(crumbs(page).locator('[aria-current="page"]')).toHaveText(/Retention migration for PostgreSQL/)
})
