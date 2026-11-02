import { render } from '@testing-library/react'
import { describe, expect, it } from 'vitest'
import { healthAnswer } from '../../src/pages/Health.tsx'
import type { Health } from '../../src/types.ts'

const counters = { denied: 4, repeated: 1, fail_open: 0, rate_limited: 0, hook_p95_ms: 12 }
const text = (h: Health | null) => render(<p>{healthAnswer(h)}</p>).container.textContent

describe('Hub health answer', () => {
  it('says the Hub is healthy when nothing deserves a look, even with refusals', () => {
    expect(text({ status: 'ok', coordination: counters } as Health)).toBe('The Hub is healthy. Nothing needs a look.')
    expect(text({ status: 'ok' } as Health)).toBe('The Hub is healthy. Nothing needs a look.')
  })

  it('names edits that went ahead undecided and a slow hook', () => {
    expect(text({ status: 'ok', coordination: { ...counters, fail_open: 1 } } as Health)).toBe('The Hub is answering, but 1 edit went ahead because it could not decide.')
    expect(text({ status: 'ok', coordination: { ...counters, fail_open: 3, hook_p95_ms: 240 } } as Health)).toBe('The Hub is answering, but 3 edits went ahead because it could not decide and its hook is slow, 240 ms at p95.')
  })

  it('says when the Hub is down or still being checked', () => {
    expect(text({ status: 'unavailable' } as Health)).toBe('The Hub is not answering, so edits go ahead unchecked.')
    expect(text(null)).toBe('Checking…')
  })
})
