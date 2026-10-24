import { fireEvent, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it, vi } from 'vitest'
import { Combobox, type ComboOption, type ComboResult } from '../../src/Combobox.tsx'

const opt = (value: string, text = value): ComboOption => ({ value, text, label: text, secondary: `id-${value}` })
const OPTIONS = [opt('a', 'Alpha branch'), opt('b', 'Beta branch'), opt('c', 'Gamma')]
const input = () => screen.getByRole('combobox', { name: 'Pick' }) as HTMLInputElement
const idOnly = (t: string) => (/^id_[0-9]+$/.test(t) ? t : null)

describe('Combobox (client filter)', () => {
  it('exposes the ARIA 1.2 combobox contract', () => {
    render(<Combobox id="p" label="Pick" options={OPTIONS} onSelect={() => {}} data-tour="x-picker" />)
    expect(input().getAttribute('aria-expanded')).toBe('false')
    expect(input().getAttribute('aria-autocomplete')).toBe('list')
    expect(input().getAttribute('aria-controls')).toBeNull()
    expect(document.querySelector('[data-tour="x-picker"]')).toBeTruthy()
    fireEvent.focus(input())
    expect(input().getAttribute('aria-expanded')).toBe('true')
    const list = screen.getByRole('listbox')
    expect(list.id).toBe('p-listbox')
    expect(input().getAttribute('aria-controls')).toBe('p-listbox')
    expect(screen.getAllByRole('option')).toHaveLength(3)
    expect(screen.getAllByRole('option').every((o) => o.getAttribute('aria-selected') === 'false')).toBe(true)
    // The ID is secondary text, the label is primary.
    expect(screen.getAllByRole('option')[0]!.textContent).toContain('Alpha branch')
    expect(screen.getAllByRole('option')[0]!.querySelector('code')?.textContent).toBe('id-a')
  })

  it('filters as you type and says when nothing matches', () => {
    render(<Combobox id="p" label="Pick" options={OPTIONS} onSelect={() => {}} />)
    fireEvent.focus(input())
    fireEvent.change(input(), { target: { value: 'branch' } })
    expect(screen.getAllByRole('option')).toHaveLength(2)
    fireEvent.change(input(), { target: { value: 'zzz' } })
    expect(screen.queryAllByRole('option')).toHaveLength(0)
    expect(screen.getByText('No matches')).toBeTruthy()
  })

  it('moves with arrows, Home and End, wraps, selects with Enter and closes with Escape and Tab', () => {
    const onSelect = vi.fn()
    render(<Combobox id="p" label="Pick" options={OPTIONS} onSelect={onSelect} />)
    fireEvent.keyDown(input(), { key: 'ArrowDown' })
    expect(input().getAttribute('aria-expanded')).toBe('true')
    const active = () => screen.getAllByRole('option').findIndex((o) => o.getAttribute('aria-selected') === 'true')
    expect(active()).toBe(0)
    expect(input().getAttribute('aria-activedescendant')).toBe(screen.getAllByRole('option')[0]!.id)
    fireEvent.keyDown(input(), { key: 'ArrowDown' })
    expect(active()).toBe(1)
    fireEvent.keyDown(input(), { key: 'End' })
    expect(active()).toBe(2)
    fireEvent.keyDown(input(), { key: 'ArrowDown' })
    expect(active()).toBe(0)
    fireEvent.keyDown(input(), { key: 'ArrowUp' })
    expect(active()).toBe(2)
    fireEvent.keyDown(input(), { key: 'Home' })
    expect(active()).toBe(0)
    fireEvent.keyDown(input(), { key: 'Escape' })
    expect(input().getAttribute('aria-expanded')).toBe('false')
    fireEvent.keyDown(input(), { key: 'ArrowDown' })
    fireEvent.keyDown(input(), { key: 'ArrowDown' })
    fireEvent.keyDown(input(), { key: 'Enter' })
    expect(onSelect).toHaveBeenCalledWith('b', expect.objectContaining({ value: 'b' }))
    expect(input().getAttribute('aria-expanded')).toBe('false')
    fireEvent.focus(input())
    fireEvent.blur(input())
    expect(input().getAttribute('aria-expanded')).toBe('false')
  })

  it('selects with a click', () => {
    const onSelect = vi.fn()
    render(<Combobox id="p" label="Pick" options={OPTIONS} onSelect={onSelect} />)
    fireEvent.focus(input())
    fireEvent.click(screen.getByRole('option', { name: /Gamma/ }))
    expect(onSelect).toHaveBeenCalledWith('c', expect.anything())
  })

  it('offers "Open <id>" first for a pasted ID and opens it with Enter', () => {
    const onSelect = vi.fn()
    render(<Combobox id="p" label="Pick" options={OPTIONS} parseId={idOnly} onSelect={onSelect} />)
    fireEvent.focus(input())
    fireEvent.change(input(), { target: { value: ' id_42 ' } })
    expect(screen.getAllByRole('option')[0]!.textContent).toBe('Open id_42')
    fireEvent.keyDown(input(), { key: 'Enter' })
    expect(onSelect).toHaveBeenCalledWith('id_42', expect.anything())
  })

  it('does not offer to open text that is not an ID', () => {
    render(<Combobox id="p" label="Pick" options={OPTIONS} parseId={idOnly} onSelect={() => {}} />)
    fireEvent.focus(input())
    fireEvent.change(input(), { target: { value: '../etc' } })
    expect(screen.queryByText(/^Open/)).toBeNull()
  })

  it('keeps the picked text when asked to', () => {
    render(<Combobox id="p" label="Pick" options={OPTIONS} keepText onSelect={() => {}} />)
    fireEvent.focus(input())
    fireEvent.click(screen.getByRole('option', { name: /Beta/ }))
    expect(input().value).toBe('Beta branch')
    // Reopening shows everything again until the person types.
    fireEvent.focus(input())
    expect(screen.getAllByRole('option')).toHaveLength(3)
  })
})

