import { render, screen } from '@testing-library/react'
import { describe, expect, it } from 'vitest'
import * as copy from '../../src/copy.ts'
import { Box, PageHead } from '../../src/ui.tsx'

describe('PageHead', () => {
  it('renders the question, the answer and an optional fact, and keeps the tour anchor', () => {
    const { container } = render(<PageHead question="What is happening?" answer="Two tasks are moving." fact="Nothing is stuck." />)
    const head = container.querySelector('[data-tour="answer"]')
    expect(head?.className).toBe('answer')
    expect(screen.getByRole('heading', { level: 1, name: 'What is happening?' }).className).toBe('page-question')
    expect(head?.querySelector('.answer-text')?.textContent).toBe('Two tasks are moving.')
    expect(head?.querySelector('.answer-fact')?.textContent).toBe('Nothing is stuck.')
  })

  it('omits the fact line when there is no fact and adds a class on request', () => {
    const { container } = render(<PageHead question="Q" answer="A" className="detail-head" />)
    expect(container.querySelector('.answer-fact')).toBeNull()
    expect(container.querySelector('[data-tour="answer"]')?.className).toBe('answer detail-head')
  })
})

describe('Box intro', () => {
  it('keeps the explainer in the tooltip and shows no visible intro by default', () => {
    const { container } = render(<Box icon="x" title="Section" explain="Why this exists.">Body</Box>)
    expect(container.querySelector('.box-intro')).toBeNull()
    expect(container.querySelector('.explain')).not.toBeNull()
  })

  it('shows an intro line only when asked for', () => {
    const { container } = render(<Box icon="x" title="Section" intro="Copy this prompt to start.">Body</Box>)
    expect(container.querySelector('.box-intro')?.textContent).toBe('Copy this prompt to start.')
  })
})

describe('copy constants', () => {
  it('are each one short sentence', () => {
    for (const [name, text] of Object.entries(copy)) {
      expect(text.length, name).toBeLessThan(130)
      expect(text.trim(), name).toBe(text)
    }
  })
})
