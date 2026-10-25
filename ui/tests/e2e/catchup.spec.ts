import { expect, test, type Page } from '@playwright/test'

const TOKEN = 'synthetic-read-token'
const DELIVERY = `cup_${'a'.repeat(32)}`
const NEXT_DELIVERY = `cup_${'b'.repeat(32)}`
const SOURCE_COVERAGE = [
  { source: 'tracked_publication', state: 'observed_only', checked_at: 1_792_940_010, unread_pages: 0, deferred: 0 },
  { source: 'forge_pull', state: 'incomplete', checked_at: 1_792_940_010, unread_pages: 2, deferred: 1 },
]

const firstPage = {
  subscriber: 'human', delivery_client: 'set-from-request', delivery: DELIVERY,
  event_from: 3, event_through: 4, event_after: 3, next_event_after: 3,
  merge_from: 0, merge_through: 2, merge_after: 0, next_merge_after: 1,
  work_events: [], coordination_events: [],
  merges: [{
    id: 'merge-observation-1', seq: 1, source: 'forge_pull', source_id: 'github:129', occurred_at: null, observed_at: 1_792_940_010,
    title: '<img src=x onerror=alert(1)>', branch: 'feat/catchup', base: 'main', head_commit: '1'.repeat(40), merge_commit: '2'.repeat(40), pull_number: 129,
    paths: [`ui/${'long-path/'.repeat(24)}catchup.ts`], paths_complete: false,
  }],
  retention: { work_gap: true, coordination_gap: false, merge_gap: false, event_available_from: 2, merge_available_from: 0 },
  source_coverage: SOURCE_COVERAGE,
  complete: false,
}
const secondPage = {
  ...firstPage,
  event_after: 3, next_event_after: 4,
  merge_after: 1, next_merge_after: 2,
  coordination_events: [{ seq: 4, at: 1_792_940_020, kind: 'coordination_requested', request_id: `crq_${'c'.repeat(32)}`, title: 'Split the shared UI scope', status: 'pending' }],
  merges: [{
    id: 'merge-observation-2', seq: 2, source: 'tracked_publication', source_id: `pub_${'d'.repeat(32)}`, occurred_at: 1_792_940_015, observed_at: 1_792_940_020,
    title: 'Add durable catch-up', branch: 'feat/catchup', base: 'main', head_commit: '3'.repeat(40), merge_commit: '4'.repeat(40), pull_number: 130,
    paths: ['src/hub/catchup.rs'], paths_complete: true,
  }],
  complete: true,
}
const afterConflict = {
  ...firstPage,
  delivery: NEXT_DELIVERY,
  event_from: 4, event_through: 4, event_after: 4, next_event_after: 4,
  merge_from: 2, merge_through: 2, merge_after: 2, next_merge_after: 2,
  coordination_events: [], merges: [],
  retention: { work_gap: false, coordination_gap: false, merge_gap: false, event_available_from: 2, merge_available_from: 0 },
  complete: true,
}

async function stubHub(page: Page) {
  const acknowledgements: Record<string, unknown>[] = []
  const deliveryClients: string[] = []
  await page.context().route('**/v1/**', async (route) => {
    const request = route.request()
    const url = new URL(request.url())
    const json = (body: unknown, status = 200) => route.fulfill({ status, contentType: 'application/json', body: JSON.stringify(body) })
    if (url.pathname === '/v1/hub/whoami') return json({ token: { id: 'tkn_1', name: 'status-page', repository: 'counterbranch/agentscm', read_only: true }, owner: { provider: 'github', host: 'github.com', login: 'synthetic-user' } })
    if (url.pathname === '/v1/hub/health') return json({ status: 'ok', storage: 'sqlite', coordination: { denied: 0, repeated: 0, rate_limited: 0, fail_open: 0, hook_p95_ms: 2 } })
    if (url.pathname === '/v1/hub/repos/counterbranch%2Fagentscm/work') return json({ intents: [], intents_truncated: false, pull_requests: [], connected: true })
    if (url.pathname === '/v1/hub/repos/counterbranch%2Fagentscm/activity') return json({ sessions: [], events: [], next_before: null })
    if (url.pathname === '/v1/hub/standup/overview') return json({ recent_since: 1_792_854_000, until: 1_792_944_000, current: [], recent: [], overlaps: [], current_complete: true, recent_complete: true, overlaps_complete: true, history_retention_gap: false })
    if (url.pathname === '/v1/hub/standup/catchup' && request.method() === 'GET') {
      const client = url.searchParams.get('delivery_client')
      if (!client) return json({ error: 'missing_delivery_client' }, 400)
      deliveryClients.push(client)
      if (url.searchParams.has('delivery')) return json({ ...secondPage, delivery_client: client })
      return json({ ...(acknowledgements.length === 0 ? firstPage : afterConflict), delivery_client: client })
    }
    if (url.pathname === '/v1/hub/standup/catchup/ack' && request.method() === 'POST') {
      acknowledgements.push(request.postDataJSON() as Record<string, unknown>)
      return json({ status: 'conflict', event_through: 4, merge_through: 2, current_event_ack: 4, current_merge_ack: 2 })
    }
    return json({ error: 'synthetic_route_not_found' }, 404)
  })
  return { acknowledgements, deliveryClients }
}