describe('Combobox (server search)', () => {
  it('keeps old results inert while a new search is pending, so Enter cannot pick them', async () => {
    let release: (r: { options: ComboOption[] }) => void = () => {}
    let asked = false
    const search = (q: string) => (q ? new Promise<{ options: ComboOption[] }>((r) => { asked = true; release = r }) : Promise.resolve({ options: [opt('old', 'Old result')] }))
    const onSelect = vi.fn()
    render(<Combobox id="p" label="Pick" search={search} onSelect={onSelect} />)
    fireEvent.focus(input())
    await screen.findByRole('option', { name: /Old result/ })
    fireEvent.change(input(), { target: { value: 'new' } })
    await waitFor(() => expect(screen.getByRole('listbox').getAttribute('aria-busy')).toBe('true'))
    fireEvent.keyDown(input(), { key: 'ArrowDown' })
    fireEvent.keyDown(input(), { key: 'Enter' })
    fireEvent.click(screen.getByRole('option', { name: /Old result/ }))
    expect(onSelect).not.toHaveBeenCalled()
    expect(screen.getByRole('option').getAttribute('aria-selected')).toBe('false')
    await waitFor(() => expect(asked).toBe(true))
    release({ options: [opt('fresh', 'Fresh result')] })
    fireEvent.click(await screen.findByRole('option', { name: /Fresh result/ }))
    expect(onSelect).toHaveBeenCalledWith('fresh', expect.anything())
  })

  it('debounces typing, aborts the previous request and shows only the latest answer', async () => {
    const calls: { q: string; signal: AbortSignal }[] = []
    const search = (q: string, signal: AbortSignal) => {
      calls.push({ q, signal })
      return new Promise<{ options: ComboOption[] }>((resolve, reject) => {
        signal.addEventListener('abort', () => reject(new DOMException('Aborted', 'AbortError')))
        if (q === 'ab') window.setTimeout(() => resolve({ options: [opt('late', 'Stale result')] }), 600)
        else resolve({ options: [opt('x', `Result for ${q || 'nothing'}`)] })
      })
    }
    render(<Combobox id="p" label="Pick" search={search} onSelect={() => {}} />)
    fireEvent.focus(input())
    // The empty query loads straight away.
    await screen.findByRole('option', { name: /Result for nothing/ })
    expect(calls.map((c) => c.q)).toEqual([''])
    fireEvent.change(input(), { target: { value: 'a' } })
    fireEvent.change(input(), { target: { value: 'ab' } })
    expect(screen.getByRole('status').textContent).toContain('Searching')
    await waitFor(() => expect(calls.map((c) => c.q)).toEqual(['', 'ab']))
    fireEvent.change(input(), { target: { value: 'abc' } })
    await waitFor(() => expect(calls.map((c) => c.q)).toEqual(['', 'ab', 'abc']))
    expect(calls[1]!.signal.aborted).toBe(true)
    await screen.findByRole('option', { name: /Result for abc/ })
    await new Promise((r) => setTimeout(r, 700))
    expect(screen.queryByText('Stale result')).toBeNull()
  })

  it('shows no results, then an error with a working retry', async () => {
    let fail = true
    const search = vi.fn(async (q: string) => {
      if (fail) throw new Error('boom')
      return { options: q === 'none' ? [] : [opt('ok', 'Recovered')] }
    })
    render(<Combobox id="p" label="Pick" search={search} onSelect={() => {}} />)
    fireEvent.focus(input())
    expect((await screen.findByRole('alert')).textContent).toContain('Could not search')
    fail = false
    fireEvent.click(screen.getByRole('button', { name: 'Try again' }))
    await screen.findByRole('option', { name: /Recovered/ })
    fireEvent.change(input(), { target: { value: 'none' } })
    await screen.findByText('No matches')
  })

  it('opens a pasted ID without waiting for the server', async () => {
    const onSelect = vi.fn()
    render(<Combobox id="p" label="Pick" search={async () => ({ options: [] })} parseId={idOnly} onSelect={onSelect} />)
    fireEvent.focus(input())
    fireEvent.change(input(), { target: { value: 'id_7' } })
    fireEvent.keyDown(input(), { key: 'Enter' })
    expect(onSelect).toHaveBeenCalledWith('id_7', expect.anything())
  })
})

