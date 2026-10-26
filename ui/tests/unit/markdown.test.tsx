import { render } from '@testing-library/react'
import { describe, expect, it } from 'vitest'
import { Markdown, parseBlocks } from '../../src/Markdown.tsx'
import { parseStep } from '../../src/skills.ts'

describe('procedure markdown', () => {
  it('drops frontmatter and groups headings, wrapped list items and paragraphs', () => {
    const md = '---\nname: x\n---\n\n# x v1\n\nRecord each gate with\n`agentscm skill evidence`.\n\n1. **branch**: work on a task branch.\n2. **tests**: run the tests\n   and record them.\n\nLast line.'
    expect(parseBlocks(md)).toEqual([
      { kind: 'h', level: 1, text: 'x v1' },
      { kind: 'p', text: 'Record each gate with `agentscm skill evidence`.' },
      { kind: 'ol', start: 1, items: ['**branch**: work on a task branch.', '**tests**: run the tests and record them.'] },
      { kind: 'p', text: 'Last line.' },
    ])
  })
  it('renders text, never markup, and links only http(s)', () => {
    const { container } = render(<Markdown source={'<img src=x onerror=alert(1)> **b** `c` [ok](https://example.test) [no](javascript:alert(1))'} />)
    expect(container.querySelector('img')).toBeNull()
    expect(container.querySelector('strong')?.textContent).toBe('b')
    expect(container.querySelector('code')?.textContent).toBe('c')
    expect([...container.querySelectorAll('a')].map((a) => a.getAttribute('href'))).toEqual(['https://example.test'])
  })
})

describe('walkthrough steps', () => {
  it('splits a hub line into who, needs and how firmly it is held', () => {
    expect(parseStep("2. Tests: The author runs the tests on the exact head commit. Done by the author agent. Needs: test results for the current head (self-reported or stronger). Blocks if the opt-in Stop hook is installed: the author's session cannot finish until this is recorded.")).toEqual({
      name: 'Tests', plain: 'The author runs the tests on the exact head commit.', who: 'Author agent', needs: 'test results for the current head (self-reported or stronger)', enforce: 'block', detail: "the author's session cannot finish until this is recorded",
    })
    expect(parseStep('8. Post merge: After the merge, it checks. Done by the author agent. Needs: a pass (self-reported or stronger). Recorded only; the alert is not built yet.')?.enforce).toBe('record')
    expect(parseStep('7. Merge: A maintainer merges. Done by a maintainer (a person). Needs: the merge (squash) (self-reported or stronger). Warns only.')).toMatchObject({ who: 'Maintainer (a person)', needs: 'the merge (squash) (self-reported or stronger)', enforce: 'warn' })
    expect(parseStep('Something the hub wrote differently')).toBeNull()
  })
})