async function exercise(page: Page, screenshotName: string) {
  const { acknowledgements, deliveryClients } = await stubHub(page)
  await page.goto(`/#token=${TOKEN}`)
  await page.evaluate(() => { window.location.hash = '#work' })
  const main = page.locator('#main')
  await expect(main.getByRole('heading', { name: 'Since you last checked' })).toBeVisible()
  await expect.poll(() => deliveryClients.length).toBe(1)
  const initialClient = deliveryClients[0]!
  expect(initialClient).toMatch(/^hub_/)
  await page.reload()
  await expect(main.getByRole('heading', { name: 'Since you last checked' })).toBeVisible()
  await expect.poll(() => deliveryClients.length).toBe(2)
  expect(deliveryClients[1]).toBe(initialClient)
  await expect(main).toContainText('Earlier records are no longer retained')
  await expect(main).toContainText('Forge pull request source incomplete')
  await expect(main).toContainText('<img src=x onerror=alert(1)>')
  await expect(main.locator('img')).toHaveCount(0)
  await expect(main.getByText('No unfinished work is assigned right now.')).toBeVisible()
  await expect(main.getByRole('button', { name: /Mark (?:available records )?reviewed/ })).toHaveCount(0)
  await expect(main.evaluate((element) => element.scrollWidth <= element.clientWidth)).resolves.toBe(true)

  await main.getByRole('button', { name: 'Load more' }).click()
  await expect(main.getByRole('button', { name: 'Mark available records reviewed' })).toBeVisible()
  expect(acknowledgements).toHaveLength(0)
  await expect(main).toContainText('Split the shared UI scope')
  await main.getByRole('button', { name: 'Mark available records reviewed' }).click()
  await expect(main).toContainText('Another device advanced this review position')
  await expect(main.getByRole('button', { name: /Mark (?:available records )?reviewed/ })).toBeEnabled()
  expect(acknowledgements).toEqual([{ delivery: DELIVERY, expected_event_from: 3, expected_merge_from: 0 }])
  expect(new Set(deliveryClients)).toEqual(new Set([initialClient]))

  const output = process.env['AGENTSCM_CATCHUP_SCREENSHOT_DIR']
  if (output) await page.screenshot({ path: `${output}/${screenshotName}`, fullPage: true })
}

test('complete fixed delivery is required before a stale acknowledgement conflict is handled', async ({ page }) => {
  await exercise(page, 'catchup-desktop.png')
})

test('a cloned tab gets an independent fixed delivery client while the original remains active', async ({ page }) => {
  const { deliveryClients } = await stubHub(page)
  await page.goto(`/#token=${TOKEN}`)
  await page.evaluate(() => { window.location.hash = '#work' })
  const main = page.locator('#main')
  await expect(main.getByRole('heading', { name: 'Since you last checked' })).toBeVisible()
  await expect.poll(() => deliveryClients.length).toBe(1)
  const originalClient = deliveryClients[0]!

  const popupReady = page.waitForEvent('popup')
  await page.evaluate(() => { window.open(window.location.href, '_blank') })
  const popup = await popupReady
  const popupMain = popup.locator('#main')
  await expect(popupMain.getByRole('heading', { name: 'Since you last checked' })).toBeVisible()
  await expect.poll(() => deliveryClients.length).toBe(2)
  const cloneClient = deliveryClients[1]!
  expect(cloneClient).toMatch(/^hub_/)
  expect(cloneClient).not.toBe(originalClient)

  await popupMain.getByRole('button', { name: 'Load more' }).click()
  await expect(popupMain.getByRole('button', { name: 'Mark available records reviewed' })).toBeVisible()
  await main.getByRole('button', { name: 'Load more' }).click()
  await expect(main.getByRole('button', { name: 'Mark available records reviewed' })).toBeVisible()
  expect(deliveryClients.filter((client) => client === originalClient)).toHaveLength(2)
  expect(deliveryClients.filter((client) => client === cloneClient)).toHaveLength(2)
  await popup.close()
})

test.describe('mobile viewport', () => {
  test.use({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true })
  test('catch-up stays readable and explicit on a phone', async ({ page }) => {
    await exercise(page, 'catchup-mobile.png')
  })
})