describe('Combobox (older pages)', () => {
  const page = (n: number): ComboResult => ({
    options: n === 0 ? [] : [opt(`run-${n}`, `Older run ${n}`)],
    more: n < 2,
    moreLabel: 'Search older runs',
    emptyMoreText: 'No matches in the newest runs searched',
    ...(n < 2 ? { loadMore: async () => page(n + 1) } : {}),
  })

  it('says nothing matched yet when a page is empty but more exist, and reaches the next page by keyboard', async () => {
    const onSelect = vi.fn()
    render(<Combobox id="p" label="Pick" search={async () => page(0)} onSelect={onSelect} />)
    fireEvent.focus(input())
    await screen.findByText('No matches in the newest runs searched')
    expect(screen.queryByText('No matches')).toBeNull()
    expect(screen.queryByText(/Keep typing/)).toBeNull()
    fireEvent.keyDown(input(), { key: 'ArrowDown' })
    expect(screen.getByRole('option', { name: 'Search older runs' }).getAttribute('aria-selected')).toBe('true')
    fireEvent.keyDown(input(), { key: 'Enter' })
    await screen.findByRole('option', { name: /Older run 1/ })
    // The popup stays open and the action remains while pages remain; the next page appends.
    expect(input().getAttribute('aria-expanded')).toBe('true')
    fireEvent.click(screen.getByRole('option', { name: 'Search older runs' }))
    await screen.findByRole('option', { name: /Older run 2/ })
    expect(screen.getAllByRole('option').map((o) => o.textContent)).toEqual(expect.arrayContaining([expect.stringContaining('Older run 1'), expect.stringContaining('Older run 2')]))
    expect(screen.queryByRole('option', { name: 'Search older runs' })).toBeNull()
    expect(onSelect).not.toHaveBeenCalled()
  })
})

describe('Combobox (focus, edits and loose IDs)', () => {
  it('keeps the popup open while Tab moves to the footer action, which Enter activates, and closes when focus leaves', async () => {
    const user = userEvent.setup()
    const onMore = vi.fn()
    render(
      <>
        <Combobox id="p" label="Pick" options={OPTIONS} footer={<button type="button" onClick={onMore}>Load more Changes</button>} onSelect={() => {}} />
        <button type="button">after</button>
      </>,
    )
    await user.click(input())
    await user.tab()
    expect(document.activeElement).toBe(screen.getByRole('button', { name: 'Load more Changes' }))
    expect(input().getAttribute('aria-expanded')).toBe('true')
    await user.keyboard('{Enter}')
    expect(onMore).toHaveBeenCalledTimes(1)
    await user.keyboard('{Escape}')
    expect(input().getAttribute('aria-expanded')).toBe('false')
    expect(document.activeElement).toBe(input())
    await user.tab()
    expect(document.activeElement).toBe(screen.getByRole('button', { name: 'after' }))
    expect(input().getAttribute('aria-expanded')).toBe('false')
  })

  it('makes the retry button reachable with Tab', async () => {
    const user = userEvent.setup()
    render(<Combobox id="p" label="Pick" search={async () => { throw new Error('x') }} onSelect={() => {}} />)
    await user.click(input())
    await screen.findByRole('alert')
    await user.tab()
    expect(document.activeElement).toBe(screen.getByRole('button', { name: 'Try again' }))
    expect(input().getAttribute('aria-expanded')).toBe('true')
  })

  it('calls onTextChange for edits but not for picks', () => {
    const onTextChange = vi.fn()
    render(<Combobox id="p" label="Pick" options={OPTIONS} keepText onTextChange={onTextChange} onSelect={() => {}} />)
    fireEvent.focus(input())
    fireEvent.click(screen.getByRole('option', { name: /Gamma/ }))
    expect(onTextChange).not.toHaveBeenCalled()
    fireEvent.change(input(), { target: { value: 'Gamm' } })
    expect(onTextChange).toHaveBeenCalledWith('Gamm')
  })

  it('looseId: "Open <text>" goes last and Enter does not pick it while results exist', () => {
    const onSelect = vi.fn()
    render(<Combobox id="p" label="Pick" options={[opt('review-1', 'review gate')]} looseId parseId={(t) => (/^[a-z-]+$/.test(t) ? t : null)} onSelect={onSelect} />)
    fireEvent.focus(input())
    fireEvent.change(input(), { target: { value: 'review' } })
    const opts = screen.getAllByRole('option')
    expect(opts[0]!.textContent).toContain('review gate')
    expect(opts.at(-1)!.textContent).toBe('Open review')
    expect(opts.every((o) => o.getAttribute('aria-selected') === 'false')).toBe(true)
    fireEvent.keyDown(input(), { key: 'Enter' })
    expect(onSelect).not.toHaveBeenCalled()
    fireEvent.keyDown(input(), { key: 'ArrowDown' })
    fireEvent.keyDown(input(), { key: 'Enter' })
    expect(onSelect).toHaveBeenCalledWith('review-1', expect.anything())
  })

  it('looseId: with no results Enter opens the typed ID', () => {
    const onSelect = vi.fn()
    render(<Combobox id="p" label="Pick" options={OPTIONS} looseId parseId={(t) => t} onSelect={onSelect} />)
    fireEvent.focus(input())
    fireEvent.change(input(), { target: { value: 'zzz-9' } })
    fireEvent.keyDown(input(), { key: 'Enter' })
    expect(onSelect).toHaveBeenCalledWith('zzz-9', expect.anything())
  })
})
